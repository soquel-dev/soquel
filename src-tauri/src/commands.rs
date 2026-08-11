use std::sync::Arc;

use tauri::State;

use soquel_core::connectors::{
  connector_for, ApplyResult, Capability, Connection, DocBrowse, DocCollection, DocCount,
  DocDatabase, DocDetail, DocFindRequest, DocPage, DocQueryResult, IndexInfo, KeyDetail,
  KeyScanPage, KvBrowse, KvDatabases, QueryColumn, QueryResult, RowsChunk, SchemaSnapshot,
  SqlQuery, SqlSession, StreamSummary, TableChanges, TableRowsRequest,
};
use soquel_core::credentials;
use soquel_core::error::{Error, SecretSubject};
use soquel_core::export::ExportFormat;
use soquel_core::profiles::{ConnectionInput, ConnectionProfile, ConnectorKind};
use soquel_core::ssh;
use soquel_core::transfer::{self, DuplicateStrategy, ExportSummary, ImportOutcome, ImportPreview};
use soquel_core::tunnels::{TunnelInput, TunnelProfile};
use soquel_core::{AppState, SessionEntry};

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
  soquel_core::ops::test_connection(state.inner(), &input, existing_id.as_deref()).await
}

#[tauri::command]
#[specta::specta]
pub async fn connect(state: State<'_, AppState>, id: String) -> Result<(), Error> {
  soquel_core::ops::connect(state.inner(), id).await
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
  soquel_core::ops::unlock_secret(state.inner(), subject, id, secret, remember);
  Ok(())
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
  let key = soquel_core::ops::key_for(subject, id);
  let command = soquel_core::ops::current_command(state.inner(), &key)?;
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
    .revoke(&soquel_core::ops::key_for(subject, id))
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
  soquel_core::ops::disconnect(state.inner(), &id).await
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
  soquel_core::export::export_statement(columns, &rows, format, kind, &table, &path)
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
  soquel_core::export::format_statement(columns, &rows, format, kind, &table)
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

pub(crate) async fn active(state: &AppState, id: &str) -> Result<Arc<dyn Connection>, Error> {
  soquel_core::ops::active(state, id).await
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

#[tauri::command]
#[specta::specta]
pub fn create_connection(
  state: State<'_, AppState>,
  input: ConnectionInput,
) -> Result<ConnectionProfile, Error> {
  soquel_core::ops::create_connection(state.inner(), &input)
}

#[tauri::command]
#[specta::specta]
pub fn update_connection(
  state: State<'_, AppState>,
  id: String,
  input: ConnectionInput,
) -> Result<ConnectionProfile, Error> {
  soquel_core::ops::update_connection(state.inner(), &id, &input)
}

#[tauri::command]
#[specta::specta]
pub fn delete_connection(state: State<'_, AppState>, id: String) -> Result<(), Error> {
  soquel_core::ops::delete_connection(state.inner(), &id)
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
  soquel_core::ops::create_tunnel(state.inner(), &input)
}

#[tauri::command]
#[specta::specta]
pub fn update_tunnel(
  state: State<'_, AppState>,
  id: String,
  input: TunnelInput,
) -> Result<TunnelProfile, Error> {
  soquel_core::ops::update_tunnel(state.inner(), &id, &input)
}

#[tauri::command]
#[specta::specta]
pub fn delete_tunnel(state: State<'_, AppState>, id: String) -> Result<(), Error> {
  soquel_core::ops::delete_tunnel(state.inner(), &id)
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
  soquel_core::ops::test_tunnel(state.inner(), &input, existing_id.as_deref()).await
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
  soquel_core::ops::trust_host_key(state.inner(), &host, port, &key)
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
