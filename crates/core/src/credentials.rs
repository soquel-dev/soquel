use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::error::{Error, SecretSubject};
use crate::profiles::{ConnectionProfile, ConnectorParams, CredentialSource};
use crate::secrets::SecretKey;
use crate::tunnels::TunnelProfile;
use crate::AppState;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(20);
const DEFAULT_REFRESH: Duration = Duration::from_secs(300);
const STDERR_LIMIT: usize = 500;

/// Shell syntax we refuse rather than silently pass to the program as literals.
const SHELL_SYNTAX: [&str; 8] = ["|", ">", "<", "&&", "||", ";", "$(", "`"];

fn command_error(message: impl Into<String>, program: &str, stderr: impl Into<String>) -> Error {
  Error::CredentialCommand {
    message: message.into(),
    program: program.to_string(),
    stderr: stderr.into(),
  }
}

/// A credential command, split into argv with the placeholders already filled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
  pub program: String,
  pub args: Vec<String>,
}

/// Connection fields a command line may interpolate.
#[derive(Debug, Default, Clone)]
pub struct Placeholders {
  pub host: Option<String>,
  pub port: Option<u16>,
  pub user: Option<String>,
  pub database: Option<String>,
}

impl Placeholders {
  pub fn from_params(params: &ConnectorParams) -> Self {
    match params {
      ConnectorParams::Postgres(params) | ConnectorParams::Mysql(params) => Self {
        host: Some(params.host.clone()),
        port: Some(params.port),
        user: Some(params.user.clone()),
        database: Some(params.database.clone()),
      },
      ConnectorParams::Redis(params) => Self {
        host: Some(params.host.clone()),
        port: Some(params.port),
        user: params.username.clone(),
        database: Some(params.db.to_string()),
      },
      ConnectorParams::Mongo(params) => Self {
        host: Some(params.host.clone()),
        port: Some(params.port),
        user: params.username.clone(),
        database: params.database.clone(),
      },
      ConnectorParams::Sqlite { .. } => Self::default(),
    }
  }

  pub fn from_tunnel(tunnel: &TunnelProfile) -> Self {
    Self {
      host: Some(tunnel.host.clone()),
      port: Some(tunnel.port),
      user: Some(tunnel.user.clone()),
      database: None,
    }
  }

  fn apply(&self, token: &str) -> String {
    let mut out = token.to_string();
    let values = [
      ("{host}", self.host.clone()),
      ("{port}", self.port.map(|port| port.to_string())),
      ("{user}", self.user.clone()),
      ("{database}", self.database.clone()),
    ];
    for (key, value) in values {
      if let Some(value) = value {
        out = out.replace(key, &value);
      }
    }
    out
  }
}

/// Splits a command line into argv. No shell runs it, so shell syntax would
/// reach the program as literal arguments: refuse it instead.
pub fn parse_command(line: &str) -> Result<CommandSpec, Error> {
  let line = line.trim();
  if line.is_empty() {
    return Err(command_error("the credential command is empty", "", ""));
  }
  if let Some(found) = SHELL_SYNTAX.iter().find(|syntax| line.contains(**syntax)) {
    return Err(command_error(
      format!("`{found}` needs a shell, and the command runs without one; move the pipeline into a script and call that script instead"),
      "",
      "",
    ));
  }
  let tokens = shell_words::split(line)
    .map_err(|err| command_error(format!("unbalanced quotes: {err}"), "", ""))?;
  let mut tokens = tokens.into_iter();
  let program = tokens
    .next()
    .ok_or_else(|| command_error("the credential command is empty", "", ""))?;
  Ok(CommandSpec {
    program,
    args: tokens.collect(),
  })
}

fn resolved_spec(line: &str, placeholders: &Placeholders) -> Result<CommandSpec, Error> {
  let spec = parse_command(line)?;
  Ok(CommandSpec {
    program: placeholders.apply(&spec.program),
    args: spec
      .args
      .iter()
      .map(|arg| placeholders.apply(arg))
      .collect(),
  })
}

