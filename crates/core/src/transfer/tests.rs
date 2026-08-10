use std::collections::HashMap;
use std::path::PathBuf;

use super::*;
use crate::known_hosts::KnownHostsStore;
use crate::profiles::{ConnectionInput, ProfileStore, RedisParams, SqlServerParams, SslMode};
use crate::secrets::{InMemoryStore, SecretKey, SecretStore};
use crate::tunnels::{TunnelInput, TunnelStore};

fn app_state(dir: &tempfile::TempDir) -> AppState {
  app_state_with(dir, Box::new(InMemoryStore::default()))
}

fn app_state_with(dir: &tempfile::TempDir, secrets: Box<dyn SecretStore>) -> AppState {
  AppState {
    profiles: std::sync::Mutex::new(
      ProfileStore::load(dir.path().join("connections.json")).unwrap(),
    ),
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
    secrets,
    secrets_problem: None,
    session_secrets: Default::default(),
    connections: tokio::sync::Mutex::new(HashMap::new()),
    sessions: tokio::sync::Mutex::new(HashMap::new()),
    data_dir: dir.path().to_path_buf(),
    mcp: tokio::sync::Mutex::new(None),
    approvals: tokio::sync::Mutex::new(HashMap::new()),
    trust_windows: tokio::sync::Mutex::new(HashMap::new()),
  }
}

fn pg_params(tunnel_id: Option<String>) -> ConnectorParams {
  ConnectorParams::Postgres(SqlServerParams {
    host: "db.internal".to_string(),
    port: 5432,
    database: "app".to_string(),
    user: "soquel".to_string(),
    ssl_mode: SslMode::VerifyFull,
    ssl_root_cert: Some("/etc/ca.pem".to_string()),
    tunnel_id,
  })
}

fn tunnel_input() -> TunnelInput {
  TunnelInput {
    name: "bastion".to_string(),
    host: "bastion.internal".to_string(),
    port: 22,
    user: "deploy".to_string(),
    auth: SshAuth::KeyFile {
      path: "~/.ssh/id_ed25519".to_string(),
    },
    credential: Default::default(),
    secret: Some("key-passphrase".to_string()),
  }
}

/// A source with a tunnel, a grouped connection referencing it, and a loose one.
fn seeded(dir: &tempfile::TempDir) -> (AppState, String) {
  let state = app_state(dir);
  let tunnel = state
    .tunnels
    .lock()
    .unwrap()
    .create(&tunnel_input())
    .unwrap();
  state
    .secrets
    .set(&SecretKey::Tunnel(tunnel.id.clone()), "key-passphrase")
    .unwrap();
  let tunneled = state
    .profiles
    .lock()
    .unwrap()
    .create(&ConnectionInput {
      name: "prod".to_string(),
      env: Env::Prod,
      group: Some("clients".to_string()),
      agent_access: AgentAccess::ReadOnly,
      credential: Default::default(),
      params: pg_params(Some(tunnel.id.clone())),
      password: None,
    })
    .unwrap();
  state
    .secrets
    .set(&SecretKey::Connection(tunneled.id.clone()), "pg-password")
    .unwrap();
  state
    .profiles
    .lock()
    .unwrap()
    .create(&ConnectionInput {
      name: "cache".to_string(),
      env: Env::Dev,
      group: None,
      agent_access: AgentAccess::None,
      credential: Default::default(),
      params: ConnectorParams::Redis(RedisParams {
        host: "localhost".to_string(),
        port: 6379,
        db: 2,
        username: None,
        tls: false,
        tunnel_id: None,
      }),
      password: None,
    })
    .unwrap();
  (state, tunnel.id)
}

fn out(dir: &tempfile::TempDir) -> PathBuf {
  dir.path().join("connections.soquel.json")
}

