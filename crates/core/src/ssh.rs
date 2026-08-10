use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::client::{self, Handle};
use russh::keys::ssh_key::{self, HashAlg};
use russh::keys::{load_secret_key, PrivateKeyWithHashAlg};
use tokio::net::{TcpListener, TcpStream};

use crate::error::Error;
use crate::tunnels::{SshAuth, TunnelProfile};

pub struct TunnelTarget {
  pub host: String,
  pub port: u16,
}

/// A live local forward: TCP on `127.0.0.1:{local_port}` is piped through the
/// SSH session to the target. Dropping it tears everything down.
pub struct SshTunnel {
  pub local_port: u16,
  task: tokio::task::JoinHandle<()>,
}

impl Drop for SshTunnel {
  fn drop(&mut self) {
    self.task.abort();
  }
}

impl std::fmt::Debug for SshTunnel {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("SshTunnel")
      .field("local_port", &self.local_port)
      .finish_non_exhaustive()
  }
}

impl SshTunnel {
  pub async fn open(
    tunnel: &TunnelProfile,
    secret: Option<&str>,
    known_key: Option<ssh_key::PublicKey>,
    target: TunnelTarget,
  ) -> Result<SshTunnel, Error> {
    let session = connect_session(tunnel, secret, known_key.as_ref()).await?;
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let local_port = listener.local_addr()?.port();

    let tunnel = tunnel.clone();
    let secret = secret.map(str::to_string);
    let task = tokio::spawn(accept_loop(
      listener, session, tunnel, secret, known_key, target,
    ));
    Ok(SshTunnel { local_port, task })
  }
}

async fn accept_loop(
  listener: TcpListener,
  mut session: Handle<TunnelHandler>,
  tunnel: TunnelProfile,
  secret: Option<String>,
  known_key: Option<ssh_key::PublicKey>,
  target: TunnelTarget,
) {
  loop {
    let Ok((tcp, peer)) = listener.accept().await else {
      break;
    };
    let channel = match open_channel(&session, &target, peer.port()).await {
      Ok(channel) => channel,
      // One reconnect attempt when the session died (network blip, sshd restart).
      Err(_) if session.is_closed() => {
        match connect_session(&tunnel, secret.as_deref(), known_key.as_ref()).await {
          Ok(new_session) => {
            session = new_session;
            match open_channel(&session, &target, peer.port()).await {
              Ok(channel) => channel,
              Err(err) => {
                log::warn!("ssh channel open failed after reconnect: {err}");
                continue;
              }
            }
          }
          Err(err) => {
            log::warn!("ssh tunnel reconnect failed: {err}");
            continue;
          }
        }
      }
      Err(err) => {
        log::warn!("ssh channel open failed: {err}");
        continue;
      }
    };
    tokio::spawn(pipe(tcp, channel));
  }
}

async fn open_channel(
  session: &Handle<TunnelHandler>,
  target: &TunnelTarget,
  originator_port: u16,
) -> Result<russh::Channel<client::Msg>, russh::Error> {
  session
    .channel_open_direct_tcpip(
      target.host.clone(),
      u32::from(target.port),
      "127.0.0.1",
      u32::from(originator_port),
    )
    .await
}

async fn pipe(mut tcp: TcpStream, channel: russh::Channel<client::Msg>) {
  let mut stream = channel.into_stream();
  let _ = tokio::io::copy_bidirectional(&mut tcp, &mut stream).await;
}

async fn connect_session(
  tunnel: &TunnelProfile,
  secret: Option<&str>,
  known_key: Option<&ssh_key::PublicKey>,
) -> Result<Handle<TunnelHandler>, Error> {
  let config = Arc::new(client::Config {
    keepalive_interval: Some(Duration::from_secs(15)),
    keepalive_max: 4,
    ..Default::default()
  });
  let seen = Arc::new(Mutex::new(None));
  let handler = TunnelHandler {
    known: known_key.cloned(),
    seen: seen.clone(),
  };
  let mut session = client::connect(config, (tunnel.host.as_str(), tunnel.port), handler)
    .await
    .map_err(|err| connect_error(err, tunnel, known_key.is_some(), &seen))?;
  authenticate(&mut session, tunnel, secret).await?;
  Ok(session)
}

