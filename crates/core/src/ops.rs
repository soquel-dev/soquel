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