#[test]
fn plaintext_roundtrip_carries_connections_tunnels_and_groups() {
  let source_dir = tempfile::tempdir().unwrap();
  let (source, source_tunnel) = seeded(&source_dir);
  let path = out(&source_dir);

  let summary = export(&source, &path, false, None).unwrap();
  assert_eq!(summary.connections, 2);
  assert_eq!(summary.tunnels, 1);
  assert_eq!(summary.secrets, 0);
  assert!(!summary.encrypted);
  // Shareable by default: no password anywhere in the bytes.
  let raw = std::fs::read_to_string(&path).unwrap();
  assert!(!raw.contains("pg-password"), "{raw}");
  assert!(!raw.contains("key-passphrase"), "{raw}");

  let target_dir = tempfile::tempdir().unwrap();
  let target = app_state(&target_dir);
  let outcome = import_file(&target, &path, None, true, DuplicateStrategy::Skip).unwrap();
  assert_eq!(outcome.created, 2);
  assert_eq!(outcome.tunnels_created, 1);

  let profiles = target.profiles.lock().unwrap().list();
  let tunnels = target.tunnels.lock().unwrap().list();
  let prod = profiles.iter().find(|p| p.name == "prod").unwrap();
  assert_eq!(prod.group.as_deref(), Some("clients"));
  assert_eq!(prod.env, Env::Prod);
  assert_eq!(
    prod.params.sql_server().unwrap().ssl_root_cert.as_deref(),
    Some("/etc/ca.pem")
  );
  // Fresh ids, and the tunnel reference follows the new one.
  let imported_tunnel = &tunnels[0];
  assert_ne!(imported_tunnel.id, source_tunnel);
  assert_ne!(prod.id, source.profiles.lock().unwrap().list()[0].id);
  assert_eq!(
    prod.params.remote().unwrap().tunnel_id,
    Some(imported_tunnel.id.as_str())
  );
  assert_eq!(imported_tunnel.name, "bastion");
  assert_eq!(
    imported_tunnel.auth,
    SshAuth::KeyFile {
      path: "~/.ssh/id_ed25519".to_string()
    }
  );
  // No secrets in the file: nothing lands in the keychain.
  assert_eq!(
    target
      .secrets
      .get(&SecretKey::Connection(prod.id.clone()))
      .unwrap(),
    None
  );
}

#[test]
fn encrypted_roundtrip_restores_the_secrets() {
  let source_dir = tempfile::tempdir().unwrap();
  let (source, _) = seeded(&source_dir);
  let path = out(&source_dir);

  let summary = export(&source, &path, true, Some("correct horse")).unwrap();
  assert_eq!(summary.secrets, 2);
  assert!(summary.encrypted);
  let raw = std::fs::read_to_string(&path).unwrap();
  assert!(!raw.contains("pg-password"), "{raw}");
  assert!(!raw.contains("db.internal"), "{raw}");

  let target_dir = tempfile::tempdir().unwrap();
  let target = app_state(&target_dir);
  let preview = preview_file(&target, &path, Some("correct horse")).unwrap();
  assert!(preview.encrypted);
  assert!(!preview.needs_passphrase);
  assert_eq!(preview.connections.len(), 2);
  assert!(preview.connections.iter().any(|entry| entry.has_secret));

  import_file(
    &target,
    &path,
    Some("correct horse"),
    true,
    DuplicateStrategy::Skip,
  )
  .unwrap();
  let profiles = target.profiles.lock().unwrap().list();
  let prod = profiles.iter().find(|p| p.name == "prod").unwrap();
  assert_eq!(
    target
      .secrets
      .get(&SecretKey::Connection(prod.id.clone()))
      .unwrap()
      .as_deref(),
    Some("pg-password")
  );
  let tunnel = &target.tunnels.lock().unwrap().list()[0];
  assert_eq!(
    target
      .secrets
      .get(&SecretKey::Tunnel(tunnel.id.clone()))
      .unwrap()
      .as_deref(),
    Some("key-passphrase")
  );
}

