use std::sync::Arc;

use tauri::State;

use soquel_core::connectors::{
  connector_for, ApplyResult, Capability, Connection, DocBrowse, DocCollection, DocCount,
  DocDatabase, DocDetail, DocFindRequest, DocPage, DocQueryResult, IndexInfo, KeyDetail,
  KeyScanPage, KvBrowse, KvDatabases, LocalForward, QueryColumn, QueryResult, RowsChunk,
  SchemaSnapshot, SqlQuery, SqlSession, StreamSummary, TableChanges, TableRowsRequest,
};
use soquel_core::credentials::{self, resolve_credentials, CredentialTarget};
use soquel_core::error::{Error, SecretSubject};
use soquel_core::export::{quote_ident, ExportFormat, ExportWriter};
use soquel_core::profiles::{ConnectionInput, ConnectionProfile, ConnectorKind, CredentialSource};
use soquel_core::secrets::SecretKey;
use soquel_core::ssh::{self, SshTunnel, TunnelTarget};
use soquel_core::transfer::{self, DuplicateStrategy, ExportSummary, ImportOutcome, ImportPreview};
use soquel_core::tunnels::{TunnelInput, TunnelProfile};
use soquel_core::{ActiveConnection, AppState, SessionEntry};

/// Resolve a profile's tunnel (if any); the returned forward tells the
/// connector where TCP actually goes while the profile keeps the logical host.
async fn open_tunnel(
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

#[tauri::command]
#[specta::specta]
pub fn connector_capabilities(kind: ConnectorKind) -> Result<Vec<Capability>, Error> {
  Ok(connector_for(kind).capabilities().to_vec())
}

/// Ephemeral connect + health check; never touches the active connections.
#[tauri::command]
#[specta::specta]
pub async fn test_connection(
  state: State<'_, AppState>,
  input: ConnectionInput,
  existing_id: Option<String>,
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
  let target = CredentialTarget::connection(&profile, existing_id.as_deref().unwrap_or_default());
  let secret = resolve_credentials(state.inner(), &target, input.password.clone())?;
  let result = test_run(&state, &profile, secret).await;
  // A password typed to test an unsaved profile has no connection to outlive.
  state.session_secrets.clear_one_shot(&target.key);
  result
}

async fn test_run(
  state: &AppState,
  profile: &ConnectionProfile,
  secret: Arc<soquel_core::credentials::Credentials>,
) -> Result<(), Error> {
  let opened = open_tunnel(state, profile).await?;
  let connection = connector_for(profile.params.kind())
    .connect(profile, secret, opened.as_ref().map(|(_, f)| *f))
    .await?;
  connection.health().await?;
  connection.close().await
}

#[tauri::command]
#[specta::specta]
pub async fn connect(state: State<'_, AppState>, id: String) -> Result<(), Error> {
  connect_impl(state.inner(), id).await
}

pub(crate) async fn connect_impl(state: &AppState, id: String) -> Result<(), Error> {
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
fn tunnel_of(state: &AppState, id: &str) -> Option<String> {
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

/// Hands the core a password for whatever asked for one at connect time.
/// Memory only: `remember` keeps it until disconnect, otherwise it dies with
/// the next attempt.
#[tauri::command]
#[specta::specta]
pub fn unlock_secret(
  state: State<'_, AppState>,
  subject: SecretSubject,
  id: String,
  secret: String,
  remember: bool,
) -> Result<(), Error> {
  state
    .session_secrets
    .set(&key_for(subject, id), secret, remember);
  Ok(())
}

fn key_for(subject: SecretSubject, id: String) -> SecretKey {
  match subject {
    SecretSubject::Connection => SecretKey::Connection(id),
    SecretSubject::Tunnel => SecretKey::Tunnel(id),
  }
}

/// Agrees to run the credential command a profile carries, as it stands now.
/// The approval is local and dies with any later edit of the command.
#[tauri::command]
#[specta::specta]
pub fn approve_credential_command(
  state: State<'_, AppState>,
  subject: SecretSubject,
  id: String,
) -> Result<(), Error> {
  let key = key_for(subject, id);
  let command = current_command(state.inner(), &key)?;
  state
    .command_approvals
    .lock()
    .unwrap()
    .approve(&key, &command)
}

#[tauri::command]
#[specta::specta]
pub fn revoke_credential_command(
  state: State<'_, AppState>,
  subject: SecretSubject,
  id: String,
) -> Result<(), Error> {
  state
    .command_approvals
    .lock()
    .unwrap()
    .revoke(&key_for(subject, id))
}

/// Reads the command off the stored profile: approving what the webview echoes
/// back would approve whatever it says instead of what will run.
fn current_command(state: &AppState, key: &SecretKey) -> Result<String, Error> {
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

/// Splits a credential command the way the core will run it, so the form can
/// show the argv instead of guessing at it.
#[tauri::command]
#[specta::specta]
pub fn parse_credential_command(line: String) -> Result<Vec<String>, Error> {
  let spec = credentials::parse_command(&line)?;
  Ok(std::iter::once(spec.program).chain(spec.args).collect())
}

#[tauri::command]
#[specta::specta]
pub async fn disconnect(state: State<'_, AppState>, id: String) -> Result<(), Error> {
  disconnect_impl(state.inner(), &id).await
}

pub(crate) async fn disconnect_impl(state: &AppState, id: &str) -> Result<(), Error> {
  let orphaned: Vec<Arc<dyn SqlSession>> = {
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

#[tauri::command]
#[specta::specta]
pub async fn open_sql_session(
  state: State<'_, AppState>,
  connection_id: String,
) -> Result<String, Error> {
  let connection = active(&state, &connection_id).await?;
  let session = sql_surface(&connection)?.open_session().await?;
  let id = uuid::Uuid::new_v4().to_string();
  state.sessions.lock().await.insert(
    id.clone(),
    SessionEntry {
      connection_id,
      session: session.into(),
    },
  );
  Ok(id)
}

#[tauri::command]
#[specta::specta]
pub async fn run_session_query(
  state: State<'_, AppState>,
  id: String,
  sql: String,
) -> Result<QueryResult, Error> {
  session(&state, &id).await?.run_query(&sql).await
}

#[tauri::command]
#[specta::specta]
pub async fn cancel_session_query(state: State<'_, AppState>, id: String) -> Result<(), Error> {
  session(&state, &id).await?.cancel().await
}

#[tauri::command]
#[specta::specta]
pub async fn close_sql_session(state: State<'_, AppState>, id: String) -> Result<(), Error> {
  let entry = state.sessions.lock().await.remove(&id);
  match entry {
    Some(entry) => entry.session.close().await,
    None => Ok(()),
  }
}

// Clone the Arc out so queries never hold the map lock.
async fn session(state: &State<'_, AppState>, id: &str) -> Result<Arc<dyn SqlSession>, Error> {
  state
    .sessions
    .lock()
    .await
    .get(id)
    .map(|entry| entry.session.clone())
    .ok_or_else(|| Error::NotFound {
      message: format!("sql session {id} is not open"),
    })
}

#[tauri::command]
#[specta::specta]
pub async fn active_connections(state: State<'_, AppState>) -> Result<Vec<String>, Error> {
  Ok(state.connections.lock().await.keys().cloned().collect())
}

#[tauri::command]
#[specta::specta]
pub async fn server_version(
  state: State<'_, AppState>,
  id: String,
) -> Result<Option<String>, Error> {
  Ok(active(&state, &id).await?.server_version())
}

#[tauri::command]
#[specta::specta]
pub async fn run_query(
  state: State<'_, AppState>,
  id: String,
  sql: String,
) -> Result<QueryResult, Error> {
  let connection = active(&state, &id).await?;
  sql_surface(&connection)?.run_query(&sql).await
}

#[tauri::command]
#[specta::specta]
pub async fn cancel_query(state: State<'_, AppState>, id: String) -> Result<(), Error> {
  let connection = active(&state, &id).await?;
  sql_surface(&connection)?.cancel().await
}

#[tauri::command]
#[specta::specta]
pub async fn table_rows(
  state: State<'_, AppState>,
  id: String,
  request: TableRowsRequest,
) -> Result<QueryResult, Error> {
  let connection = active(&state, &id).await?;
  sql_surface(&connection)?.table_rows(&request).await
}

#[tauri::command]
#[specta::specta]
pub async fn stream_table_rows(
  state: State<'_, AppState>,
  id: String,
  request: TableRowsRequest,
  channel: tauri::ipc::Channel<RowsChunk>,
) -> Result<StreamSummary, Error> {
  let connection = active(&state, &id).await?;
  sql_surface(&connection)?
    .stream_rows(&request, Box::new(move |chunk| channel.send(chunk).is_ok()))
    .await
}

#[derive(Debug, Clone, serde::Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct ExportProgress {
  pub rows: f64,
}

/// Streams the full filtered/sorted table to a file; rows never enter the
/// webview. Cancel = `cancel_query` on the same connection; a canceled or
/// failed export removes the partial file.
#[tauri::command]
#[specta::specta]
pub async fn export_table_rows(
  state: State<'_, AppState>,
  id: String,
  request: TableRowsRequest,
  format: ExportFormat,
  path: String,
  channel: tauri::ipc::Channel<ExportProgress>,
) -> Result<StreamSummary, Error> {
  let kind = state.profiles.lock().unwrap().get(&id)?.params.kind();
  let connection = active(&state, &id).await?;
  soquel_core::export::run_export(
    sql_surface(&connection)?,
    &request,
    format,
    kind,
    &path,
    move |rows| {
      let _ = channel.send(ExportProgress { rows: rows as f64 });
    },
  )
  .await
}

/// Materialized results (SQL editor); the grid path is `export_table_rows`.
#[tauri::command]
#[specta::specta]
pub fn export_statement(
  columns: Vec<QueryColumn>,
  rows: Vec<Vec<Option<String>>>,
  format: ExportFormat,
  kind: ConnectorKind,
  table: String,
  path: String,
) -> Result<(), Error> {
  let file = std::io::BufWriter::new(std::fs::File::create(&path)?);
  let mut writer = ExportWriter::new(file, format, kind, columns, quote_ident(kind, &table))?;
  for row in &rows {
    writer.row(row)?;
  }
  writer.finish()?;
  Ok(())
}

/// Clipboard copy: same formats, returned as a string.
#[tauri::command]
#[specta::specta]
pub fn format_statement(
  columns: Vec<QueryColumn>,
  rows: Vec<Vec<Option<String>>>,
  format: ExportFormat,
  kind: ConnectorKind,
  table: String,
) -> Result<String, Error> {
  let mut out = Vec::new();
  let mut writer = ExportWriter::new(&mut out, format, kind, columns, quote_ident(kind, &table))?;
  for row in &rows {
    writer.row(row)?;
  }
  writer.finish()?;
  Ok(String::from_utf8(out).expect("formats emit utf-8"))
}

#[tauri::command]
#[specta::specta]
pub async fn apply_table_changes(
  state: State<'_, AppState>,
  id: String,
  changes: TableChanges,
) -> Result<ApplyResult, Error> {
  let connection = active(&state, &id).await?;
  sql_surface(&connection)?.apply_changes(&changes).await
}

#[tauri::command]
#[specta::specta]
pub async fn schema_snapshot(
  state: State<'_, AppState>,
  id: String,
) -> Result<SchemaSnapshot, Error> {
  let connection = active(&state, &id).await?;
  introspect_surface(&connection)?.schema_snapshot().await
}

#[tauri::command]
#[specta::specta]
pub async fn table_ddl(
  state: State<'_, AppState>,
  id: String,
  schema: String,
  table: String,
) -> Result<String, Error> {
  let connection = active(&state, &id).await?;
  introspect_surface(&connection)?
    .table_ddl(&schema, &table)
    .await
}

fn introspect_surface(
  connection: &Arc<dyn Connection>,
) -> Result<&dyn soquel_core::connectors::Introspect, Error> {
  connection.introspect().ok_or_else(|| Error::Unsupported {
    message: "this connection does not support schema introspection".to_string(),
  })
}

// Clone the Arc out so queries never hold the map lock.
pub(crate) async fn active(state: &AppState, id: &str) -> Result<Arc<dyn Connection>, Error> {
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

fn sql_surface(connection: &Arc<dyn Connection>) -> Result<&dyn SqlQuery, Error> {
  connection.sql().ok_or_else(|| Error::Unsupported {
    message: "this connection does not support sql queries".to_string(),
  })
}

fn kv_surface(connection: &Arc<dyn Connection>) -> Result<&dyn KvBrowse, Error> {
  connection.kv().ok_or_else(|| Error::Unsupported {
    message: "this connection does not browse keys".to_string(),
  })
}

fn doc_surface(connection: &Arc<dyn Connection>) -> Result<&dyn DocBrowse, Error> {
  connection.doc().ok_or_else(|| Error::Unsupported {
    message: "this connection does not browse documents".to_string(),
  })
}

#[tauri::command]
#[specta::specta]
pub async fn scan_keys(
  state: State<'_, AppState>,
  id: String,
  pattern: String,
  cursor: Option<String>,
  count: u32,
) -> Result<KeyScanPage, Error> {
  let connection = active(&state, &id).await?;
  kv_surface(&connection)?
    .scan_keys(&pattern, cursor.as_deref(), count)
    .await
}

#[tauri::command]
#[specta::specta]
pub async fn key_detail(
  state: State<'_, AppState>,
  id: String,
  key: String,
) -> Result<KeyDetail, Error> {
  let connection = active(&state, &id).await?;
  kv_surface(&connection)?.key_detail(&key).await
}

#[tauri::command]
#[specta::specta]
pub async fn kv_set_string(
  state: State<'_, AppState>,
  id: String,
  key: String,
  value: String,
) -> Result<(), Error> {
  let connection = active(&state, &id).await?;
  kv_surface(&connection)?.set_string(&key, &value).await
}

#[tauri::command]
#[specta::specta]
pub async fn kv_delete_key(
  state: State<'_, AppState>,
  id: String,
  key: String,
) -> Result<(), Error> {
  let connection = active(&state, &id).await?;
  kv_surface(&connection)?.delete_key(&key).await
}

#[tauri::command]
#[specta::specta]
pub async fn kv_set_ttl(
  state: State<'_, AppState>,
  id: String,
  key: String,
  ttl_ms: Option<f64>,
) -> Result<(), Error> {
  let connection = active(&state, &id).await?;
  kv_surface(&connection)?.set_ttl(&key, ttl_ms).await
}

#[tauri::command]
#[specta::specta]
pub async fn kv_run_command(
  state: State<'_, AppState>,
  id: String,
  command: String,
) -> Result<Vec<String>, Error> {
  let connection = active(&state, &id).await?;
  kv_surface(&connection)?.run_command(&command).await
}

#[tauri::command]
#[specta::specta]
pub async fn kv_databases(state: State<'_, AppState>, id: String) -> Result<KvDatabases, Error> {
  let connection = active(&state, &id).await?;
  kv_surface(&connection)?.databases().await
}

#[tauri::command]
#[specta::specta]
pub async fn kv_select_db(state: State<'_, AppState>, id: String, db: u32) -> Result<(), Error> {
  state.select_kv_db(&id, db).await
}

#[tauri::command]
#[specta::specta]
pub async fn doc_databases(
  state: State<'_, AppState>,
  id: String,
) -> Result<Vec<DocDatabase>, Error> {
  let connection = active(&state, &id).await?;
  doc_surface(&connection)?.databases().await
}

#[tauri::command]
#[specta::specta]
pub async fn doc_collections(
  state: State<'_, AppState>,
  id: String,
  db: String,
) -> Result<Vec<DocCollection>, Error> {
  let connection = active(&state, &id).await?;
  doc_surface(&connection)?.collections(&db).await
}

#[tauri::command]
#[specta::specta]
pub async fn doc_find(
  state: State<'_, AppState>,
  id: String,
  request: DocFindRequest,
) -> Result<DocPage, Error> {
  let connection = active(&state, &id).await?;
  doc_surface(&connection)?.find_docs(&request).await
}

#[tauri::command]
#[specta::specta]
pub async fn doc_detail(
  state: State<'_, AppState>,
  id: String,
  db: String,
  collection: String,
  doc_id: String,
) -> Result<DocDetail, Error> {
  let connection = active(&state, &id).await?;
  doc_surface(&connection)?
    .doc_detail(&db, &collection, &doc_id)
    .await
}

#[tauri::command]
#[specta::specta]
pub async fn doc_replace(
  state: State<'_, AppState>,
  id: String,
  db: String,
  collection: String,
  doc_id: String,
  doc: String,
) -> Result<(), Error> {
  let connection = active(&state, &id).await?;
  doc_surface(&connection)?
    .replace_doc(&db, &collection, &doc_id, &doc)
    .await
}

#[tauri::command]
#[specta::specta]
pub async fn doc_delete(
  state: State<'_, AppState>,
  id: String,
  db: String,
  collection: String,
  doc_id: String,
) -> Result<(), Error> {
  let connection = active(&state, &id).await?;
  doc_surface(&connection)?
    .delete_doc(&db, &collection, &doc_id)
    .await
}

#[tauri::command]
#[specta::specta]
pub async fn doc_indexes(
  state: State<'_, AppState>,
  id: String,
  db: String,
  collection: String,
) -> Result<Vec<IndexInfo>, Error> {
  let connection = active(&state, &id).await?;
  doc_surface(&connection)?.indexes(&db, &collection).await
}

#[tauri::command]
#[specta::specta]
pub async fn doc_count(
  state: State<'_, AppState>,
  id: String,
  db: String,
  collection: String,
  filter: Option<String>,
) -> Result<DocCount, Error> {
  let connection = active(&state, &id).await?;
  doc_surface(&connection)?
    .count_docs(&db, &collection, filter.as_deref())
    .await
}

#[tauri::command]
#[specta::specta]
pub async fn doc_run_query(
  state: State<'_, AppState>,
  id: String,
  db: String,
  collection: String,
  source: String,
) -> Result<DocQueryResult, Error> {
  let connection = active(&state, &id).await?;
  doc_surface(&connection)?
    .run_query(&db, &collection, &source)
    .await
}

#[tauri::command]
#[specta::specta]
pub fn list_connections(state: State<'_, AppState>) -> Result<Vec<ConnectionProfile>, Error> {
  Ok(state.profiles.lock().unwrap().list())
}

#[tauri::command]
#[specta::specta]
pub fn get_connection(state: State<'_, AppState>, id: String) -> Result<ConnectionProfile, Error> {
  state.profiles.lock().unwrap().get(&id)
}

/// Keeps the stores in step with the mode: only `keychain` stores a password,
/// and leaving it wipes what was stored for something that no longer keeps one.
pub(crate) fn persist_secret(
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
fn approve_own_command(
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

#[tauri::command]
#[specta::specta]
pub fn create_connection(
  state: State<'_, AppState>,
  input: ConnectionInput,
) -> Result<ConnectionProfile, Error> {
  let profile = state.profiles.lock().unwrap().create(&input)?;
  // No orphan profile when the keychain is unavailable.
  let key = SecretKey::Connection(profile.id.clone());
  if let Err(err) = persist_secret(
    state.inner(),
    &key,
    &profile.credential,
    input.password.as_deref(),
  ) {
    let _ = state.profiles.lock().unwrap().delete(&profile.id);
    return Err(err);
  }
  approve_own_command(state.inner(), &key, &profile.credential)?;
  Ok(profile)
}

#[tauri::command]
#[specta::specta]
pub fn update_connection(
  state: State<'_, AppState>,
  id: String,
  input: ConnectionInput,
) -> Result<ConnectionProfile, Error> {
  let profile = state.profiles.lock().unwrap().update(&id, &input)?;
  let key = SecretKey::Connection(profile.id.clone());
  persist_secret(
    state.inner(),
    &key,
    &profile.credential,
    input.password.as_deref(),
  )?;
  approve_own_command(state.inner(), &key, &profile.credential)?;
  Ok(profile)
}

#[tauri::command]
#[specta::specta]
pub fn delete_connection(state: State<'_, AppState>, id: String) -> Result<(), Error> {
  state.profiles.lock().unwrap().delete(&id)?;
  forget(state.inner(), &SecretKey::Connection(id))
}

/// Everything kept next to a profile rather than in it, dropped with it.
fn forget(state: &AppState, key: &SecretKey) -> Result<(), Error> {
  state.command_approvals.lock().unwrap().revoke(key)?;
  state.session_secrets.clear(key);
  state.secrets.delete(key)
}

/// Writes every connection, tunnel and group to a file. Passwords ride along
/// only with `include_secrets`, and only inside a passphrase-encrypted payload.
#[tauri::command]
#[specta::specta]
pub fn export_connections(
  state: State<'_, AppState>,
  path: String,
  include_secrets: bool,
  passphrase: Option<String>,
) -> Result<ExportSummary, Error> {
  transfer::export(
    state.inner(),
    std::path::Path::new(&path),
    include_secrets,
    passphrase.as_deref(),
  )
}

/// A file handed to the app from outside the webview: the OS opening a
/// `.soquel`, or a path dropped on the window. The UI answers by opening the
/// import dialog on it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, specta::Type, tauri_specta::Event)]
#[serde(rename_all = "camelCase")]
pub struct ImportFileRequested {
  pub path: String,
}

/// Hands a connections file to the UI, which opens the import dialog on it.
/// The entry point for a `.soquel` opened from the OS or dropped on the window.
#[tauri::command]
#[specta::specta]
pub fn open_connections_file(app: tauri::AppHandle, path: String) -> Result<(), Error> {
  if !std::path::Path::new(&path).is_file() {
    return Err(Error::NotFound {
      message: format!("{path} is not a file"),
    });
  }
  use tauri_specta::Event;
  ImportFileRequested { path }
    .emit(&app)
    .map_err(|err| Error::Storage {
      message: format!("could not hand the file to the window: {err}"),
    })
}

/// What importing that file would do; writes nothing.
#[tauri::command]
#[specta::specta]
pub fn preview_connection_import(
  state: State<'_, AppState>,
  path: String,
  passphrase: Option<String>,
) -> Result<ImportPreview, Error> {
  transfer::preview_file(
    state.inner(),
    std::path::Path::new(&path),
    passphrase.as_deref(),
  )
}

#[tauri::command]
#[specta::specta]
pub fn import_connections(
  state: State<'_, AppState>,
  path: String,
  passphrase: Option<String>,
  with_secrets: bool,
  strategy: DuplicateStrategy,
) -> Result<ImportOutcome, Error> {
  transfer::import_file(
    state.inner(),
    std::path::Path::new(&path),
    passphrase.as_deref(),
    with_secrets,
    strategy,
  )
}

#[tauri::command]
#[specta::specta]
pub fn list_tunnels(state: State<'_, AppState>) -> Result<Vec<TunnelProfile>, Error> {
  Ok(state.tunnels.lock().unwrap().list())
}

#[tauri::command]
#[specta::specta]
pub fn get_tunnel(state: State<'_, AppState>, id: String) -> Result<TunnelProfile, Error> {
  state.tunnels.lock().unwrap().get(&id)
}

#[tauri::command]
#[specta::specta]
pub fn create_tunnel(
  state: State<'_, AppState>,
  input: TunnelInput,
) -> Result<TunnelProfile, Error> {
  let tunnel = state.tunnels.lock().unwrap().create(&input)?;
  // No orphan tunnel when the keychain is unavailable.
  let key = SecretKey::Tunnel(tunnel.id.clone());
  if let Err(err) = persist_secret(
    state.inner(),
    &key,
    &tunnel.credential,
    input.secret.as_deref(),
  ) {
    let _ = state.tunnels.lock().unwrap().delete(&tunnel.id);
    return Err(err);
  }
  approve_own_command(state.inner(), &key, &tunnel.credential)?;
  Ok(tunnel)
}

#[tauri::command]
#[specta::specta]
pub fn update_tunnel(
  state: State<'_, AppState>,
  id: String,
  input: TunnelInput,
) -> Result<TunnelProfile, Error> {
  let tunnel = state.tunnels.lock().unwrap().update(&id, &input)?;
  let key = SecretKey::Tunnel(tunnel.id.clone());
  persist_secret(
    state.inner(),
    &key,
    &tunnel.credential,
    input.secret.as_deref(),
  )?;
  approve_own_command(state.inner(), &key, &tunnel.credential)?;
  Ok(tunnel)
}

#[tauri::command]
#[specta::specta]
pub fn delete_tunnel(state: State<'_, AppState>, id: String) -> Result<(), Error> {
  let used_by: Vec<String> = state
    .profiles
    .lock()
    .unwrap()
    .list()
    .into_iter()
    .filter(|p| p.params.remote().and_then(|remote| remote.tunnel_id) == Some(id.as_str()))
    .map(|p| p.name)
    .collect();
  if !used_by.is_empty() {
    return Err(Error::Storage {
      message: format!("tunnel is used by {}", used_by.join(", ")),
    });
  }
  state.tunnels.lock().unwrap().delete(&id)?;
  forget(state.inner(), &SecretKey::Tunnel(id))
}

/// Ephemeral tunnel bring-up: validates the host key and the credentials
/// without touching a database (no channel is opened until a client connects).
#[tauri::command]
#[specta::specta]
pub async fn test_tunnel(
  state: State<'_, AppState>,
  input: TunnelInput,
  existing_id: Option<String>,
) -> Result<(), Error> {
  let tunnel = TunnelProfile {
    id: String::new(),
    name: input.name.clone(),
    host: input.host.clone(),
    port: input.port,
    user: input.user.clone(),
    auth: input.auth.clone(),
    credential: input.credential.clone(),
  };
  let target = CredentialTarget::tunnel(&tunnel, existing_id.as_deref().unwrap_or_default());
  let secret = resolve_credentials(state.inner(), &target, input.secret.clone())?
    .resolve()
    .await?;
  let known_key = state
    .known_hosts
    .lock()
    .unwrap()
    .get(&tunnel.host, tunnel.port)
    .map(|raw| ssh::parse_public_key(&raw))
    .transpose()?;
  let result = SshTunnel::open(
    &tunnel,
    secret.as_deref(),
    known_key,
    TunnelTarget {
      host: "127.0.0.1".to_string(),
      port: 1,
    },
  )
  .await
  .map(|_| ());
  // A password typed to test an unsaved tunnel has no tunnel to outlive.
  state.session_secrets.clear_one_shot(&target.key);
  result
}

#[tauri::command]
#[specta::specta]
pub fn default_ssh_keys() -> Result<Vec<String>, Error> {
  Ok(ssh::default_key_paths())
}

#[tauri::command]
#[specta::specta]
pub fn trust_host_key(
  state: State<'_, AppState>,
  host: String,
  port: u16,
  key: String,
) -> Result<(), Error> {
  ssh::parse_public_key(&key)?;
  state.known_hosts.lock().unwrap().trust(&host, port, &key)
}

#[tauri::command]
#[specta::specta]
pub async fn mcp_status(state: State<'_, AppState>) -> Result<crate::mcp::McpStatus, Error> {
  crate::mcp::status(state.inner()).await
}

#[tauri::command]
#[specta::specta]
pub async fn mcp_start(
  app: tauri::AppHandle,
  state: State<'_, AppState>,
  port: Option<u16>,
) -> Result<crate::mcp::McpStatus, Error> {
  let port = port.unwrap_or_else(|| crate::mcp::configured_port(state.inner()));
  crate::mcp::start(app, port).await?;
  crate::mcp::status(state.inner()).await
}

#[tauri::command]
#[specta::specta]
pub async fn mcp_stop(state: State<'_, AppState>) -> Result<(), Error> {
  crate::mcp::stop(state.inner()).await
}

#[tauri::command]
#[specta::specta]
pub async fn mcp_set_port(
  state: State<'_, AppState>,
  port: u16,
) -> Result<crate::mcp::McpStatus, Error> {
  crate::mcp::set_port(state.inner(), port).await?;
  crate::mcp::status(state.inner()).await
}

#[tauri::command]
#[specta::specta]
pub async fn mcp_regenerate_token(
  state: State<'_, AppState>,
) -> Result<crate::mcp::McpStatus, Error> {
  crate::mcp::regenerate_token(state.inner()).await?;
  crate::mcp::status(state.inner()).await
}

#[tauri::command]
#[specta::specta]
pub async fn mcp_audit_log(
  state: State<'_, AppState>,
  limit: Option<u32>,
) -> Result<Vec<crate::mcp::AuditEntry>, Error> {
  crate::mcp::audit_log(state.inner(), limit.unwrap_or(200) as usize)
}

#[tauri::command]
#[specta::specta]
pub async fn mcp_resolve_approval(
  state: State<'_, AppState>,
  id: String,
  answer: soquel_core::ApprovalAnswer,
) -> Result<(), Error> {
  crate::mcp::resolve_approval(state.inner(), &id, answer).await
}

#[tauri::command]
#[specta::specta]
pub async fn mcp_trust_windows(
  state: State<'_, AppState>,
) -> Result<Vec<crate::mcp::TrustWindowInfo>, Error> {
  Ok(crate::mcp::trust_windows(state.inner()).await)
}

#[tauri::command]
#[specta::specta]
pub async fn mcp_revoke_trust(
  state: State<'_, AppState>,
  session: String,
  connection_id: String,
) -> Result<(), Error> {
  crate::mcp::revoke_trust(state.inner(), &session, &connection_id).await;
  Ok(())
}

#[tauri::command]
#[specta::specta]
pub async fn secrets_status(
  state: State<'_, AppState>,
) -> Result<soquel_core::secrets::SecretsStatus, Error> {
  let problem = state.secrets_problem.clone();
  Ok(soquel_core::secrets::SecretsStatus {
    keychain: problem.is_none(),
    problem,
  })
}

/// Read from disk each time rather than cached in state: the file changes when
/// the user installs one, and this is called on a dialog opening, not per frame.
#[tauri::command]
#[specta::specta]
pub async fn licence_status(
  state: State<'_, AppState>,
) -> Result<soquel_core::licence::LicenceStatus, Error> {
  Ok(soquel_core::licence::read(&licence_path(state.inner())))
}

#[tauri::command]
#[specta::specta]
pub async fn install_licence(
  state: State<'_, AppState>,
  token: String,
) -> Result<soquel_core::licence::LicenceStatus, Error> {
  soquel_core::licence::install(&licence_path(state.inner()), &token)
}

/// None in a release build, whatever the environment says.
#[tauri::command]
#[specta::specta]
pub async fn tab_limit_override() -> Result<Option<u32>, Error> {
  Ok(soquel_core::licence::tab_limit_override())
}

/// The normal path: a key goes out, a signed file comes back and is installed
/// through the same validation as a pasted one.
#[tauri::command]
#[specta::specta]
pub async fn activate_licence(
  state: State<'_, AppState>,
  key: String,
) -> Result<soquel_core::licence::LicenceStatus, Error> {
  let token = soquel_core::activation::activate(key.trim()).await?;
  soquel_core::licence::install(&licence_path(state.inner()), &token)
}

fn licence_path(state: &AppState) -> std::path::PathBuf {
  state.data_dir.join("licence.txt")
}

/// The compile target, not a user agent string: the webview needs it for the
/// modifier key it prints and the file manager it names.
#[tauri::command]
#[specta::specta]
pub async fn platform() -> Result<String, Error> {
  Ok(std::env::consts::OS.to_string())
}

/// One preformatted block: the shape is the product, and a struct would move the
/// formatting to the webview where the facts do not live.
#[tauri::command]
#[specta::specta]
pub async fn diagnostics(
  app: tauri::AppHandle,
  state: State<'_, AppState>,
) -> Result<String, Error> {
  Ok(crate::diagnostics::block(&app, state.inner()).await)
}

#[tauri::command]
#[specta::specta]
pub async fn open_log_folder(app: tauri::AppHandle) -> Result<String, Error> {
  crate::diagnostics::open_log_folder(&app)
}

#[tauri::command]
#[specta::specta]
pub async fn check_update(
  app: tauri::AppHandle,
) -> Result<Option<crate::updater::UpdateInfo>, Error> {
  crate::updater::check(&app).await
}

#[tauri::command]
#[specta::specta]
pub async fn install_update(app: tauri::AppHandle) -> Result<(), Error> {
  crate::updater::install(app).await
}

#[cfg(test)]
mod tests {
  use super::*;
  use soquel_core::profiles::{ConnectorParams, Env, SqlServerParams, SslMode};
  use soquel_core::secrets::InMemoryStore;

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
      .connect(
        &profile,
        soquel_core::credentials::Credentials::fixed(None),
        None,
      )
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
    persist_secret(
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
      auth: soquel_core::tunnels::SshAuth::Password,
      credential: CredentialSource::Keychain,
      secret: Some("s3cret".to_string()),
    };
    let tunnel = state.tunnels.lock().unwrap().create(&stored).unwrap();
    let tunnel_key = SecretKey::Tunnel(tunnel.id.clone());
    persist_secret(
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
        auth: soquel_core::tunnels::SshAuth::Password,
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
        auth: soquel_core::tunnels::SshAuth::Password,
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

    disconnect_impl(&state, &first.id).await.unwrap();
    assert_eq!(
      state.session_secrets.get(&tunnel_key),
      Some("bastion-pw".to_string()),
      "the second connection still rides the tunnel"
    );

    disconnect_impl(&state, &second.id).await.unwrap();
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
        auth: soquel_core::tunnels::SshAuth::Password,
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
    let Err(err) = connect_impl(&state, profile.id.clone()).await else {
      panic!("the tunnel must ask for its password");
    };
    assert!(
      matches!(&err, Error::SecretRequired { subject, target_id, .. }
        if *subject == SecretSubject::Tunnel && target_id == &tunnel.id)
    );

    state
      .session_secrets
      .set(&tunnel_key, "one-off".to_string(), false);
    assert!(connect_impl(&state, profile.id.clone()).await.is_err());
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
    let err = connect_impl(&state, profile.id.clone()).await.unwrap_err();
    assert!(matches!(&err, Error::SecretRequired { target_name, .. } if target_name == "db"));

    state
      .session_secrets
      .set(&key(&profile.id), "typed".to_string(), false);
    connect_impl(&state, profile.id.clone()).await.unwrap();
    assert!(state.connections.lock().await.contains_key(&profile.id));
    // Not remembered: the next connect asks again.
    assert_eq!(state.session_secrets.get(&key(&profile.id)), None);

    state
      .session_secrets
      .set(&key(&profile.id), "typed".to_string(), true);
    connect_impl(&state, profile.id.clone()).await.unwrap();
    assert_eq!(
      state.session_secrets.get(&key(&profile.id)),
      Some("typed".to_string())
    );
  }
}