fn connect_error(
  err: russh::Error,
  tunnel: &TunnelProfile,
  previously_trusted: bool,
  seen: &Arc<Mutex<Option<ssh_key::PublicKey>>>,
) -> Error {
  if matches!(err, russh::Error::UnknownKey) {
    if let Some(seen) = seen.lock().unwrap().take() {
      let fingerprint = seen.fingerprint(HashAlg::Sha256).to_string();
      return Error::HostKeyUntrusted {
        message: format!(
          "host key for {}:{} is not trusted ({fingerprint})",
          tunnel.host, tunnel.port
        ),
        host: tunnel.host.clone(),
        port: tunnel.port,
        fingerprint,
        key: seen.to_openssh().unwrap_or_default(),
        previously_trusted,
      };
    }
  }
  Error::Tunnel {
    message: format!("ssh {}:{}: {err}", tunnel.host, tunnel.port),
  }
}

async fn authenticate(
  session: &mut Handle<TunnelHandler>,
  tunnel: &TunnelProfile,
  secret: Option<&str>,
) -> Result<(), Error> {
  let result = match &tunnel.auth {
    SshAuth::Password => session
      .authenticate_password(tunnel.user.clone(), secret.unwrap_or_default())
      .await
      .map_err(|err| tunnel_error(tunnel, err))?,
    SshAuth::KeyFile { path } => {
      let key = load_secret_key(expand_tilde(path), secret).map_err(|err| Error::Tunnel {
        message: format!("ssh key {path}: {err}"),
      })?;
      let hash = rsa_hash(session, tunnel).await?;
      session
        .authenticate_publickey(
          tunnel.user.clone(),
          PrivateKeyWithHashAlg::new(Arc::new(key), hash),
        )
        .await
        .map_err(|err| tunnel_error(tunnel, err))?
    }
    SshAuth::Agent => authenticate_with_agent(session, tunnel).await?,
    SshAuth::None => session
      .authenticate_none(tunnel.user.clone())
      .await
      .map_err(|err| tunnel_error(tunnel, err))?,
  };
  match result {
    client::AuthResult::Success => Ok(()),
    // The offered methods turn a mismatch (wrong port, wrong method) into a
    // self-explanatory failure.
    client::AuthResult::Failure {
      remaining_methods, ..
    } => Err(Error::Tunnel {
      message: format!(
        "ssh authentication failed for {}@{}:{} (server accepts: {})",
        tunnel.user,
        tunnel.host,
        tunnel.port,
        method_names(&remaining_methods)
      ),
    }),
  }
}

fn method_names(methods: &russh::MethodSet) -> String {
  if methods.is_empty() {
    return "nothing advertised".to_string();
  }
  methods
    .iter()
    .map(<&str>::from)
    .collect::<Vec<_>>()
    .join(", ")
}

#[cfg(unix)]
async fn authenticate_with_agent(
  session: &mut Handle<TunnelHandler>,
  tunnel: &TunnelProfile,
) -> Result<client::AuthResult, Error> {
  use russh::keys::agent::client::AgentClient;

  let agent_error = |err: String| Error::Tunnel {
    message: format!("ssh-agent: {err}"),
  };
  let sock = std::env::var("SSH_AUTH_SOCK")
    .map_err(|_| agent_error("SSH_AUTH_SOCK is not set".to_string()))?;
  let stream = tokio::net::UnixStream::connect(&sock)
    .await
    .map_err(|err| agent_error(err.to_string()))?;
  let mut agent = AgentClient::connect(stream);
  let identities = agent
    .request_identities()
    .await
    .map_err(|err| agent_error(err.to_string()))?;
  if identities.is_empty() {
    return Err(agent_error("no identities loaded".to_string()));
  }
  let hash = rsa_hash(session, tunnel).await?;
  let mut last = None;
  for identity in identities {
    let result = session
      .authenticate_publickey_with(
        tunnel.user.clone(),
        identity.public_key().into_owned(),
        hash,
        &mut agent,
      )
      .await
      .map_err(|err| agent_error(err.to_string()))?;
    if result.success() {
      return Ok(result);
    }
    last = Some(result);
  }
  Ok(last.expect("identities are non-empty"))
}

