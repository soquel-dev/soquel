//! Reading connections in from somewhere else, and writing them back out.
//! `file` owns the soquel format; every other source (pgpass, ssh config,
//! DBeaver...) only has to build an [`ImportBundle`] and the engine here does
//! the rest: validation, id remapping, dedupe, forced agent lockout.

pub mod crypto;
pub mod file;
#[cfg(test)]
mod tests;

use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::profiles::{AgentAccess, ConnectionProfile, ConnectorParams, CredentialSource, Env};
use crate::secrets::SecretKey;
use crate::tunnels::{SshAuth, TunnelProfile};
use crate::AppState;

/// A connection on its way in. Ids are deliberately absent: they belong to the
/// store, and two machines can hand us the same one.
pub struct IncomingConnection {
  pub name: String,
  pub env: Env,
  pub group: Option<String>,
  pub credential: CredentialSource,
  /// `tunnel_id` is always empty here: `tunnel_ref` is the link.
  pub params: ConnectorParams,
  pub tunnel_ref: Option<String>,
  pub secret: Option<String>,
}

pub struct IncomingTunnel {
  /// Source-local key the connections' `tunnel_ref` points at.
  pub reference: String,
  pub name: String,
  pub credential: CredentialSource,
  pub host: String,
  pub port: u16,
  pub user: String,
  pub auth: SshAuth,
  pub secret: Option<String>,
}

pub struct ImportBundle {
  pub connections: Vec<IncomingConnection>,
  pub tunnels: Vec<IncomingTunnel>,
}

