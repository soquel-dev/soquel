//! Connection lifecycle shared by every frontend: the command layer and the
//! gpui app both call these, so the flows cannot drift apart.

use std::sync::Arc;

use crate::connectors::{connector_for, Connection, LocalForward};
use crate::credentials::{resolve_credentials, CredentialTarget};
use crate::error::{Error, SecretSubject};
use crate::profiles::{ConnectionInput, ConnectionProfile, CredentialSource};
use crate::secrets::SecretKey;
use crate::ssh::{self, SshTunnel, TunnelTarget};
use crate::{ActiveConnection, AppState};

/// Resolve a profile's tunnel (if any); the returned forward tells the
/// connector where TCP actually goes while the profile keeps the logical host.
pub async fn open_tunnel(
  state: &AppState,
  profile: &ConnectionProfile,
) -> Result<Option<(SshTunnel, LocalForward)>, Error> {
  let Some(remote) = profile.params.remote() else {
    return Ok(None);
  };
  let Some(tunnel_id) = remote.tunnel_id else {
    return Ok(None);
  };
  let tunnel = state.tunnels.lock().unwrap().get(tunnel_id)?;
  // Opened once per connection: no pool to re-resolve for, so a command runs
  // at each bring-up, which is what a short-lived token wants.
  let secret = resolve_credentials(state, &CredentialTarget::tunnel(&tunnel, tunnel_id), None)?
    .resolve()
    .await?;
  let known_key = state
    .known_hosts
    .lock()
    .unwrap()
    .get(&tunnel.host, tunnel.port)
    .map(|raw| ssh::parse_public_key(&raw))
    .transpose()?;
  let opened = SshTunnel::open(
    &tunnel,
    secret.as_deref(),
    known_key,
    TunnelTarget {
      host: remote.host.to_string(),
      port: remote.port,
    },
  )
  .await?;
  let forward = LocalForward {
    port: opened.local_port,
  };
  Ok(Some((opened, forward)))
}

pub async fn connect(state: &AppState, id: String) -> Result<(), Error> {
  let result = connect_attempt(state, &id).await;
  // A password typed for this attempt only dies with it, success or failure,
  // on both ends of the hop.
  state
    .session_secrets
    .clear_one_shot(&SecretKey::Connection(id.clone()));
  if let Some(tunnel_id) = tunnel_of(state, &id) {
    state
      .session_secrets
      .clear_one_shot(&SecretKey::Tunnel(tunnel_id));
  }
  result
}

/// The tunnel a connection rides on, if any.
pub fn tunnel_of(state: &AppState, id: &str) -> Option<String> {
  let profile = state.profiles.lock().unwrap().get(id).ok()?;
  profile
    .params
    .remote()
    .and_then(|remote| remote.tunnel_id)
    .map(str::to_string)
}

async fn connect_attempt(state: &AppState, id: &str) -> Result<(), Error> {
  let profile = state.profiles.lock().unwrap().get(id)?;
  let secret = resolve_credentials(state, &CredentialTarget::connection(&profile, id), None)?;
  let opened = open_tunnel(state, &profile).await?;
  let forward = opened.as_ref().map(|(_, f)| *f);
  let connection = connector_for(profile.params.kind())
    .connect(&profile, secret, forward)
    .await?;
  state.connections.lock().await.insert(
    id.to_string(),
    ActiveConnection {
      connection: connection.into(),
      _tunnel: opened.map(|(tunnel, _)| tunnel),
    },
  );
  Ok(())
}

pub async fn disconnect(state: &AppState, id: &str) -> Result<(), Error> {
  let orphaned: Vec<Arc<dyn crate::connectors::SqlSession>> = {
    let mut sessions = state.sessions.lock().await;
    let ids: Vec<String> = sessions
      .iter()
      .filter(|(_, entry)| entry.connection_id == id)
      .map(|(session_id, _)| session_id.clone())
      .collect();
    ids
      .iter()
      .filter_map(|session_id| sessions.remove(session_id))
      .map(|entry| entry.session)
      .collect()
  };
  for session in orphaned {
    let _ = session.close().await;
  }
  state
    .session_secrets
    .clear(&SecretKey::Connection(id.to_string()));
  // The tunnel's password only outlives the hop while another connection still
  // rides it.
  if let Some(tunnel_id) = tunnel_of(state, id) {
    let still_used = state
      .connections
      .lock()
      .await
      .keys()
      .any(|other| other != id && tunnel_of(state, other).as_deref() == Some(&tunnel_id));
    if !still_used {
      state.session_secrets.clear(&SecretKey::Tunnel(tunnel_id));
    }
  }
  let active = state.connections.lock().await.remove(id);
  match active {
    Some(active) => active.connection.close().await,
    None => Ok(()),
  }
}