#[cfg(not(unix))]
async fn authenticate_with_agent(
  _session: &mut Handle<TunnelHandler>,
  _tunnel: &TunnelProfile,
) -> Result<client::AuthResult, Error> {
  Err(Error::Tunnel {
    message: "ssh-agent auth is not supported on this platform yet".to_string(),
  })
}

async fn rsa_hash(
  session: &mut Handle<TunnelHandler>,
  tunnel: &TunnelProfile,
) -> Result<Option<HashAlg>, Error> {
  Ok(
    session
      .best_supported_rsa_hash()
      .await
      .map_err(|err| tunnel_error(tunnel, err))?
      .flatten(),
  )
}

fn tunnel_error(tunnel: &TunnelProfile, err: russh::Error) -> Error {
  Error::Tunnel {
    message: format!("ssh {}:{}: {err}", tunnel.host, tunnel.port),
  }
}

pub fn parse_public_key(raw: &str) -> Result<ssh_key::PublicKey, Error> {
  ssh_key::PublicKey::from_openssh(raw).map_err(|err| Error::Tunnel {
    message: format!("stored host key: {err}"),
  })
}

// OpenSSH's own default identity order.
const DEFAULT_KEY_NAMES: [&str; 5] = [
  "id_ed25519",
  "id_ecdsa",
  "id_ecdsa_sk",
  "id_ed25519_sk",
  "id_rsa",
];

/// Default identity files that actually exist, tilde-form for display.
pub fn default_key_paths() -> Vec<String> {
  match std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
    Some(home) => key_paths_in(std::path::Path::new(&home)),
    None => Vec::new(),
  }
}

fn key_paths_in(home: &std::path::Path) -> Vec<String> {
  DEFAULT_KEY_NAMES
    .iter()
    .filter(|name| home.join(".ssh").join(name).is_file())
    .map(|name| format!("~/.ssh/{name}"))
    .collect()
}

fn expand_tilde(path: &str) -> PathBuf {
  if let Some(rest) = path.strip_prefix("~/") {
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
      return PathBuf::from(home).join(rest);
    }
  }
  PathBuf::from(path)
}

struct TunnelHandler {
  known: Option<ssh_key::PublicKey>,
  seen: Arc<Mutex<Option<ssh_key::PublicKey>>>,
}

impl client::Handler for TunnelHandler {
  type Error = russh::Error;