/// What to do with an entry that already exists here (same name, same target).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DuplicateStrategy {
  Replace,
  KeepBoth,
  Skip,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewEntry {
  pub name: String,
  pub target: String,
  pub has_secret: bool,
  /// Carries a credential command: it will not run before the user approves it.
  pub has_command: bool,
  pub duplicate: bool,
  /// Set when the entry cannot be written; any problem blocks the whole import.
  pub problem: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportPreview {
  pub encrypted: bool,
  /// Encrypted file, no passphrase yet: ask, then preview again.
  pub needs_passphrase: bool,
  pub connections: Vec<PreviewEntry>,
  pub tunnels: Vec<PreviewEntry>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportOutcome {
  pub created: u32,
  pub replaced: u32,
  pub skipped: u32,
  pub tunnels_created: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportSummary {
  pub connections: u32,
  pub tunnels: u32,
  pub secrets: u32,
  pub encrypted: bool,
}

/// One-line identity used for display and for dedupe; mirrors the frontend's
/// `connectionTarget`.
pub fn target(params: &ConnectorParams) -> String {
  match params {
    ConnectorParams::Sqlite { path } => path.clone(),
    ConnectorParams::Redis(params) => format!("{}:{}/{}", params.host, params.port, params.db),
    ConnectorParams::Mongo(params) => match &params.database {
      Some(database) => format!("{}:{}/{database}", params.host, params.port),
      None => format!("{}:{}", params.host, params.port),
    },
    ConnectorParams::Postgres(params) | ConnectorParams::Mysql(params) => {
      format!("{}:{}/{}", params.host, params.port, params.database)
    }
  }
}

fn tunnel_target(tunnel: &TunnelProfile) -> String {
  format!("{}@{}:{}", tunnel.user, tunnel.host, tunnel.port)
}

fn incoming_tunnel_target(tunnel: &IncomingTunnel) -> String {
  format!("{}@{}:{}", tunnel.user, tunnel.host, tunnel.port)
}

/// The same rules the connection form enforces (see `connectionSchema`): a
/// hand-edited file must not be able to write a profile the UI would reject.
fn validate(name: &str, params: &ConnectorParams) -> Option<String> {
  if name.trim().is_empty() {
    return Some("the name is empty".to_string());
  }
  if let ConnectorParams::Sqlite { path } = params {
    return path
      .trim()
      .is_empty()
      .then(|| "the database file is empty".to_string());
  }
  let remote = params.remote().expect("only sqlite has no remote");
  if remote.host.trim().is_empty() {
    return Some("the host is empty".to_string());
  }
  if remote.port == 0 {
    return Some("the port is 0".to_string());
  }
  match params {
    ConnectorParams::Postgres(params) | ConnectorParams::Mysql(params) => {
      if params.database.trim().is_empty() {
        return Some("the database is empty".to_string());
      }
      if params.user.trim().is_empty() {
        return Some("the user is empty".to_string());
      }
      None
    }
    _ => None,
  }
}

fn validate_tunnel(tunnel: &IncomingTunnel) -> Option<String> {
  if tunnel.name.trim().is_empty() {
    return Some("the name is empty".to_string());
  }
  if tunnel.host.trim().is_empty() {
    return Some("the host is empty".to_string());
  }
  if tunnel.port == 0 {
    return Some("the port is 0".to_string());
  }
  if tunnel.user.trim().is_empty() {
    return Some("the user is empty".to_string());
  }
  if let SshAuth::KeyFile { path } = &tunnel.auth {
    if path.trim().is_empty() {
      return Some("the key file is empty".to_string());
    }
  }
  None
}

/// Read-only pass over a bundle: what would land, what already exists, what
/// blocks. Never writes.
pub fn preview(
  bundle: &ImportBundle,
  existing_connections: &[ConnectionProfile],
  existing_tunnels: &[TunnelProfile],
) -> ImportPreview {
  let refs: Vec<&str> = bundle
    .tunnels
    .iter()
    .map(|tunnel| tunnel.reference.as_str())
    .collect();
  ImportPreview {
    encrypted: false,
    needs_passphrase: false,
    connections: bundle
      .connections
      .iter()
      .map(|entry| {
        let dangling = entry
          .tunnel_ref
          .as_deref()
          .is_some_and(|reference| !refs.contains(&reference));
        PreviewEntry {
          name: entry.name.clone(),
          target: target(&entry.params),
          has_secret: entry.secret.is_some(),
          has_command: is_command(&entry.credential),
          duplicate: existing_connections
            .iter()
            .any(|existing| is_same_connection(existing, entry)),
          problem: validate(&entry.name, &entry.params)
            .or_else(|| dangling.then(|| "its ssh tunnel is missing from the file".to_string())),
        }
      })
      .collect(),
    tunnels: bundle
      .tunnels
      .iter()
      .map(|entry| PreviewEntry {
        name: entry.name.clone(),
        target: incoming_tunnel_target(entry),
        has_secret: entry.secret.is_some(),
        has_command: is_command(&entry.credential),
        duplicate: existing_tunnels
          .iter()
          .any(|existing| is_same_tunnel(existing, entry)),
        problem: validate_tunnel(entry),
      })
      .collect(),
  }
}

fn is_command(credential: &CredentialSource) -> bool {
  matches!(credential, CredentialSource::Command { .. })
}

fn is_same_connection(existing: &ConnectionProfile, incoming: &IncomingConnection) -> bool {
  existing.name == incoming.name
    && existing.params.kind() == incoming.params.kind()
    && target(&existing.params) == target(&incoming.params)
}

fn is_same_tunnel(existing: &TunnelProfile, incoming: &IncomingTunnel) -> bool {
  existing.name == incoming.name && tunnel_target(existing) == incoming_tunnel_target(incoming)
}

/// Free variant of a colliding name, so "keep both" stays readable.
fn free_name(taken: &[String], name: &str) -> String {
  let candidate = format!("{name} (imported)");
  if !taken.contains(&candidate) {
    return candidate;
  }
  (2..)
    .map(|n| format!("{name} (imported {n})"))
    .find(|candidate| !taken.contains(candidate))
    .expect("an unbounded range always yields a free name")
}

struct PlannedSecret {
  key: SecretKey,
  secret: String,
}

/// Validates everything, then writes: one save per store, so a refused bundle
/// leaves the stores untouched. Ids are minted here and `tunnel_ref` is
/// resolved against them; `agent_access` is forced off whatever the source says.
pub fn apply(
  state: &AppState,
  bundle: &ImportBundle,
  strategy: DuplicateStrategy,
) -> Result<ImportOutcome, Error> {
  let mut profiles = state.profiles.lock().unwrap();
  let mut tunnels = state.tunnels.lock().unwrap();
  let profiles_before = profiles.list();
  let tunnels_before = tunnels.list();

  let plan = preview(bundle, &profiles_before, &tunnels_before);
  if let Some(entry) = plan
    .connections
    .iter()
    .chain(plan.tunnels.iter())
    .find(|entry| entry.problem.is_some())
  {
    return Err(Error::Storage {
      message: format!(
        "\"{}\" cannot be imported: {}",
        entry.name,
        entry.problem.as_deref().unwrap_or_default()
      ),
    });
  }

  let mut outcome = ImportOutcome::default();
  let mut secrets = Vec::new();
  let mut next_tunnels = tunnels_before.clone();
  // Source reference -> the tunnel id it ended up on here.
  let mut tunnel_ids: std::collections::HashMap<&str, String> = Default::default();

  for entry in &bundle.tunnels {
    let existing = tunnels_before
      .iter()
      .find(|existing| is_same_tunnel(existing, entry));
    let id = match (existing, strategy) {
      (Some(existing), DuplicateStrategy::Skip) => existing.id.clone(),
      (Some(existing), DuplicateStrategy::Replace) => {
        let id = existing.id.clone();
        let taken = next_tunnels
          .iter_mut()
          .find(|candidate| candidate.id == id)
          .expect("the snapshot holds every existing tunnel");
        taken.name = entry.name.clone();
        taken.host = entry.host.clone();
        taken.port = entry.port;
        taken.user = entry.user.clone();
        taken.auth = entry.auth.clone();
        taken.credential = entry.credential.clone();
        id
      }
      (existing, _) => {
        let id = uuid::Uuid::new_v4().to_string();
        let taken: Vec<String> = next_tunnels
          .iter()
          .map(|tunnel| tunnel.name.clone())
          .collect();
        next_tunnels.push(TunnelProfile {
          id: id.clone(),
          name: if existing.is_some() {
            free_name(&taken, &entry.name)
          } else {
            entry.name.clone()
          },
          host: entry.host.clone(),
          port: entry.port,
          user: entry.user.clone(),
          auth: entry.auth.clone(),
          credential: entry.credential.clone(),
        });
        outcome.tunnels_created += 1;
        id
      }
    };
    if let Some(secret) = &entry.secret {
      // Skipped duplicates keep the credential already stored here.
      if !(existing.is_some() && strategy == DuplicateStrategy::Skip) {
        secrets.push(PlannedSecret {
          key: SecretKey::Tunnel(id.clone()),
          secret: secret.clone(),
        });
      }
    }
    tunnel_ids.insert(entry.reference.as_str(), id);
  }

  let mut next_profiles = profiles_before.clone();
  for entry in &bundle.connections {
    let existing = profiles_before
      .iter()
      .find(|existing| is_same_connection(existing, entry));
    if existing.is_some() && strategy == DuplicateStrategy::Skip {
      outcome.skipped += 1;
      continue;
    }
    let mut params = entry.params.clone();
    params.set_tunnel_id(
      entry
        .tunnel_ref
        .as_deref()
        .and_then(|reference| tunnel_ids.get(reference).cloned()),
    );
    let id = match (existing, strategy) {
      (Some(existing), DuplicateStrategy::Replace) => {
        let id = existing.id.clone();
        let taken = next_profiles
          .iter_mut()
          .find(|candidate| candidate.id == id)
          .expect("the snapshot holds every existing connection");
        taken.name = entry.name.clone();
        taken.env = entry.env;
        taken.group = entry.group.clone();
        taken.credential = entry.credential.clone();
        taken.params = params;
        // Replacing must not silently keep an agent grant either.
        taken.agent_access = AgentAccess::None;
        outcome.replaced += 1;
        id
      }
      (existing, _) => {
        let id = uuid::Uuid::new_v4().to_string();
        let taken: Vec<String> = next_profiles
          .iter()
          .map(|profile| profile.name.clone())
          .collect();
        next_profiles.push(ConnectionProfile {
          id: id.clone(),
          name: if existing.is_some() {
            free_name(&taken, &entry.name)
          } else {
            entry.name.clone()
          },
          env: entry.env,
          group: entry.group.clone(),
          // An imported file never grants agent access: opting in stays a
          // deliberate gesture in the app.
          agent_access: AgentAccess::None,
          credential: entry.credential.clone(),
          params,
        });
        outcome.created += 1;
        id
      }
    };
    if let Some(secret) = &entry.secret {
      secrets.push(PlannedSecret {
        key: SecretKey::Connection(id),
        secret: secret.clone(),
      });
    }
  }

  tunnels.replace_all(next_tunnels)?;
  if let Err(err) = profiles.replace_all(next_profiles) {
    let _ = tunnels.replace_all(tunnels_before);
    return Err(err);
  }
  for planned in &secrets {
    // A keychain that refuses halfway would leave profiles without their
    // password: unwind both stores instead.
    if let Err(err) = state.secrets.set(&planned.key, &planned.secret) {
      let _ = profiles.replace_all(profiles_before);
      let _ = tunnels.replace_all(tunnels_before);
      return Err(err);
    }
  }
  Ok(outcome)
}

/// Everything in the stores, with the secrets only when asked for.
pub fn export(
  state: &AppState,
  path: &std::path::Path,
  include_secrets: bool,
  passphrase: Option<&str>,
) -> Result<ExportSummary, Error> {
  if include_secrets && passphrase.map(str::trim).unwrap_or_default().is_empty() {
    return Err(Error::Secret {
      message: "a passphrase is required to export passwords".to_string(),
    });
  }
  let profiles = state.profiles.lock().unwrap().list();
  let tunnels = state.tunnels.lock().unwrap().list();
  let mut secret_count = 0;
  let mut connection_entries = Vec::with_capacity(profiles.len());
  for profile in &profiles {
    let secret = match include_secrets {
      true => state
        .secrets
        .get(&SecretKey::Connection(profile.id.clone()))?,
      false => None,
    };
    secret_count += u32::from(secret.is_some());
    connection_entries.push(file::ExportEntry { profile, secret });
  }
  let mut tunnel_entries = Vec::with_capacity(tunnels.len());
  for tunnel in &tunnels {
    let secret = match include_secrets {
      true => state.secrets.get(&SecretKey::Tunnel(tunnel.id.clone()))?,
      false => None,
    };
    secret_count += u32::from(secret.is_some());
    tunnel_entries.push(file::ExportTunnel { tunnel, secret });
  }
  // No secrets in the file, no reason to lock the user out of reading it.
  let passphrase = include_secrets.then_some(passphrase).flatten();
  file::write(path, connection_entries, tunnel_entries, passphrase)?;
  Ok(ExportSummary {
    connections: profiles.len() as u32,
    tunnels: tunnels.len() as u32,
    secrets: secret_count,
    encrypted: passphrase.is_some(),
  })
}

/// Read a soquel file and describe what importing it would do.
pub fn preview_file(
  state: &AppState,
  path: &std::path::Path,
  passphrase: Option<&str>,
) -> Result<ImportPreview, Error> {
  let read = file::read(path, passphrase)?;
  let Some(bundle) = read.bundle else {
    return Ok(ImportPreview {
      encrypted: true,
      needs_passphrase: true,
      connections: Vec::new(),
      tunnels: Vec::new(),
    });
  };
  let profiles = state.profiles.lock().unwrap().list();
  let tunnels = state.tunnels.lock().unwrap().list();
  let mut preview = preview(&bundle, &profiles, &tunnels);
  preview.encrypted = read.encrypted;
  Ok(preview)
}

pub fn import_file(
  state: &AppState,
  path: &std::path::Path,
  passphrase: Option<&str>,
  with_secrets: bool,
  strategy: DuplicateStrategy,
) -> Result<ImportOutcome, Error> {
  let mut bundle = file::read(path, passphrase)?
    .bundle
    .ok_or_else(|| Error::Secret {
      message: "this file is encrypted: a passphrase is required".to_string(),
    })?;
  // Decrypting the file is not the same as agreeing to take what is inside:
  // a password only crosses over when it was asked for.
  if !with_secrets {
    for connection in &mut bundle.connections {
      connection.secret = None;
    }
    for tunnel in &mut bundle.tunnels {
      tunnel.secret = None;
    }
  }
  apply(state, &bundle, strategy)
}