/// Ephemeral connect + health check; never touches the active connections.
pub async fn test_connection(
  state: &AppState,
  input: &ConnectionInput,
  existing_id: Option<&str>,
) -> Result<(), Error> {
  let profile = ConnectionProfile {
    id: String::new(),
    name: input.name.clone(),
    env: input.env,
    group: input.group.clone(),
    agent_access: input.agent_access,
    credential: input.credential.clone(),
    params: input.params.clone(),
  };
  let target = CredentialTarget::connection(&profile, existing_id.unwrap_or_default());
  let secret = resolve_credentials(state, &target, input.password.clone())?;
  let result = test_run(state, &profile, secret).await;
  // A password typed to test an unsaved profile has no connection to outlive.
  state.session_secrets.clear_one_shot(&target.key);
  result
}

async fn test_run(
  state: &AppState,
  profile: &ConnectionProfile,
  secret: Arc<crate::credentials::Credentials>,
) -> Result<(), Error> {
  let opened = open_tunnel(state, profile).await?;
  let connection = connector_for(profile.params.kind())
    .connect(profile, secret, opened.as_ref().map(|(_, f)| *f))
    .await?;
  connection.health().await?;
  connection.close().await
}

/// Hands the core a password for whatever asked for one at connect time.
/// Memory only: `remember` keeps it until disconnect, otherwise it dies with
/// the next attempt.
pub fn unlock_secret(
  state: &AppState,
  subject: SecretSubject,
  id: String,
  secret: String,
  remember: bool,
) {
  state
    .session_secrets
    .set(&key_for(subject, id), secret, remember);
}

pub fn key_for(subject: SecretSubject, id: String) -> SecretKey {
  match subject {
    SecretSubject::Connection => SecretKey::Connection(id),
    SecretSubject::Tunnel => SecretKey::Tunnel(id),
  }
}

/// Reads the command off the stored profile: approving what the frontend
/// echoes back would approve whatever it says instead of what will run.
pub fn current_command(state: &AppState, key: &SecretKey) -> Result<String, Error> {
  let credential = match key {
    SecretKey::Connection(id) => state.profiles.lock().unwrap().get(id)?.credential,
    SecretKey::Tunnel(id) => state.tunnels.lock().unwrap().get(id)?.credential,
    SecretKey::McpToken => CredentialSource::Keychain,
  };
  match credential {
    CredentialSource::Command { command, .. } => Ok(command),
    _ => Err(Error::NotFound {
      message: "this connection does not get its password from a command".to_string(),
    }),
  }
}

/// Keeps the stores in step with the mode: only `keychain` stores a password,
/// and leaving it wipes what was stored for something that no longer keeps one.
pub fn persist_secret(
  state: &AppState,
  key: &SecretKey,
  credential: &CredentialSource,
  password: Option<&str>,
) -> Result<(), Error> {
  if credential == &CredentialSource::Keychain {
    if let Some(password) = password {
      state.secrets.set(key, password)?;
    }
    return Ok(());
  }
  state.secrets.delete(key)?;
  state.session_secrets.clear(key);
  Ok(())
}

/// A command typed in the form is approved by saving it: the user just read
/// the argv under the field. Only imports leave one waiting.
pub fn approve_own_command(
  state: &AppState,
  key: &SecretKey,
  credential: &CredentialSource,
) -> Result<(), Error> {
  let mut approvals = state.command_approvals.lock().unwrap();
  match credential {
    CredentialSource::Command { command, .. } => approvals.approve(key, command),
    _ => approvals.revoke(key),
  }
}