#[test]
fn an_encrypted_file_announces_itself_before_the_passphrase() {
  let source_dir = tempfile::tempdir().unwrap();
  let (source, _) = seeded(&source_dir);
  let path = out(&source_dir);
  export(&source, &path, true, Some("pass")).unwrap();

  let target_dir = tempfile::tempdir().unwrap();
  let target = app_state(&target_dir);
  let preview = preview_file(&target, &path, None).unwrap();
  assert!(preview.needs_passphrase);
  assert!(preview.connections.is_empty());

  let err = import_file(&target, &path, None, true, DuplicateStrategy::Skip).unwrap_err();
  assert!(
    matches!(&err, Error::Secret { message } if message.contains("passphrase is required")),
    "{err:?}"
  );
  assert!(target.profiles.lock().unwrap().list().is_empty());
}

#[test]
fn a_wrong_passphrase_is_a_clear_error_and_writes_nothing() {
  let source_dir = tempfile::tempdir().unwrap();
  let (source, _) = seeded(&source_dir);
  let path = out(&source_dir);
  export(&source, &path, true, Some("right")).unwrap();

  let target_dir = tempfile::tempdir().unwrap();
  let target = app_state(&target_dir);
  let err = import_file(&target, &path, Some("wrong"), true, DuplicateStrategy::Skip).unwrap_err();
  assert!(
    matches!(&err, Error::Secret { message } if message.contains("wrong passphrase")),
    "{err:?}"
  );
  assert!(target.profiles.lock().unwrap().list().is_empty());
  assert!(target.tunnels.lock().unwrap().list().is_empty());
}

#[test]
fn exporting_secrets_without_a_passphrase_is_refused() {
  let dir = tempfile::tempdir().unwrap();
  let (state, _) = seeded(&dir);
  let path = out(&dir);
  let err = export(&state, &path, true, Some("   ")).unwrap_err();
  assert!(matches!(err, Error::Secret { .. }), "{err:?}");
  assert!(!path.exists());
}

#[test]
fn a_future_version_is_refused_explicitly() {
  let dir = tempfile::tempdir().unwrap();
  let state = app_state(&dir);
  let path = out(&dir);
  std::fs::write(
    &path,
    format!(
      r#"{{"soquel":"soquel-connections","version":{},"document":{{"connections":[],"tunnels":[]}}}}"#,
      file::CURRENT_VERSION + 1
    ),
  )
  .unwrap();

  let err = preview_file(&state, &path, None).unwrap_err();
  assert!(
    matches!(&err, Error::Unsupported { message } if message.contains("update soquel")),
    "{err:?}"
  );
}

#[test]
fn a_foreign_json_file_is_not_mistaken_for_an_export() {
  let dir = tempfile::tempdir().unwrap();
  let state = app_state(&dir);
  let path = out(&dir);
  std::fs::write(&path, r#"{"connections":[]}"#).unwrap();
  let err = preview_file(&state, &path, None).unwrap_err();
  assert!(
    matches!(&err, Error::Storage { message } if message.contains("not a soquel connections file")),
    "{err:?}"
  );
}

#[test]
fn agent_access_is_forced_off_whatever_the_file_says() {
  let dir = tempfile::tempdir().unwrap();
  let state = app_state(&dir);
  let path = out(&dir);
  std::fs::write(
    &path,
    r#"{
      "soquel": "soquel-connections",
      "version": 1,
      "document": {
        "connections": [{
          "name": "smuggled",
          "env": "prod",
          "agentAccess": "read-only",
          "params": {
            "kind": "postgres", "host": "db", "port": 5432,
            "database": "app", "user": "soquel"
          }
        }],
        "tunnels": []
      }
    }"#,
  )
  .unwrap();

  import_file(&state, &path, None, true, DuplicateStrategy::Skip).unwrap();
  let imported = &state.profiles.lock().unwrap().list()[0];
  assert_eq!(imported.agent_access, AgentAccess::None);
}