async fn run_command(spec: &CommandSpec) -> Result<String, Error> {
  let mut command = tokio::process::Command::new(&spec.program);
  command
    .args(&spec.args)
    .stdin(std::process::Stdio::null())
    .kill_on_drop(true);
  let output = tokio::time::timeout(COMMAND_TIMEOUT, command.output())
    .await
    .map_err(|_| {
      command_error(
        format!(
          "`{}` did not finish within {}s; a command that waits for input (2FA, passphrase) cannot be answered from here",
          spec.program,
          COMMAND_TIMEOUT.as_secs()
        ),
        &spec.program,
        "",
      )
    })?
    .map_err(|err| command_error(format!("`{}` failed to start: {err}", spec.program), &spec.program, ""))?;

  let mut stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
  stderr.truncate(STDERR_LIMIT);
  if !output.status.success() {
    return Err(command_error(
      format!("`{}` exited with {}", spec.program, output.status),
      &spec.program,
      stderr,
    ));
  }
  // Trailing newline only: a shell prints one, a password never ends with one.
  let secret = String::from_utf8_lossy(&output.stdout)
    .trim_end()
    .to_string();
  if secret.is_empty() {
    return Err(command_error(
      format!("`{}` returned no password", spec.program),
      &spec.program,
      stderr,
    ));
  }
  Ok(secret)
}

enum Source {
  Fixed(Option<String>),
  Command {
    spec: CommandSpec,
    refresh_after: Duration,
    cached: Mutex<Option<(String, Instant)>>,
  },
}

/// The password a connection authenticates with, resolved on demand: a pool
/// creating a connection later must be able to pick up a fresh token.
pub struct Credentials(Source);

impl Credentials {
  pub fn fixed(secret: Option<String>) -> Arc<Self> {
    Arc::new(Self(Source::Fixed(secret)))
  }

  pub fn command(spec: CommandSpec, refresh_after: Duration) -> Arc<Self> {
    Arc::new(Self(Source::Command {
      spec,
      refresh_after,
      cached: Mutex::new(None),
    }))
  }

  pub async fn resolve(&self) -> Result<Option<String>, Error> {
    match &self.0 {
      Source::Fixed(secret) => Ok(secret.clone()),
      Source::Command {
        spec,
        refresh_after,
        cached,
      } => {
        if let Some((secret, at)) = cached.lock().unwrap().as_ref() {
          if at.elapsed() < *refresh_after {
            return Ok(Some(secret.clone()));
          }
        }
        let secret = run_command(spec).await?;
        *cached.lock().unwrap() = Some((secret.clone(), Instant::now()));
        Ok(Some(secret))
      }
    }
  }
}

struct SessionSecret {
  value: String,
  one_shot: bool,
}

/// Passwords typed into the prompt. Memory only: never persisted, never
/// handed back to the frontend.
#[derive(Default)]
pub struct SessionSecrets(Mutex<HashMap<SecretKey, SessionSecret>>);

impl SessionSecrets {
  pub fn set(&self, key: &SecretKey, value: String, remember: bool) {
    self.0.lock().unwrap().insert(
      key.clone(),
      SessionSecret {
        value,
        one_shot: !remember,
      },
    );
  }

  pub fn get(&self, key: &SecretKey) -> Option<String> {
    self
      .0
      .lock()
      .unwrap()
      .get(key)
      .map(|secret| secret.value.clone())
  }

  /// Drops what the user did not ask to remember, once the connect attempt is over.
  pub fn clear_one_shot(&self, key: &SecretKey) {
    let mut secrets = self.0.lock().unwrap();
    if secrets.get(key).is_some_and(|secret| secret.one_shot) {
      secrets.remove(key);
    }
  }

  pub fn clear(&self, key: &SecretKey) {
    self.0.lock().unwrap().remove(key);
  }
}

/// What the resolver needs to know about whatever asks for a password.
pub struct CredentialTarget<'a> {
  pub key: SecretKey,
  pub subject: SecretSubject,
  /// Bare id: what the prompt sends back, without the storage prefix.
  pub id: &'a str,
  pub name: &'a str,
  pub credential: &'a CredentialSource,
  pub placeholders: Placeholders,
}