  async fn check_server_key(&mut self, key: &ssh_key::PublicKey) -> Result<bool, Self::Error> {
    *self.seen.lock().unwrap() = Some(key.clone());
    Ok(self.known.as_ref() == Some(key))
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::credentials::Credentials;
  use tokio_postgres::NoTls;

  const TEST_USER: &str = "tunnel";
  // The DB target as seen from the sshd container's network.
  const TEST_TARGET: (&str, u16) = ("postgres", 5432);
  const TEST_KEY: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../scripts/test-ssh/id_ed25519"
  );
  const TEST_KEY_PP: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../scripts/test-ssh/id_ed25519_pp"
  );
  const TEST_KEY_PP_PASSPHRASE: &str = "soquel-test";

  #[test]
  fn default_keys_list_only_existing_files_in_openssh_order() {
    let home = tempfile::tempdir().unwrap();
    let ssh = home.path().join(".ssh");
    std::fs::create_dir_all(&ssh).unwrap();
    assert!(key_paths_in(home.path()).is_empty());

    std::fs::write(ssh.join("id_rsa"), "x").unwrap();
    std::fs::write(ssh.join("id_ed25519"), "x").unwrap();
    std::fs::create_dir(ssh.join("id_ecdsa")).unwrap();
    assert_eq!(
      key_paths_in(home.path()),
      vec!["~/.ssh/id_ed25519", "~/.ssh/id_rsa"]
    );
  }

  fn tunnel_from_env(key_path: &str) -> Option<TunnelProfile> {
    let addr = std::env::var("SOQUEL_TEST_SSH").ok()?;
    let (host, port) = addr.split_once(':').expect("SOQUEL_TEST_SSH is host:port");
    Some(TunnelProfile {
      id: "test".to_string(),
      name: "test".to_string(),
      host: host.to_string(),
      port: port.parse().unwrap(),
      user: TEST_USER.to_string(),
      auth: SshAuth::KeyFile {
        path: key_path.to_string(),
      },
      credential: Default::default(),
    })
  }

  async fn host_key(tunnel: &TunnelProfile) -> ssh_key::PublicKey {
    let err = SshTunnel::open(
      tunnel,
      None,
      None,
      TunnelTarget {
        host: TEST_TARGET.0.to_string(),
        port: TEST_TARGET.1,
      },
    )
    .await
    .expect_err("first contact must be untrusted");
    let Error::HostKeyUntrusted {
      key,
      previously_trusted: false,
      ..
    } = err
    else {
      panic!("expected an untrusted host key error, got {err:?}");
    };
    parse_public_key(&key).unwrap()
  }

  async fn try_select_through(tunnel: &SshTunnel) -> Result<(), tokio_postgres::Error> {
    let url = format!(
      "host=127.0.0.1 port={} user=soquel password=soquel dbname=soquel_test",
      tunnel.local_port
    );
    let (client, connection) = tokio_postgres::connect(&url, NoTls).await?;
    tokio::spawn(connection);
    client.simple_query("SELECT 1").await.map(|_| ())
  }

  async fn select_one_through(tunnel: &SshTunnel) {
    try_select_through(tunnel).await.unwrap();
  }

  #[tokio::test(flavor = "multi_thread")]
  async fn integration_ssh_tofu_then_forwards_postgres() {
    let Some(profile) = tunnel_from_env(TEST_KEY) else {
      return;
    };
    let key = host_key(&profile).await;
    let tunnel = SshTunnel::open(
      &profile,
      None,
      Some(key),
      TunnelTarget {
        host: TEST_TARGET.0.to_string(),
        port: TEST_TARGET.1,
      },
    )
    .await
    .unwrap();
    select_one_through(&tunnel).await;
  }

  #[tokio::test(flavor = "multi_thread")]
  async fn integration_ssh_passphrase_key_auth() {
    let Some(profile) = tunnel_from_env(TEST_KEY_PP) else {
      return;
    };
    let key = host_key(&profile).await;
    let tunnel = SshTunnel::open(
      &profile,
      Some(TEST_KEY_PP_PASSPHRASE),
      Some(key),
      TunnelTarget {
        host: TEST_TARGET.0.to_string(),
        port: TEST_TARGET.1,
      },
    )
    .await
    .unwrap();
    select_one_through(&tunnel).await;
  }

  #[tokio::test(flavor = "multi_thread")]
  async fn integration_ssh_password_auth() {
    let Some(mut profile) = tunnel_from_env(TEST_KEY) else {
      return;
    };
    profile.user = "tunnelpw".to_string();
    profile.auth = SshAuth::Password;
    let key = host_key(&profile).await;
    let tunnel = SshTunnel::open(
      &profile,
      Some("soquel-pw"),
      Some(key),
      TunnelTarget {
        host: TEST_TARGET.0.to_string(),
        port: TEST_TARGET.1,
      },
    )
    .await
    .unwrap();
    select_one_through(&tunnel).await;
  }

  /// The bastion's password comes out of a command, like an RDS IAM token.
  #[tokio::test(flavor = "multi_thread")]
  async fn integration_ssh_password_from_a_command() {
    use crate::credentials::{resolve_credentials, CredentialTarget};
    use crate::profiles::CredentialSource;
    use crate::secrets::InMemoryStore;
    use crate::AppState;

    let Some(mut profile) = tunnel_from_env(TEST_KEY) else {
      return;
    };
    profile.user = "tunnelpw".to_string();
    profile.auth = SshAuth::Password;
    profile.credential = CredentialSource::Command {
      command: "printf %s soquel-pw".to_string(),
      refresh_after_secs: None,
    };
    let key = host_key(&profile).await;

    let dir = tempfile::tempdir().unwrap();
    let state = AppState::for_tests(dir.path(), Box::new(InMemoryStore::default()));
    // Saving the tunnel in the form is what approves its command.
    let CredentialSource::Command { command, .. } = &profile.credential else {
      unreachable!("the profile above is in command mode");
    };
    state
      .command_approvals
      .lock()
      .unwrap()
      .approve(
        &crate::secrets::SecretKey::Tunnel("t-1".to_string()),
        command,
      )
      .unwrap();

    let secret = resolve_credentials(&state, &CredentialTarget::tunnel(&profile, "t-1"), None)
      .unwrap()
      .resolve()
      .await
      .unwrap();
    assert_eq!(secret.as_deref(), Some("soquel-pw"));

    let tunnel = SshTunnel::open(
      &profile,
      secret.as_deref(),
      Some(key),
      TunnelTarget {
        host: TEST_TARGET.0.to_string(),
        port: TEST_TARGET.1,
      },
    )
    .await
    .unwrap();
    select_one_through(&tunnel).await;
  }

  /// TLS through the tunnel: the config keeps the logical host for SNI while
  /// TCP goes through the local forward.
  #[tokio::test(flavor = "multi_thread")]
  async fn integration_ssh_tls_require_through_tunnel() {
    use crate::connectors::{Connector, LocalForward};
    use crate::postgres::PostgresConnector;
    use crate::profiles::{ConnectionProfile, ConnectorParams, Env, SqlServerParams, SslMode};

    let Some(profile) = tunnel_from_env(TEST_KEY) else {
      return;
    };
    let key = host_key(&profile).await;
    let tunnel = SshTunnel::open(
      &profile,
      None,
      Some(key),
      TunnelTarget {
        host: "postgres-tls".to_string(),
        port: 5432,
      },
    )
    .await
    .unwrap();

    let connection = PostgresConnector
      .connect(
        &ConnectionProfile {
          id: String::new(),
          name: String::new(),
          env: Env::Dev,
          group: None,
          agent_access: Default::default(),
          credential: Default::default(),
          params: ConnectorParams::Postgres(SqlServerParams {
            host: "postgres-tls".to_string(),
            port: 5432,
            database: "soquel_test".to_string(),
            user: "soquel".to_string(),
            ssl_mode: SslMode::Require,
            ssl_root_cert: None,
            tunnel_id: None,
          }),
        },
        Credentials::fixed(Some("soquel".to_string())),
        Some(LocalForward {
          port: tunnel.local_port,
        }),
      )
      .await
      .unwrap();
    connection.health().await.unwrap();
    connection.close().await.unwrap();
  }

  /// The kv connector rides the same LocalForward plumbing as the sql ones.
  #[tokio::test(flavor = "multi_thread")]
  async fn integration_ssh_redis_through_tunnel() {
    use crate::connectors::{Connector, LocalForward};
    use crate::profiles::{ConnectionProfile, ConnectorParams, Env, RedisParams};
    use crate::redis::RedisConnector;

    let Some(profile) = tunnel_from_env(TEST_KEY) else {
      return;
    };
    if std::env::var("SOQUEL_TEST_REDIS").is_err() {
      return;
    }
    let key = host_key(&profile).await;
    let tunnel = SshTunnel::open(
      &profile,
      None,
      Some(key),
      TunnelTarget {
        host: "redis".to_string(),
        port: 6379,
      },
    )
    .await
    .unwrap();

    let connection = RedisConnector
      .connect(
        &ConnectionProfile {
          id: String::new(),
          name: String::new(),
          env: Env::Dev,
          group: None,
          agent_access: Default::default(),
          credential: Default::default(),
          params: ConnectorParams::Redis(RedisParams {
            host: "redis".to_string(),
            port: 6379,
            db: 0,
            username: None,
            tls: false,
            tunnel_id: None,
          }),
        },
        Credentials::fixed(Some("soquel".to_string())),
        Some(LocalForward {
          port: tunnel.local_port,
        }),
      )
      .await
      .unwrap();
    connection.health().await.unwrap();
    let kv = connection.kv().unwrap();
    kv.scan_keys("*", None, 10).await.unwrap();
    connection.close().await.unwrap();
  }

  /// The command-layer db switch: a second dial through the already-open
  /// forward, swapping the entry's connection without disturbing the tunnel.
  #[tokio::test(flavor = "multi_thread")]
  async fn integration_ssh_redis_select_db_through_tunnel() {
    use std::collections::HashMap;

    use crate::connectors::{Connection, Connector, LocalForward};
    use crate::known_hosts::KnownHostsStore;
    use crate::profiles::{ConnectionInput, ConnectorParams, Env, ProfileStore, RedisParams};
    use crate::redis::RedisConnector;
    use crate::secrets::{InMemoryStore, SecretStore};
    use crate::tunnels::TunnelStore;
    use crate::{ActiveConnection, AppState};

    let Some(tunnel_profile) = tunnel_from_env(TEST_KEY) else {
      return;
    };
    if std::env::var("SOQUEL_TEST_REDIS").is_err() {
      return;
    }
    let key = host_key(&tunnel_profile).await;
    let tunnel = SshTunnel::open(
      &tunnel_profile,
      None,
      Some(key),
      TunnelTarget {
        host: "redis".to_string(),
        port: 6379,
      },
    )
    .await
    .unwrap();

    let dir = tempfile::tempdir().unwrap();
    let mut profiles = ProfileStore::load(dir.path().join("connections.json")).unwrap();
    let profile = profiles
      .create(&ConnectionInput {
        name: "kv-swap".to_string(),
        env: Env::Dev,
        group: None,
        agent_access: Default::default(),
        credential: Default::default(),
        params: ConnectorParams::Redis(RedisParams {
          host: "redis".to_string(),
          port: 6379,
          db: 0,
          username: None,
          tls: false,
          tunnel_id: None,
        }),
        password: None,
      })
      .unwrap();
    let id = profile.id.clone();

    let connection = RedisConnector
      .connect(
        &profile,
        Credentials::fixed(Some("soquel".to_string())),
        Some(LocalForward {
          port: tunnel.local_port,
        }),
      )
      .await
      .unwrap();
    const SEEDED: &str = "soquel_test:kvswap:key";
    connection
      .kv()
      .unwrap()
      .set_string(SEEDED, "db0")
      .await
      .unwrap();

    let secrets = InMemoryStore::default();
    secrets
      .set(&crate::secrets::SecretKey::Connection(id.clone()), "soquel")
      .unwrap();
    let state = AppState {
      profiles: std::sync::Mutex::new(profiles),
      tunnels: std::sync::Mutex::new(TunnelStore::load(dir.path().join("tunnels.json")).unwrap()),
      known_hosts: std::sync::Mutex::new(
        KnownHostsStore::load(dir.path().join("known_hosts.json")).unwrap(),
      ),
      command_approvals: std::sync::Mutex::new(
        crate::command_approvals::CommandApprovalsStore::load(
          dir.path().join("command_approvals.json"),
        )
        .unwrap(),
      ),
      secrets: Box::new(secrets),
      secrets_problem: None,
      session_secrets: Default::default(),
      connections: tokio::sync::Mutex::new(HashMap::from([(
        id.clone(),
        ActiveConnection {
          connection: connection.into(),
          _tunnel: Some(tunnel),
        },
      )])),
      sessions: tokio::sync::Mutex::new(HashMap::new()),
      data_dir: dir.path().to_path_buf(),
      mcp: tokio::sync::Mutex::new(None),
      approvals: tokio::sync::Mutex::new(HashMap::new()),
      trust_windows: tokio::sync::Mutex::new(HashMap::new()),
    };
    let active =
      |connections: &HashMap<String, ActiveConnection>| -> std::sync::Arc<dyn Connection> {
        connections.get(&id).unwrap().connection.clone()
      };

    state.select_kv_db(&id, 1).await.unwrap();
    let swapped = active(&*state.connections.lock().await);
    let kv = swapped.kv().unwrap();
    assert_eq!(kv.databases().await.unwrap().current, 1);
    let miss = kv.key_detail(SEEDED).await;
    assert!(
      matches!(miss, Err(Error::NotFound { .. })),
      "seeded key must not exist in db 1, got {miss:?}"
    );

    state.select_kv_db(&id, 0).await.unwrap();
    let back = active(&*state.connections.lock().await);
    let kv = back.kv().unwrap();
    assert_eq!(kv.databases().await.unwrap().current, 0);
    assert_eq!(kv.key_detail(SEEDED).await.unwrap().key, SEEDED);

    kv.delete_key(SEEDED).await.unwrap();
    back.close().await.unwrap();
  }

  /// The doc connector rides the same LocalForward plumbing. Also proves the
  /// direct connection never redials the advertised address (the container
  /// hostname is unreachable from the host).
  #[tokio::test(flavor = "multi_thread")]
  async fn integration_ssh_mongo_through_tunnel() {
    use crate::connectors::{Connector, LocalForward};
    use crate::mongo::MongoConnector;
    use crate::profiles::{ConnectionProfile, ConnectorParams, Env, MongoParams};

    let Some(profile) = tunnel_from_env(TEST_KEY) else {
      return;
    };
    if std::env::var("SOQUEL_TEST_MONGO").is_err() {
      return;
    }
    let key = host_key(&profile).await;
    let tunnel = SshTunnel::open(
      &profile,
      None,
      Some(key),
      TunnelTarget {
        host: "mongo".to_string(),
        port: 27017,
      },
    )
    .await
    .unwrap();

    let connection = MongoConnector
      .connect(
        &ConnectionProfile {
          id: String::new(),
          name: String::new(),
          env: Env::Dev,
          group: None,
          agent_access: Default::default(),
          credential: Default::default(),
          params: ConnectorParams::Mongo(MongoParams {
            host: "mongo".to_string(),
            port: 27017,
            database: None,
            username: Some("soquel".to_string()),
            auth_source: Some("admin".to_string()),
            tls: false,
            tunnel_id: None,
          }),
        },
        Credentials::fixed(Some("soquel".to_string())),
        Some(LocalForward {
          port: tunnel.local_port,
        }),
      )
      .await
      .unwrap();
    connection.health().await.unwrap();
    let databases = connection.doc().unwrap().databases().await.unwrap();
    assert!(
      databases.iter().any(|db| db.name == "admin"),
      "{databases:?}"
    );
    connection.close().await.unwrap();
  }

  /// End-to-end optimistic proof of the SNI/hostname override: verify-full
  /// through the tunnel validates the profile's logical host, not 127.0.0.1.
  #[tokio::test(flavor = "multi_thread")]
  async fn integration_ssh_verify_full_through_tunnel_checks_logical_host() {
    use crate::connectors::{Connector, LocalForward};
    use crate::postgres::tests::TEST_ROOT_CERT;
    use crate::postgres::PostgresConnector;
    use crate::profiles::{ConnectionProfile, ConnectorParams, Env, SqlServerParams, SslMode};

    let Some(profile) = tunnel_from_env(TEST_KEY) else {
      return;
    };
    if std::env::var("SOQUEL_TEST_PG_TLS").is_err() {
      return;
    }
    let key = host_key(&profile).await;
    let tunnel = SshTunnel::open(
      &profile,
      None,
      Some(key),
      TunnelTarget {
        host: "postgres-tls".to_string(),
        port: 5432,
      },
    )
    .await
    .unwrap();

    let connection_profile = |host: &str| ConnectionProfile {
      id: String::new(),
      name: String::new(),
      env: Env::Dev,
      group: None,
      agent_access: Default::default(),
      credential: Default::default(),
      params: ConnectorParams::Postgres(SqlServerParams {
        host: host.to_string(),
        port: 5432,
        database: "soquel_test".to_string(),
        user: "soquel".to_string(),
        ssl_mode: SslMode::VerifyFull,
        ssl_root_cert: Some(TEST_ROOT_CERT.to_string()),
        tunnel_id: None,
      }),
    };
    let forward = LocalForward {
      port: tunnel.local_port,
    };

    // "localhost" is in the cert SAN: the handshake must pass through the tunnel.
    let connection = PostgresConnector
      .connect(
        &connection_profile("localhost"),
        Credentials::fixed(Some("soquel".to_string())),
        Some(forward),
      )
      .await
      .unwrap();
    connection.health().await.unwrap();
    connection.close().await.unwrap();

    // A logical host missing from the SAN must be rejected, proving the
    // verification targets the profile host rather than the forward address.
    let rejected = PostgresConnector
      .connect(
        &connection_profile("postgres-tls"),
        Credentials::fixed(Some("soquel".to_string())),
        Some(forward),
      )
      .await;
    let Err(Error::Database { message }) = rejected else {
      panic!("expected a hostname mismatch");
    };
    assert!(!message.is_empty());
  }

  #[tokio::test(flavor = "multi_thread")]
  async fn integration_ssh_reconnects_after_sshd_restart() {
    let Ok(addr) = std::env::var("SOQUEL_TEST_SSH_RECONNECT") else {
      return;
    };
    let (host, port) = addr.split_once(':').expect("host:port");
    let profile = TunnelProfile {
      id: "test".to_string(),
      name: "test".to_string(),
      host: host.to_string(),
      port: port.parse().unwrap(),
      user: TEST_USER.to_string(),
      auth: SshAuth::KeyFile {
        path: TEST_KEY.to_string(),
      },
      credential: Default::default(),
    };
    let key = host_key(&profile).await;
    let tunnel = SshTunnel::open(
      &profile,
      None,
      Some(key),
      TunnelTarget {
        host: TEST_TARGET.0.to_string(),
        port: TEST_TARGET.1,
      },
    )
    .await
    .unwrap();
    select_one_through(&tunnel).await;

    // Kill the SSH session server-side; the container (and its host key) survives.
    let compose = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docker-compose.test.yml");
    let status = std::process::Command::new("docker")
      .args(["compose", "-f", compose, "restart", "sshd-reconnect"])
      .status()
      .expect("docker compose restart");
    assert!(status.success());

    // The dead session is only noticed at the next channel open: retry until
    // the accept loop's reconnect-once path kicks in.
    let mut recovered = false;
    for _ in 0..30 {
      tokio::time::sleep(Duration::from_millis(500)).await;
      if try_select_through(&tunnel).await.is_ok() {
        recovered = true;
        break;
      }
    }
    assert!(recovered, "tunnel never recovered after sshd restart");
  }

  #[tokio::test(flavor = "multi_thread")]
  async fn integration_ssh_mismatched_host_key_is_flagged() {
    let Some(profile) = tunnel_from_env(TEST_KEY) else {
      return;
    };
    // A valid key that is not the server's: the other test key's public half.
    let wrong = std::fs::read_to_string(format!("{TEST_KEY_PP}.pub")).unwrap();
    let wrong = parse_public_key(&wrong).unwrap();
    let err = SshTunnel::open(
      &profile,
      None,
      Some(wrong),
      TunnelTarget {
        host: TEST_TARGET.0.to_string(),
        port: TEST_TARGET.1,
      },
    )
    .await
    .expect_err("mismatched key must fail");
    let Error::HostKeyUntrusted {
      previously_trusted: true,
      fingerprint,
      ..
    } = err
    else {
      panic!("expected a mismatch error, got {err:?}");
    };
    assert!(fingerprint.starts_with("SHA256:"));
  }

  #[tokio::test(flavor = "multi_thread")]
  async fn integration_ssh_none_auth_fails_on_key_only_server() {
    let Some(mut profile) = tunnel_from_env(TEST_KEY) else {
      return;
    };
    let key = host_key(&profile).await;
    profile.auth = SshAuth::None;
    let err = SshTunnel::open(
      &profile,
      None,
      Some(key),
      TunnelTarget {
        host: TEST_TARGET.0.to_string(),
        port: TEST_TARGET.1,
      },
    )
    .await
    .expect_err("none auth against key-only sshd must fail");
    let Error::Tunnel { message } = err else {
      panic!("expected a tunnel error, got {err:?}");
    };
    assert!(message.contains("authentication failed"), "{message}");
  }

  #[tokio::test(flavor = "multi_thread")]
  async fn integration_ssh_wrong_key_auth_fails_cleanly() {
    let Some(mut profile) = tunnel_from_env(TEST_KEY_PP) else {
      return;
    };
    profile.auth = SshAuth::Password;
    let key = host_key(&profile).await;
    let err = SshTunnel::open(
      &profile,
      Some("wrong-password"),
      Some(key),
      TunnelTarget {
        host: TEST_TARGET.0.to_string(),
        port: TEST_TARGET.1,
      },
    )
    .await
    .expect_err("password auth against key-only sshd must fail");
    let Error::Tunnel { message } = err else {
      panic!("expected a tunnel error, got {err:?}");
    };
    assert!(message.contains("authentication failed"), "{message}");
  }
}