#[test]
fn the_credential_mode_survives_a_roundtrip_without_the_secret() {
  let dir = tempfile::tempdir().unwrap();
  let state = app_state(&dir);
  let path = out(&dir);
  state
    .profiles
    .lock()
    .unwrap()
    .create(&ConnectionInput {
      name: "iam".to_string(),
      env: Env::Prod,
      group: None,
      agent_access: AgentAccess::None,
      credential: CredentialSource::Command {
        command: "aws rds generate-db-auth-token --hostname {host}".to_string(),
        refresh_after_secs: Some(60),
      },
      params: pg_params(None),
      password: None,
    })
    .unwrap();

  export(&state, &path, false, None).unwrap();

  let target_dir = tempfile::tempdir().unwrap();
  let target = app_state(&target_dir);
  import_file(&target, &path, None, true, DuplicateStrategy::Skip).unwrap();
  let imported = &target.profiles.lock().unwrap().list()[0];
  assert_eq!(
    imported.credential,
    CredentialSource::Command {
      command: "aws rds generate-db-auth-token --hostname {host}".to_string(),
      refresh_after_secs: Some(60),
    }
  );
}

#[test]
fn the_preview_flags_the_entries_that_carry_a_command() {
  let dir = tempfile::tempdir().unwrap();
  let state = app_state(&dir);
  let path = out(&dir);
  state
    .profiles
    .lock()
    .unwrap()
    .create(&ConnectionInput {
      name: "iam".to_string(),
      env: Env::Prod,
      group: None,
      agent_access: AgentAccess::None,
      credential: CredentialSource::Command {
        command: "aws rds generate-db-auth-token".to_string(),
        refresh_after_secs: None,
      },
      params: pg_params(None),
      password: None,
    })
    .unwrap();
  state
    .profiles
    .lock()
    .unwrap()
    .create(&ConnectionInput {
      name: "plain".to_string(),
      env: Env::Dev,
      group: None,
      agent_access: AgentAccess::None,
      credential: CredentialSource::Keychain,
      params: pg_params(None),
      password: None,
    })
    .unwrap();

  export(&state, &path, false, None).unwrap();
  // The approval is local: an export cannot carry one along.
  let raw = std::fs::read_to_string(&path).unwrap();
  assert!(!raw.contains("approv"), "{raw}");

  let target_dir = tempfile::tempdir().unwrap();
  let preview = preview_file(&app_state(&target_dir), &path, None).unwrap();
  let flagged: Vec<&str> = preview
    .connections
    .iter()
    .filter(|entry| entry.has_command)
    .map(|entry| entry.name.as_str())
    .collect();
  assert_eq!(flagged, vec!["iam"]);
}

#[test]
fn a_password_only_crosses_over_when_it_was_asked_for() {
  let source_dir = tempfile::tempdir().unwrap();
  let (source, _) = seeded(&source_dir);
  let path = out(&source_dir);
  export(&source, &path, true, Some("correct horse")).unwrap();

  // Same file, same passphrase, twice: only the flag differs. Decrypting is
  // not the same as agreeing to take what is inside.
  let without_dir = tempfile::tempdir().unwrap();
  let without = app_state(&without_dir);
  import_file(
    &without,
    &path,
    Some("correct horse"),
    false,
    DuplicateStrategy::Skip,
  )
  .unwrap();
  let prod = without
    .profiles
    .lock()
    .unwrap()
    .list()
    .into_iter()
    .find(|profile| profile.name == "prod")
    .unwrap();
  assert_eq!(
    without
      .secrets
      .get(&SecretKey::Connection(prod.id.clone()))
      .unwrap(),
    None,
    "the connection landed without the password the file was carrying"
  );

  let with_dir = tempfile::tempdir().unwrap();
  let with = app_state(&with_dir);
  import_file(
    &with,
    &path,
    Some("correct horse"),
    true,
    DuplicateStrategy::Skip,
  )
  .unwrap();
  let prod = with
    .profiles
    .lock()
    .unwrap()
    .list()
    .into_iter()
    .find(|profile| profile.name == "prod")
    .unwrap();
  assert_eq!(
    with
      .secrets
      .get(&SecretKey::Connection(prod.id))
      .unwrap()
      .as_deref(),
    Some("pg-password")
  );
}

