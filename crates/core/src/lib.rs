//! Soquel core: connectors, tunnels, credentials, secrets, licence. UI-agnostic.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::Serialize;
use specta::Type;
use tokio_util::sync::CancellationToken;

use crate::command_approvals::CommandApprovalsStore;
use crate::connectors::{Connection, SqlSession};
use crate::credentials::SessionSecrets;
use crate::known_hosts::KnownHostsStore;
use crate::profiles::ProfileStore;
use crate::secrets::SecretStore;
use crate::ssh::SshTunnel;
use crate::tunnels::TunnelStore;

pub mod activation;
pub mod command_approvals;
pub mod connectors;
pub mod credentials;
pub mod error;
pub mod export;
pub mod known_hosts;
pub mod licence;
mod mongo;
mod mysql;
pub mod ops;
mod postgres;
pub mod profiles;
mod redis;
pub mod secrets;
mod sqlite;
pub mod ssh;
pub mod transfer;
pub mod tunnels;

/// A connected database plus the tunnel carrying it: dropped together.
pub struct ActiveConnection {
  pub connection: Arc<dyn Connection>,
  pub _tunnel: Option<SshTunnel>,
}

pub struct SessionEntry {
  pub connection_id: String,
  pub session: Arc<dyn SqlSession>,
}

pub struct McpRunning {
  pub port: u16,
  pub cancel: CancellationToken,
}

/// What the user answered. `ForWindow` also opens a trust window on the
/// connection; every other value, including silence, refuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize, Type)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalAnswer {
  Deny,
  Once,
  ForWindow,
}

/// A live "allow writes for a while" grant. Memory only: it dies with the
/// server, and nothing persists it to disk.
pub struct TrustWindow {
  /// Authority for "still live": monotonic, so a clock change cannot extend it.
  pub expires: Instant,
  /// The same moment as epoch millis, for the panel countdown only.
  pub expires_at_ms: f64,
  pub connection_name: String,
}

pub struct AppState {
  pub profiles: Mutex<ProfileStore>,
  pub tunnels: Mutex<TunnelStore>,
  pub known_hosts: Mutex<KnownHostsStore>,
  pub command_approvals: Mutex<CommandApprovalsStore>,
  pub secrets: Box<dyn SecretStore>,
  /// Probed once at startup: talking to the keyring on every render would be
  /// a D-Bus round trip per keystroke in the form.
  pub secrets_problem: Option<String>,
  pub session_secrets: SessionSecrets,
  pub connections: tokio::sync::Mutex<HashMap<String, ActiveConnection>>,
  pub sessions: tokio::sync::Mutex<HashMap<String, SessionEntry>>,
  pub data_dir: std::path::PathBuf,
  pub mcp: tokio::sync::Mutex<Option<McpRunning>>,
  /// Agent write requests waiting on the approval dialog.
  pub approvals: tokio::sync::Mutex<HashMap<String, tokio::sync::oneshot::Sender<ApprovalAnswer>>>,
  /// Live "allow writes for a while" grants, keyed by (mcp session, connection).
  pub trust_windows: tokio::sync::Mutex<HashMap<(String, String), TrustWindow>>,
}

impl AppState {
  /// Stores rooted in `dir`, nothing connected.
  pub fn load(dir: &std::path::Path, secrets: Box<dyn SecretStore>) -> Result<Self, error::Error> {
    let secrets_problem = secrets.probe().err().map(|err| err.to_string());
    Ok(Self {
      profiles: Mutex::new(ProfileStore::load(dir.join("connections.json"))?),
      tunnels: Mutex::new(TunnelStore::load(dir.join("tunnels.json"))?),
      known_hosts: Mutex::new(KnownHostsStore::load(dir.join("known_hosts.json"))?),
      command_approvals: Mutex::new(CommandApprovalsStore::load(
        dir.join("command_approvals.json"),
      )?),
      secrets,
      secrets_problem,
      session_secrets: SessionSecrets::default(),
      connections: tokio::sync::Mutex::new(HashMap::new()),
      sessions: tokio::sync::Mutex::new(HashMap::new()),
      data_dir: dir.to_path_buf(),
      mcp: tokio::sync::Mutex::new(None),
      approvals: tokio::sync::Mutex::new(HashMap::new()),
      trust_windows: tokio::sync::Mutex::new(HashMap::new()),
    })
  }

  /// Reconnect on the target db and swap the active connection: a SELECT on the
  /// multiplexed socket would silently revert on reconnect.
  pub async fn select_kv_db(&self, id: &str, db: u32) -> Result<(), error::Error> {
    use crate::connectors::{connector_for, LocalForward};
    use crate::credentials::{resolve_credentials, CredentialTarget};
    use crate::profiles::ConnectorParams;

    let mut profile = self.profiles.lock().unwrap().get(id)?;
    let ConnectorParams::Redis(params) = &mut profile.params else {
      return Err(error::Error::Unsupported {
        message: "database selection is a redis feature".to_string(),
      });
    };
    params.db = db;
    let secret = resolve_credentials(self, &CredentialTarget::connection(&profile, id), None)?;
    let forward = {
      let connections = self.connections.lock().await;
      let entry = connections.get(id).ok_or_else(|| error::Error::NotFound {
        message: format!("connection {id} is not active"),
      })?;
      entry._tunnel.as_ref().map(|tunnel| LocalForward {
        port: tunnel.local_port,
      })
    };
    let connection = connector_for(profile.params.kind())
      .connect(&profile, secret, forward)
      .await?;
    let mut connections = self.connections.lock().await;
    let Some(entry) = connections.get_mut(id) else {
      let _ = connection.close().await;
      return Err(error::Error::NotFound {
        message: format!("connection {id} is not active"),
      });
    };
    let old = std::mem::replace(&mut entry.connection, Arc::from(connection));
    drop(connections);
    old.close().await
  }
}

impl AppState {
  /// Stores rooted in `dir`, nothing connected. Tests that need a live
  /// connection go through the connectors themselves. Not cfg(test): dependent
  /// crates' tests need it too.
  pub fn for_tests(dir: &std::path::Path, secrets: Box<dyn SecretStore>) -> Self {
    let mut state = Self::load(dir, secrets).unwrap();
    state.secrets_problem = None;
    state
  }
}