pub fn create_connection(
  state: &AppState,
  input: &ConnectionInput,
) -> Result<ConnectionProfile, Error> {
  let profile = state.profiles.lock().unwrap().create(input)?;
  // No orphan profile when the keychain is unavailable.
  let key = SecretKey::Connection(profile.id.clone());
  if let Err(err) = persist_secret(state, &key, &profile.credential, input.password.as_deref()) {
    let _ = state.profiles.lock().unwrap().delete(&profile.id);
    return Err(err);
  }
  approve_own_command(state, &key, &profile.credential)?;
  Ok(profile)
}

pub fn update_connection(
  state: &AppState,
  id: &str,
  input: &ConnectionInput,
) -> Result<ConnectionProfile, Error> {
  let profile = state.profiles.lock().unwrap().update(id, input)?;
  let key = SecretKey::Connection(profile.id.clone());
  persist_secret(state, &key, &profile.credential, input.password.as_deref())?;
  approve_own_command(state, &key, &profile.credential)?;
  Ok(profile)
}

pub fn delete_connection(state: &AppState, id: &str) -> Result<(), Error> {
  state.profiles.lock().unwrap().delete(id)?;
  forget(state, &SecretKey::Connection(id.to_string()))
}

pub fn forget(state: &AppState, key: &SecretKey) -> Result<(), Error> {
  state.command_approvals.lock().unwrap().revoke(key)?;
  state.session_secrets.clear(key);
  state.secrets.delete(key)
}