#[test]
fn a_tunnel_carries_its_credential_mode_across_a_roundtrip() {
  let dir = tempfile::tempdir().unwrap();
  let state = app_state(&dir);
  let path = out(&dir);
  let asked = TunnelInput {
    credential: CredentialSource::Prompt,
    ..tunnel_input()
  };
  state.tunnels.lock().unwrap().create(&asked).unwrap();

  export(&state, &path, false, None).unwrap();

  let target_dir = tempfile::tempdir().unwrap();
  let target = app_state(&target_dir);
  import_file(&target, &path, None, true, DuplicateStrategy::Skip).unwrap();
  assert_eq!(
    target.tunnels.lock().unwrap().list()[0].credential,
    CredentialSource::Prompt
  );
}

#[test]
fn a_file_without_a_credential_mode_reads_as_keychain() {
  let dir = tempfile::tempdir().unwrap();
  let state = app_state(&dir);
  let path = out(&dir);
  std::fs::write(
    &path,
    r#"{
      "soquel": "soquel-connections",
      "version": 1,
      "document": {
        "connections": [{
          "name": "older",
          "env": "dev",
          "params": {
            "kind": "postgres", "host": "db", "port": 5432,
            "database": "app", "user": "soquel"
          }
        }],
        "tunnels": []
      }
    }"#,
  )
  .unwrap();

  import_file(&state, &path, None, true, DuplicateStrategy::Skip).unwrap();
  let imported = &state.profiles.lock().unwrap().list()[0];
  assert_eq!(imported.credential, CredentialSource::Keychain);
}

#[test]
fn replacing_a_duplicate_also_revokes_its_agent_access() {
  let dir = tempfile::tempdir().unwrap();
  let (state, _) = seeded(&dir);
  let path = out(&dir);
  export(&state, &path, false, None).unwrap();
  // The store still holds the read-only grant the export dropped.
  let before = state.profiles.lock().unwrap().list();
  let prod = before.iter().find(|p| p.name == "prod").unwrap();
  assert_eq!(prod.agent_access, AgentAccess::ReadOnly);

  let outcome = import_file(&state, &path, None, true, DuplicateStrategy::Replace).unwrap();
  assert_eq!(outcome.replaced, 2);
  assert_eq!(outcome.created, 0);
  let after = state.profiles.lock().unwrap().list();
  assert_eq!(after.len(), 2);
  let prod_after = after.iter().find(|p| p.name == "prod").unwrap();
  assert_eq!(prod_after.id, prod.id);
  assert_eq!(prod_after.agent_access, AgentAccess::None);
  // Replace reuses the existing tunnel too: no orphan copy.
  assert_eq!(state.tunnels.lock().unwrap().list().len(), 1);
}

#[test]
fn skip_leaves_duplicates_and_their_passwords_alone() {
  let dir = tempfile::tempdir().unwrap();
  let (state, _) = seeded(&dir);
  let path = out(&dir);
  export(&state, &path, true, Some("pass")).unwrap();
  let prod_id = state
    .profiles
    .lock()
    .unwrap()
    .list()
    .into_iter()
    .find(|p| p.name == "prod")
    .unwrap()
    .id;
  state
    .secrets
    .set(&SecretKey::Connection(prod_id.clone()), "rotated-since")
    .unwrap();

  let outcome = import_file(&state, &path, Some("pass"), true, DuplicateStrategy::Skip).unwrap();
  assert_eq!(outcome.skipped, 2);
  assert_eq!(outcome.created, 0);
  assert_eq!(outcome.tunnels_created, 0);
  assert_eq!(state.profiles.lock().unwrap().list().len(), 2);
  assert_eq!(
    state
      .secrets
      .get(&SecretKey::Connection(prod_id.clone()))
      .unwrap()
      .as_deref(),
    Some("rotated-since")
  );
}