impl<'a> CredentialTarget<'a> {
  pub fn connection(profile: &'a ConnectionProfile, id: &'a str) -> Self {
    Self {
      key: SecretKey::Connection(id.to_string()),
      subject: SecretSubject::Connection,
      id,
      name: &profile.name,
      credential: &profile.credential,
      placeholders: Placeholders::from_params(&profile.params),
    }
  }

  pub fn tunnel(tunnel: &'a TunnelProfile, id: &'a str) -> Self {
    Self {
      key: SecretKey::Tunnel(id.to_string()),
      subject: SecretSubject::Tunnel,
      id,
      name: &tunnel.name,
      credential: &tunnel.credential,
      placeholders: Placeholders::from_tunnel(tunnel),
    }
  }
}

/// How a target gets its password. `override_secret` short-circuits every
/// mode: it is the password typed in the form for a one-off test.
pub fn resolve_credentials(
  state: &AppState,
  target: &CredentialTarget<'_>,
  override_secret: Option<String>,
) -> Result<Arc<Credentials>, Error> {
  if override_secret.is_some() {
    return Ok(Credentials::fixed(override_secret));
  }
  match target.credential {
    CredentialSource::Keychain => Ok(Credentials::fixed(state.secrets.get(&target.key)?)),
    CredentialSource::Prompt => match state.session_secrets.get(&target.key) {
      Some(secret) => Ok(Credentials::fixed(Some(secret))),
      None => Err(Error::SecretRequired {
        message: format!("{} asks for its password at each connection", target.name),
        subject: target.subject,
        target_id: target.id.to_string(),
        target_name: target.name.to_string(),
      }),
    },
    CredentialSource::Command {
      command,
      refresh_after_secs,
    } => {
      let spec = resolved_spec(command, &target.placeholders)?;
      // Running a command is running code: an imported one waits for a yes.
      if !state
        .command_approvals
        .lock()
        .unwrap()
        .is_approved(&target.key, command)
      {
        return Err(Error::CommandApprovalRequired {
          message: format!(
            "{} gets its password from a command that has not been approved on this machine",
            target.name
          ),
          subject: target.subject,
          target_id: target.id.to_string(),
          target_name: target.name.to_string(),
          program: spec.program,
          args: spec.args,
        });
      }
      let refresh_after = refresh_after_secs
        .map(|secs| Duration::from_secs(u64::from(secs)))
        .unwrap_or(DEFAULT_REFRESH);
      Ok(Credentials::command(spec, refresh_after))
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::profiles::{Env, RedisParams, SqlServerParams, SslMode};
  use crate::secrets::InMemoryStore;

  fn profile(credential: CredentialSource, params: ConnectorParams) -> ConnectionProfile {
    ConnectionProfile {
      id: "c-1".to_string(),
      name: "prod".to_string(),
      env: Env::Prod,
      group: None,
      agent_access: Default::default(),
      credential,
      params,
    }
  }

  fn pg_params() -> ConnectorParams {
    ConnectorParams::Postgres(SqlServerParams {
      host: "db.internal".to_string(),
      port: 5432,
      database: "shop".to_string(),
      user: "app".to_string(),
      ssl_mode: SslMode::Prefer,
      ssl_root_cert: None,
      tunnel_id: None,
    })
  }

  /// The store the user's own form writes to when they save a command.
  fn approve(state: &AppState, key: &SecretKey, credential: &CredentialSource) {
    let CredentialSource::Command { command, .. } = credential else {
      panic!("only a command mode is approved");
    };
    state
      .command_approvals
      .lock()
      .unwrap()
      .approve(key, command)
      .unwrap();
  }

  fn spec(program: &str, args: &[&str]) -> CommandSpec {
    CommandSpec {
      program: program.to_string(),
      args: args.iter().map(|arg| arg.to_string()).collect(),
    }
  }

  #[test]
  fn splits_a_command_line_honouring_quotes() {
    assert_eq!(
      parse_command("  aws rds generate-db-auth-token --username 'app user'  ").unwrap(),
      spec(
        "aws",
        &["rds", "generate-db-auth-token", "--username", "app user"]
      )
    );
  }

  #[test]
  fn refuses_shell_syntax_and_unbalanced_quotes() {
    for line in [
      "vault read x | jq -r .token",
      "echo `whoami`",
      "a && b",
      "x; y",
    ] {
      assert!(matches!(
        parse_command(line),
        Err(Error::CredentialCommand { .. })
      ));
    }
    assert!(parse_command("aws --username 'app").is_err());
    assert!(parse_command("   ").is_err());
  }

  #[test]
  fn substitutes_placeholders_per_argument() {
    let placeholders = Placeholders {
      host: Some("db.internal".to_string()),
      port: Some(5432),
      user: Some("app".to_string()),
      database: Some("shop".to_string()),
    };
    let resolved = resolved_spec(
      "token --host={host} --port {port} --user {user} --db {database} --keep {unknown}",
      &placeholders,
    )
    .unwrap();
    assert_eq!(
      resolved,
      spec(
        "token",
        &[
          "--host=db.internal",
          "--port",
          "5432",
          "--user",
          "app",
          "--db",
          "shop",
          "--keep",
          "{unknown}",
        ]
      )
    );
  }

  #[tokio::test]
  async fn command_output_loses_its_trailing_newline() {
    let credentials = Credentials::command(spec("printf", &["s3cret\\n"]), DEFAULT_REFRESH);
    assert_eq!(
      credentials.resolve().await.unwrap(),
      Some("s3cret".to_string())
    );
  }

  #[tokio::test]
  async fn a_failing_command_reports_its_stderr() {
    let credentials = Credentials::command(
      spec("sh", &["-c", "echo boom >&2; exit 3"]),
      DEFAULT_REFRESH,
    );
    let Err(Error::CredentialCommand { stderr, .. }) = credentials.resolve().await else {
      panic!("expected a credential command error");
    };
    assert_eq!(stderr, "boom");
  }

  #[tokio::test]
  async fn an_empty_output_is_not_a_password() {
    let credentials = Credentials::command(spec("true", &[]), DEFAULT_REFRESH);
    assert!(credentials.resolve().await.is_err());
  }

  #[tokio::test]
  async fn a_missing_program_fails_to_start() {
    let credentials = Credentials::command(spec("soquel-no-such-binary", &[]), DEFAULT_REFRESH);
    assert!(credentials.resolve().await.is_err());
  }

  #[tokio::test]
  async fn the_command_runs_again_once_the_cache_expires() {
    let dir = tempfile::tempdir().unwrap();
    let counter = dir.path().join("runs");
    let script = dir.path().join("token.sh");
    std::fs::write(
      &script,
      format!(
        "#!/bin/sh\necho x >> {counter}\nwc -l < {counter} | tr -d ' '\n",
        counter = counter.display()
      ),
    )
    .unwrap();
    #[cfg(unix)]
    {
      use std::os::unix::fs::PermissionsExt;
      std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    let spec = spec(script.to_str().unwrap(), &[]);
    let cached = Credentials::command(spec.clone(), Duration::from_secs(300));
    assert_eq!(cached.resolve().await.unwrap(), Some("1".to_string()));
    assert_eq!(cached.resolve().await.unwrap(), Some("1".to_string()));

    let expiring = Credentials::command(spec, Duration::from_millis(1));
    assert_eq!(expiring.resolve().await.unwrap(), Some("2".to_string()));
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert_eq!(expiring.resolve().await.unwrap(), Some("3".to_string()));
  }

  #[tokio::test]
  async fn a_command_gets_the_placeholders_of_its_own_connection() {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::for_tests(dir.path(), Box::new(InMemoryStore::default()));
    let credential = CredentialSource::Command {
      command: "printf %s {user}@{host}:{port}/{database}".to_string(),
      refresh_after_secs: None,
    };

    let pg = profile(credential.clone(), pg_params());
    approve(
      &state,
      &SecretKey::Connection("c-1".to_string()),
      &credential,
    );
    let credentials =
      resolve_credentials(&state, &CredentialTarget::connection(&pg, "c-1"), None).unwrap();
    assert_eq!(
      credentials.resolve().await.unwrap(),
      Some("app@db.internal:5432/shop".to_string())
    );

    // Every kind fills what it has; redis carries its numeric db.
    let redis = ConnectorParams::Redis(RedisParams {
      host: "cache".to_string(),
      port: 6379,
      db: 2,
      username: None,
      tls: false,
      tunnel_id: None,
    });
    let cache = profile(credential, redis);
    let credentials =
      resolve_credentials(&state, &CredentialTarget::connection(&cache, "c-1"), None).unwrap();
    assert_eq!(
      credentials.resolve().await.unwrap(),
      // No ACL user: {user} is left alone rather than blanked.
      Some("{user}@cache:6379/2".to_string())
    );
  }

  #[tokio::test]
  async fn an_unapproved_command_shows_what_it_would_run_instead_of_running_it() {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::for_tests(dir.path(), Box::new(InMemoryStore::default()));
    let credential = CredentialSource::Command {
      command: "aws rds generate-db-auth-token --hostname {host}".to_string(),
      refresh_after_secs: None,
    };
    let pg = profile(credential.clone(), pg_params());
    let key = SecretKey::Connection("c-1".to_string());

    let Err(err) = resolve_credentials(&state, &CredentialTarget::connection(&pg, "c-1"), None)
    else {
      panic!("an unapproved command must not resolve");
    };
    let Error::CommandApprovalRequired {
      subject,
      target_id,
      program,
      args,
      ..
    } = &err
    else {
      panic!("expected an approval request: {err:?}");
    };
    assert_eq!(*subject, SecretSubject::Connection);
    assert_eq!(target_id, "c-1");
    // The dialog shows the resolved argv, not the raw line.
    assert_eq!(program, "aws");
    assert_eq!(
      args,
      &["rds", "generate-db-auth-token", "--hostname", "db.internal"]
    );

    approve(&state, &key, &credential);
    assert!(resolve_credentials(&state, &CredentialTarget::connection(&pg, "c-1"), None).is_ok());
  }

  #[tokio::test]
  async fn editing_an_approved_command_puts_it_back_in_waiting() {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::for_tests(dir.path(), Box::new(InMemoryStore::default()));
    let key = SecretKey::Connection("c-1".to_string());
    approve(
      &state,
      &key,
      &CredentialSource::Command {
        command: "aws rds token".to_string(),
        refresh_after_secs: None,
      },
    );

    let edited = profile(
      CredentialSource::Command {
        command: "curl evil.example.com".to_string(),
        refresh_after_secs: None,
      },
      pg_params(),
    );
    assert!(matches!(
      resolve_credentials(&state, &CredentialTarget::connection(&edited, "c-1"), None),
      Err(Error::CommandApprovalRequired { .. })
    ));
  }

  #[tokio::test]
  async fn each_mode_resolves_from_its_own_store() {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::for_tests(dir.path(), Box::new(InMemoryStore::default()));
    state
      .secrets
      .set(&SecretKey::Connection("c-1".to_string()), "from-keychain")
      .unwrap();

    let keychain = profile(CredentialSource::Keychain, pg_params());
    let target = CredentialTarget::connection(&keychain, "c-1");
    let resolved = resolve_credentials(&state, &target, None).unwrap();
    assert_eq!(
      resolved.resolve().await.unwrap(),
      Some("from-keychain".to_string())
    );

    // The password typed in the form wins, whatever the profile says.
    let resolved = resolve_credentials(&state, &target, Some("typed".to_string())).unwrap();
    assert_eq!(resolved.resolve().await.unwrap(), Some("typed".to_string()));

    // Prompt never reads the keychain, even with a password sitting there.
    let prompt = profile(CredentialSource::Prompt, pg_params());
    let target = CredentialTarget::connection(&prompt, "c-1");
    let Err(err) = resolve_credentials(&state, &target, None) else {
      panic!("prompt mode must ask instead of reading the keychain");
    };
    assert!(
      matches!(&err, Error::SecretRequired { subject, target_id, .. }
        if *subject == SecretSubject::Connection && target_id == "c-1")
    );

    state.session_secrets.set(
      &SecretKey::Connection("c-1".to_string()),
      "typed".to_string(),
      true,
    );
    let resolved = resolve_credentials(&state, &target, None).unwrap();
    assert_eq!(resolved.resolve().await.unwrap(), Some("typed".to_string()));
  }

  #[tokio::test]
  async fn a_command_waiting_on_stdin_gets_an_eof_not_a_hang() {
    let credentials = Credentials::command(spec("cat", &[]), DEFAULT_REFRESH);
    assert!(credentials.resolve().await.is_err());
  }

  #[test]
  fn session_secrets_forget_what_was_not_remembered() {
    let secrets = SessionSecrets::default();
    let a = SecretKey::Connection("a".to_string());
    let b = SecretKey::Connection("b".to_string());

    secrets.set(&a, "one-off".to_string(), false);
    assert_eq!(secrets.get(&a), Some("one-off".to_string()));
    secrets.clear_one_shot(&a);
    assert_eq!(secrets.get(&a), None);

    secrets.set(&b, "kept".to_string(), true);
    secrets.clear_one_shot(&b);
    assert_eq!(secrets.get(&b), Some("kept".to_string()));
    secrets.clear(&b);
    assert_eq!(secrets.get(&b), None);
  }

  #[tokio::test]
  async fn a_tunnel_prompt_names_its_own_subject() {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::for_tests(dir.path(), Box::new(InMemoryStore::default()));
    let tunnel = TunnelProfile {
      id: "t-1".to_string(),
      name: "bastion".to_string(),
      host: "bastion.internal".to_string(),
      port: 2222,
      user: "deploy".to_string(),
      auth: crate::tunnels::SshAuth::Password,
      credential: CredentialSource::Prompt,
    };

    let target = CredentialTarget::tunnel(&tunnel, "t-1");
    let Err(err) = resolve_credentials(&state, &target, None) else {
      panic!("a prompt tunnel must ask before opening");
    };
    assert!(
      matches!(&err, Error::SecretRequired { subject, target_id, target_name, .. }
        if *subject == SecretSubject::Tunnel && target_id == "t-1" && target_name == "bastion")
    );

    // The prompt writes under the tunnel key, not the connection one.
    state.session_secrets.set(
      &SecretKey::Tunnel("t-1".to_string()),
      "pass".to_string(),
      true,
    );
    let resolved = resolve_credentials(&state, &target, None).unwrap();
    assert_eq!(resolved.resolve().await.unwrap(), Some("pass".to_string()));
  }

  #[tokio::test]
  async fn a_tunnel_command_gets_host_port_and_user_but_no_database() {
    let tunnel = TunnelProfile {
      id: "t-1".to_string(),
      name: "bastion".to_string(),
      host: "bastion.internal".to_string(),
      port: 2222,
      user: "deploy".to_string(),
      auth: crate::tunnels::SshAuth::Password,
      credential: CredentialSource::Command {
        command: "printf %s {user}@{host}:{port}/{database}".to_string(),
        refresh_after_secs: None,
      },
    };
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::for_tests(dir.path(), Box::new(InMemoryStore::default()));

    approve(
      &state,
      &SecretKey::Tunnel("t-1".to_string()),
      &tunnel.credential,
    );
    let credentials =
      resolve_credentials(&state, &CredentialTarget::tunnel(&tunnel, "t-1"), None).unwrap();
    assert_eq!(
      credentials.resolve().await.unwrap(),
      // A tunnel has no database: the placeholder stays literal.
      Some("deploy@bastion.internal:2222/{database}".to_string())
    );
  }
}