// Clone the Arc out so queries never hold the map lock.
pub async fn active(state: &AppState, id: &str) -> Result<Arc<dyn Connection>, Error> {
  state
    .connections
    .lock()
    .await
    .get(id)
    .map(|active| active.connection.clone())
    .ok_or_else(|| Error::NotFound {
      message: format!("connection {id} is not active"),
    })
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::connectors::connector_for;
  use crate::profiles::ConnectorKind;
  use crate::profiles::{
    ConnectionInput, ConnectionProfile, ConnectorParams, CredentialSource, Env, SqlServerParams,
    SslMode,
  };
  use crate::secrets::InMemoryStore;
  use crate::transfer::{self, DuplicateStrategy};
  use crate::tunnels::TunnelInput;

  fn input(credential: CredentialSource, password: Option<&str>) -> ConnectionInput {
    ConnectionInput {
      name: "db".to_string(),
      env: Env::Dev,
      group: None,
      agent_access: Default::default(),
      credential,
      params: ConnectorParams::Postgres(SqlServerParams {
        host: "localhost".to_string(),
        port: 5432,
        database: "app".to_string(),
        user: "soquel".to_string(),
        ssl_mode: SslMode::Prefer,
        ssl_root_cert: None,
        tunnel_id: None,
      }),
      password: password.map(str::to_string),
    }
  }

  fn key(id: &str) -> SecretKey {
    SecretKey::Connection(id.to_string())
  }

  fn state(dir: &tempfile::TempDir) -> AppState {
    AppState::for_tests(dir.path(), Box::new(InMemoryStore::default()))
  }

  /// A live connection to park in the state: the tunnel bookkeeping only reads
  /// which ids are active, not what they talk to.
  async fn parked_connection(dir: &tempfile::TempDir) -> ActiveConnection {
    let file = dir.path().join("parked.db");
    rusqlite::Connection::open(&file).unwrap();
    let profile = ConnectionProfile {
      id: String::new(),
      name: "parked".to_string(),
      env: Env::Dev,
      group: None,
      agent_access: Default::default(),
      credential: Default::default(),
      params: ConnectorParams::Sqlite {
        path: file.to_string_lossy().into_owned(),
      },
    };
    let connection = connector_for(ConnectorKind::Sqlite)
      .connect(&profile, crate::credentials::Credentials::fixed(None), None)
      .await
      .unwrap();
    ActiveConnection {
      connection: connection.into(),
      _tunnel: None,
    }
  }

  fn tunnelled_input(tunnel_id: &str) -> ConnectionInput {
    let mut input = input(CredentialSource::Keychain, None);
    input.params.set_tunnel_id(Some(tunnel_id.to_string()));
    input
  }

  #[test]
  fn leaving_the_keychain_mode_wipes_the_stored_password() {
    let dir = tempfile::tempdir().unwrap();
    let state = state(&dir);

    let stored = state
      .profiles
      .lock()
      .unwrap()
      .create(&input(CredentialSource::Keychain, Some("s3cret")))
      .unwrap();
    persist_secret(&state, &key(&stored.id), &stored.credential, Some("s3cret")).unwrap();
    state
      .session_secrets
      .set(&key(&stored.id), "typed".to_string(), true);
    assert_eq!(
      state.secrets.get(&key(&stored.id)).unwrap(),
      Some("s3cret".to_string())
    );

    // The user switches the connection to "ask every time".
    let switched = state
      .profiles
      .lock()
      .unwrap()
      .update(&stored.id, &input(CredentialSource::Prompt, None))
      .unwrap();
    persist_secret(&state, &key(&switched.id), &switched.credential, None).unwrap();

    assert_eq!(state.secrets.get(&key(&stored.id)).unwrap(), None);
    assert_eq!(state.session_secrets.get(&key(&stored.id)), None);
  }

  #[test]
  fn a_command_profile_never_writes_a_password_to_the_keychain() {
    let dir = tempfile::tempdir().unwrap();
    let state = state(&dir);
    let credential = CredentialSource::Command {
      command: "printf %s token".to_string(),
      refresh_after_secs: None,
    };

    let profile = state
      .profiles
      .lock()
      .unwrap()
      .create(&input(credential, Some("typed-by-mistake")))
      .unwrap();
    crate::ops::persist_secret(
      &state,
      &key(&profile.id),
      &profile.credential,
      Some("typed-by-mistake"),
    )
    .unwrap();

    assert_eq!(state.secrets.get(&key(&profile.id)).unwrap(), None);
  }

  #[test]
  fn a_tunnel_leaving_the_keychain_mode_wipes_its_own_secret() {
    let dir = tempfile::tempdir().unwrap();
    let state = state(&dir);
    let stored = TunnelInput {
      name: "bastion".to_string(),
      host: "bastion.internal".to_string(),
      port: 22,
      user: "deploy".to_string(),
      auth: crate::tunnels::SshAuth::Password,
      credential: CredentialSource::Keychain,
      secret: Some("s3cret".to_string()),
    };
    let tunnel = state.tunnels.lock().unwrap().create(&stored).unwrap();
    let tunnel_key = SecretKey::Tunnel(tunnel.id.clone());
    crate::ops::persist_secret(
      &state,
      &tunnel_key,
      &tunnel.credential,
      stored.secret.as_deref(),
    )
    .unwrap();
    // A connection of the same id keeps its own secret: the keys differ.
    state.secrets.set(&key(&tunnel.id), "db").unwrap();
    state
      .session_secrets
      .set(&tunnel_key, "typed".to_string(), true);

    let asked = TunnelInput {
      credential: CredentialSource::Prompt,
      secret: None,
      ..stored
    };
    let switched = state
      .tunnels
      .lock()
      .unwrap()
      .update(&tunnel.id, &asked)
      .unwrap();
    persist_secret(&state, &tunnel_key, &switched.credential, None).unwrap();

    assert_eq!(state.secrets.get(&tunnel_key).unwrap(), None);
    assert_eq!(state.session_secrets.get(&tunnel_key), None);
    assert_eq!(
      state.secrets.get(&key(&tunnel.id)).unwrap(),
      Some("db".to_string())
    );
  }

  fn command(line: &str) -> CredentialSource {
    CredentialSource::Command {
      command: line.to_string(),
      refresh_after_secs: None,
    }
  }

  #[test]
  fn saving_a_command_approves_it_but_importing_the_same_profile_does_not() {
    let dir = tempfile::tempdir().unwrap();
    let source = state(&dir);
    let line = "aws rds generate-db-auth-token --hostname {host}";

    // Typed in the form: the user just read the argv under the field.
    let saved = source
      .profiles
      .lock()
      .unwrap()
      .create(&input(command(line), None))
      .unwrap();
    approve_own_command(&source, &key(&saved.id), &saved.credential).unwrap();
    assert!(source
      .command_approvals
      .lock()
      .unwrap()
      .is_approved(&key(&saved.id), line));

    // The same profile arriving in a file: nothing grants it anything.
    let target_dir = tempfile::tempdir().unwrap();
    let target = state(&target_dir);
    let path = target_dir.path().join("shared.json");
    transfer::export(&source, &path, false, None).unwrap();
    transfer::import_file(&target, &path, None, true, DuplicateStrategy::Skip).unwrap();

    let imported = &target.profiles.lock().unwrap().list()[0];
    assert_eq!(imported.credential, command(line));
    assert!(
      !target
        .command_approvals
        .lock()
        .unwrap()
        .is_approved(&key(&imported.id), line),
      "an imported command must wait for a yes"
    );
  }

  #[test]
  fn the_approval_records_the_stored_command_not_what_the_caller_says() {
    let dir = tempfile::tempdir().unwrap();
    let state = state(&dir);
    let line = "aws rds generate-db-auth-token";
    let profile = state
      .profiles
      .lock()
      .unwrap()
      .create(&input(command(line), None))
      .unwrap();

    // The webview only names a target: the command comes off the profile, so
    // it cannot approve one thing and run another.
    let stored = current_command(&state, &key(&profile.id)).unwrap();
    assert_eq!(stored, line);

    // A tunnel resolves against its own store, and a profile with no command
    // has nothing to approve.
    let tunnel = state
      .tunnels
      .lock()
      .unwrap()
      .create(&TunnelInput {
        name: "bastion".to_string(),
        host: "bastion.internal".to_string(),
        port: 22,
        user: "deploy".to_string(),
        auth: crate::tunnels::SshAuth::Password,
        credential: command("vault-ssh {host}"),
        secret: None,
      })
      .unwrap();
    assert_eq!(
      current_command(&state, &SecretKey::Tunnel(tunnel.id.clone())).unwrap(),
      "vault-ssh {host}"
    );

    let plain = state
      .profiles
      .lock()
      .unwrap()
      .create(&input(CredentialSource::Keychain, None))
      .unwrap();
    assert!(matches!(
      current_command(&state, &key(&plain.id)),
      Err(Error::NotFound { .. })
    ));
  }

  #[test]
  fn deleting_a_profile_takes_its_approval_with_it() {
    let dir = tempfile::tempdir().unwrap();
    let state = state(&dir);
    let line = "vault read db";
    let profile = state
      .profiles
      .lock()
      .unwrap()
      .create(&input(command(line), None))
      .unwrap();
    approve_own_command(&state, &key(&profile.id), &profile.credential).unwrap();

    state
      .session_secrets
      .set(&key(&profile.id), "typed".to_string(), true);

    state.profiles.lock().unwrap().delete(&profile.id).unwrap();
    forget(&state, &key(&profile.id)).unwrap();

    assert_eq!(state.session_secrets.get(&key(&profile.id)), None);
    assert!(!state
      .command_approvals
      .lock()
      .unwrap()
      .is_approved(&key(&profile.id), line));
  }

  #[test]
  fn leaving_the_command_mode_drops_the_approval() {
    let dir = tempfile::tempdir().unwrap();
    let state = state(&dir);
    let line = "vault read db";
    let profile = state
      .profiles
      .lock()
      .unwrap()
      .create(&input(command(line), None))
      .unwrap();
    approve_own_command(&state, &key(&profile.id), &profile.credential).unwrap();

    let switched = state
      .profiles
      .lock()
      .unwrap()
      .update(&profile.id, &input(CredentialSource::Keychain, Some("pw")))
      .unwrap();
    approve_own_command(&state, &key(&profile.id), &switched.credential).unwrap();

    assert!(!state
      .command_approvals
      .lock()
      .unwrap()
      .is_approved(&key(&profile.id), line));
  }

  #[tokio::test]
  async fn a_shared_tunnel_keeps_its_password_until_the_last_connection_leaves() {
    let dir = tempfile::tempdir().unwrap();
    let state = state(&dir);
    let tunnel = state
      .tunnels
      .lock()
      .unwrap()
      .create(&TunnelInput {
        name: "bastion".to_string(),
        host: "bastion.internal".to_string(),
        port: 22,
        user: "deploy".to_string(),
        auth: crate::tunnels::SshAuth::Password,
        credential: CredentialSource::Prompt,
        secret: None,
      })
      .unwrap();
    let tunnel_key = SecretKey::Tunnel(tunnel.id.clone());

    let first = state
      .profiles
      .lock()
      .unwrap()
      .create(&tunnelled_input(&tunnel.id))
      .unwrap();
    let second = state
      .profiles
      .lock()
      .unwrap()
      .create(&tunnelled_input(&tunnel.id))
      .unwrap();
    {
      let mut connections = state.connections.lock().await;
      connections.insert(first.id.clone(), parked_connection(&dir).await);
      connections.insert(second.id.clone(), parked_connection(&dir).await);
    }
    state
      .session_secrets
      .set(&tunnel_key, "bastion-pw".to_string(), true);

    disconnect(&state, &first.id).await.unwrap();
    assert_eq!(
      state.session_secrets.get(&tunnel_key),
      Some("bastion-pw".to_string()),
      "the second connection still rides the tunnel"
    );

    disconnect(&state, &second.id).await.unwrap();
    assert_eq!(state.session_secrets.get(&tunnel_key), None);
  }

  #[tokio::test]
  async fn a_failed_connect_forgets_the_tunnel_password_it_was_given_once() {
    let dir = tempfile::tempdir().unwrap();
    let state = state(&dir);
    let tunnel = state
      .tunnels
      .lock()
      .unwrap()
      .create(&TunnelInput {
        name: "bastion".to_string(),
        // Nothing listens here: the connect fails on the tunnel, which is the point.
        host: "127.0.0.1".to_string(),
        port: 1,
        user: "deploy".to_string(),
        auth: crate::tunnels::SshAuth::Password,
        credential: CredentialSource::Prompt,
        secret: None,
      })
      .unwrap();
    let tunnel_key = SecretKey::Tunnel(tunnel.id.clone());
    let profile = state
      .profiles
      .lock()
      .unwrap()
      .create(&tunnelled_input(&tunnel.id))
      .unwrap();

    // No password for the tunnel: the core asks instead of dialling.
    let Err(err) = connect(&state, profile.id.clone()).await else {
      panic!("the tunnel must ask for its password");
    };
    assert!(
      matches!(&err, Error::SecretRequired { subject, target_id, .. }
        if *subject == SecretSubject::Tunnel && target_id == &tunnel.id)
    );

    state
      .session_secrets
      .set(&tunnel_key, "one-off".to_string(), false);
    assert!(connect(&state, profile.id.clone()).await.is_err());
    // Given for that attempt only: a failed connect must not keep it around.
    assert_eq!(state.session_secrets.get(&tunnel_key), None);
  }

  #[tokio::test]
  async fn a_one_shot_password_dies_with_the_connect_attempt() {
    let dir = tempfile::tempdir().unwrap();
    let state = state(&dir);
    let file = dir.path().join("app.db");
    rusqlite::Connection::open(&file).unwrap();

    let mut sqlite = input(CredentialSource::Prompt, None);
    sqlite.params = ConnectorParams::Sqlite {
      path: file.to_string_lossy().into_owned(),
    };
    let profile = state.profiles.lock().unwrap().create(&sqlite).unwrap();

    // No password anywhere: the core asks for one instead of connecting.
    let err = connect(&state, profile.id.clone()).await.unwrap_err();
    assert!(matches!(&err, Error::SecretRequired { target_name, .. } if target_name == "db"));

    state
      .session_secrets
      .set(&key(&profile.id), "typed".to_string(), false);
    connect(&state, profile.id.clone()).await.unwrap();
    assert!(state.connections.lock().await.contains_key(&profile.id));
    // Not remembered: the next connect asks again.
    assert_eq!(state.session_secrets.get(&key(&profile.id)), None);

    state
      .session_secrets
      .set(&key(&profile.id), "typed".to_string(), true);
    connect(&state, profile.id.clone()).await.unwrap();
    assert_eq!(
      state.session_secrets.get(&key(&profile.id)),
      Some("typed".to_string())
    );
  }
}