#[test]
fn keep_both_disambiguates_the_names_and_the_tunnels() {
  let dir = tempfile::tempdir().unwrap();
  let (state, _) = seeded(&dir);
  let path = out(&dir);
  export(&state, &path, false, None).unwrap();

  import_file(&state, &path, None, true, DuplicateStrategy::KeepBoth).unwrap();
  let profiles = state.profiles.lock().unwrap().list();
  let tunnels = state.tunnels.lock().unwrap().list();
  assert_eq!(profiles.len(), 4);
  assert_eq!(tunnels.len(), 2);
  assert!(profiles.iter().any(|p| p.name == "prod (imported)"));
  assert!(tunnels.iter().any(|t| t.name == "bastion (imported)"));
  // The copy points at the copied tunnel, not the original.
  let copy = profiles
    .iter()
    .find(|p| p.name == "prod (imported)")
    .unwrap();
  let copied_tunnel = tunnels
    .iter()
    .find(|t| t.name == "bastion (imported)")
    .unwrap();
  assert_eq!(
    copy.params.remote().unwrap().tunnel_id,
    Some(copied_tunnel.id.as_str())
  );

  // A second pass has to find yet another free name.
  import_file(&state, &path, None, true, DuplicateStrategy::KeepBoth).unwrap();
  let profiles = state.profiles.lock().unwrap().list();
  assert!(profiles.iter().any(|p| p.name == "prod (imported 2)"));
}

#[test]
fn an_invalid_entry_blocks_the_whole_import() {
  let dir = tempfile::tempdir().unwrap();
  let state = app_state(&dir);
  let path = out(&dir);
  std::fs::write(
    &path,
    r#"{
      "soquel": "soquel-connections",
      "version": 1,
      "document": {
        "connections": [
          {"name": "fine", "env": "dev", "params": {
            "kind": "postgres", "host": "db", "port": 5432, "database": "app", "user": "soquel"}},
          {"name": "broken", "env": "dev", "params": {
            "kind": "postgres", "host": "", "port": 5432, "database": "app", "user": "soquel"}}
        ],
        "tunnels": []
      }
    }"#,
  )
  .unwrap();

  let preview = preview_file(&state, &path, None).unwrap();
  assert_eq!(preview.connections[0].problem, None);
  assert_eq!(
    preview.connections[1].problem.as_deref(),
    Some("the host is empty")
  );

  let err = import_file(&state, &path, None, true, DuplicateStrategy::Skip).unwrap_err();
  assert!(
    matches!(&err, Error::Storage { message } if message.contains("\"broken\"")),
    "{err:?}"
  );
  assert!(state.profiles.lock().unwrap().list().is_empty());
}

#[test]
fn a_tunnel_reference_with_no_tunnel_blocks_the_import() {
  let dir = tempfile::tempdir().unwrap();
  let state = app_state(&dir);
  let path = out(&dir);
  std::fs::write(
    &path,
    r#"{
      "soquel": "soquel-connections",
      "version": 1,
      "document": {
        "connections": [{"name": "orphan", "env": "dev", "params": {
          "kind": "postgres", "host": "db", "port": 5432, "database": "app",
          "user": "soquel", "tunnelId": "gone"}}],
        "tunnels": []
      }
    }"#,
  )
  .unwrap();

  let preview = preview_file(&state, &path, None).unwrap();
  assert_eq!(
    preview.connections[0].problem.as_deref(),
    Some("its ssh tunnel is missing from the file")
  );
  assert!(import_file(&state, &path, None, true, DuplicateStrategy::Skip).is_err());
}

#[test]
fn the_preview_flags_what_already_exists() {
  let source_dir = tempfile::tempdir().unwrap();
  let (source, _) = seeded(&source_dir);
  let path = out(&source_dir);
  export(&source, &path, false, None).unwrap();

  let preview = preview_file(&source, &path, None).unwrap();
  assert!(preview.connections.iter().all(|entry| entry.duplicate));
  assert!(preview.tunnels.iter().all(|entry| entry.duplicate));
  assert_eq!(
    preview
      .connections
      .iter()
      .find(|entry| entry.name == "cache")
      .unwrap()
      .target,
    "localhost:6379/2"
  );

  let target_dir = tempfile::tempdir().unwrap();
  let fresh = app_state(&target_dir);
  let preview = preview_file(&fresh, &path, None).unwrap();
  assert!(preview.connections.iter().all(|entry| !entry.duplicate));
}

#[test]
fn a_sqlite_only_export_needs_no_tunnel() {
  let dir = tempfile::tempdir().unwrap();
  let state = app_state(&dir);
  state
    .profiles
    .lock()
    .unwrap()
    .create(&ConnectionInput {
      name: "local file".to_string(),
      env: Env::Dev,
      group: None,
      agent_access: AgentAccess::WriteWithApproval,
      credential: Default::default(),
      params: ConnectorParams::Sqlite {
        path: "/tmp/app.db".to_string(),
      },
      password: None,
    })
    .unwrap();
  let path = out(&dir);
  export(&state, &path, false, None).unwrap();

  let target_dir = tempfile::tempdir().unwrap();
  let receiver = app_state(&target_dir);
  import_file(&receiver, &path, None, true, DuplicateStrategy::Skip).unwrap();
  let imported = &receiver.profiles.lock().unwrap().list()[0];
  assert_eq!(target(&imported.params), "/tmp/app.db");
  assert_eq!(imported.agent_access, AgentAccess::None);
}

/// A keychain that goes away partway through: writes past `allowed` fail.
struct FlakySecrets {
  inner: InMemoryStore,
  allowed: usize,
  writes: std::sync::Mutex<usize>,
}

impl SecretStore for FlakySecrets {
  fn set(&self, key: &SecretKey, secret: &str) -> Result<(), Error> {
    let mut writes = self.writes.lock().unwrap();
    *writes += 1;
    if *writes > self.allowed {
      return Err(Error::Secret {
        message: "keychain: locked".to_string(),
      });
    }
    self.inner.set(key, secret)
  }

  fn get(&self, key: &SecretKey) -> Result<Option<String>, Error> {
    self.inner.get(key)
  }

  fn delete(&self, key: &SecretKey) -> Result<(), Error> {
    self.inner.delete(key)
  }
}

#[test]
fn a_keychain_that_refuses_midway_unwinds_both_stores() {
  let source_dir = tempfile::tempdir().unwrap();
  let (source, _) = seeded(&source_dir);
  let path = out(&source_dir);
  export(&source, &path, true, Some("pass")).unwrap();

  // The tunnel secret lands, the connection secret does not.
  let target_dir = tempfile::tempdir().unwrap();
  let target = app_state_with(
    &target_dir,
    Box::new(FlakySecrets {
      inner: InMemoryStore::default(),
      allowed: 1,
      writes: Default::default(),
    }),
  );
  let kept = target
    .profiles
    .lock()
    .unwrap()
    .create(&ConnectionInput {
      name: "already here".to_string(),
      env: Env::Dev,
      group: None,
      agent_access: AgentAccess::None,
      credential: Default::default(),
      params: ConnectorParams::Sqlite {
        path: "/tmp/kept.db".to_string(),
      },
      password: None,
    })
    .unwrap();

  let err = import_file(&target, &path, Some("pass"), true, DuplicateStrategy::Skip).unwrap_err();
  assert!(matches!(err, Error::Secret { .. }), "{err:?}");

  // What was here before is still here, untouched, and nothing new stuck.
  let profiles = target.profiles.lock().unwrap().list();
  assert_eq!(profiles.len(), 1);
  assert_eq!(profiles[0].id, kept.id);
  assert!(target.tunnels.lock().unwrap().list().is_empty());
  // The rolled-back file on disk agrees with the in-memory store.
  let reloaded = ProfileStore::load(target_dir.path().join("connections.json")).unwrap();
  assert_eq!(reloaded.list().len(), 1);
  assert!(TunnelStore::load(target_dir.path().join("tunnels.json"))
    .unwrap()
    .list()
    .is_empty());
}

#[test]
fn the_preview_never_carries_a_secret_across_the_ipc_boundary() {
  let source_dir = tempfile::tempdir().unwrap();
  let (source, _) = seeded(&source_dir);
  let path = out(&source_dir);
  export(&source, &path, true, Some("pass")).unwrap();

  let target_dir = tempfile::tempdir().unwrap();
  let target = app_state(&target_dir);
  let preview = preview_file(&target, &path, Some("pass")).unwrap();
  let serialized = serde_json::to_string(&preview).unwrap();
  assert!(!serialized.contains("pg-password"), "{serialized}");
  assert!(!serialized.contains("key-passphrase"), "{serialized}");
  assert!(serialized.contains(r#""hasSecret":true"#), "{serialized}");
}

#[test]
fn connections_sharing_one_tunnel_land_on_one_tunnel() {
  let dir = tempfile::tempdir().unwrap();
  let state = app_state(&dir);
  let path = out(&dir);
  std::fs::write(
    &path,
    r#"{
      "soquel": "soquel-connections",
      "version": 1,
      "document": {
        "connections": [
          {"name": "one", "env": "dev", "params": {
            "kind": "postgres", "host": "db", "port": 5432, "database": "app",
            "user": "soquel", "tunnelId": "t1"}},
          {"name": "two", "env": "prod", "params": {
            "kind": "mysql", "host": "db", "port": 3306, "database": "app",
            "user": "soquel", "tunnelId": "t1"}}
        ],
        "tunnels": [{"id": "t1", "name": "bastion", "host": "b.internal",
          "port": 22, "user": "deploy", "auth": {"method": "agent"}}]
      }
    }"#,
  )
  .unwrap();

  let outcome = import_file(&state, &path, None, true, DuplicateStrategy::Skip).unwrap();
  assert_eq!(outcome.created, 2);
  assert_eq!(outcome.tunnels_created, 1);
  let tunnels = state.tunnels.lock().unwrap().list();
  assert_eq!(tunnels.len(), 1);
  let profiles = state.profiles.lock().unwrap().list();
  for profile in &profiles {
    assert_eq!(
      profile.params.remote().unwrap().tunnel_id,
      Some(tunnels[0].id.as_str()),
      "{} lost the shared tunnel",
      profile.name
    );
  }
}

#[test]
fn a_file_that_is_half_new_replaces_and_creates_in_one_pass() {
  let dir = tempfile::tempdir().unwrap();
  let (state, _) = seeded(&dir);
  let path = out(&dir);
  export(&state, &path, false, None).unwrap();
  let cache_id = state
    .profiles
    .lock()
    .unwrap()
    .list()
    .into_iter()
    .find(|profile| profile.name == "cache")
    .unwrap()
    .id;
  state.profiles.lock().unwrap().delete(&cache_id).unwrap();

  let outcome = import_file(&state, &path, None, true, DuplicateStrategy::Replace).unwrap();
  assert_eq!(outcome.replaced, 1);
  assert_eq!(outcome.created, 1);
  assert_eq!(outcome.tunnels_created, 0);
  let profiles = state.profiles.lock().unwrap().list();
  assert_eq!(profiles.len(), 2);
  // The recreated one is a new profile, the replaced one kept its identity.
  assert!(profiles.iter().any(|profile| profile.name == "cache"));
  assert!(profiles.iter().all(|profile| profile.id != cache_id));
}
