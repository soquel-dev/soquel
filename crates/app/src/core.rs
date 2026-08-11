use std::future::Future;
use std::sync::{Arc, OnceLock};

use gpui::{AppContext, Task};
use soquel_core::connectors::{
  Connection, DocCollection, DocCount, DocDatabase, DocDetail, DocFindRequest, DocPage,
  DocQueryResult, IndexInfo, KeyDetail, KeyScanPage, KvDatabases, QueryResult, SchemaSnapshot,
  TableRowsRequest,
};
use soquel_core::error::{Error, SecretSubject};
use soquel_core::profiles::{ConnectionInput, ConnectionProfile, ConnectorKind};
use soquel_core::tunnels::{TunnelInput, TunnelProfile};
use soquel_core::{AppState, ApprovalAnswer};

fn runtime() -> &'static tokio::runtime::Runtime {
  static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
  RT.get_or_init(|| {
    tokio::runtime::Builder::new_multi_thread()
      .enable_all()
      .build()
      .expect("tokio runtime")
  })
}

/// Runs a core future on the runtime. The returned task only observes
/// completion: dropping it never aborts the work, so an in-flight write
/// cannot be cut mid-protocol.
fn bridge<T>(cx: &impl AppContext, fut: impl Future<Output = T> + Send + 'static) -> Task<T>
where
  T: Send + 'static,
{
  let handle = runtime().spawn(fut);
  cx.background_spawn(async move { handle.await.expect("core task panicked") })
}

/// Same, but dropping the task aborts the work. Only for reads a view
/// supersedes (scans, fetches, introspection), never for writes.
fn bridge_abortable<T>(
  cx: &impl AppContext,
  fut: impl Future<Output = T> + Send + 'static,
) -> Task<T>
where
  T: Send + 'static,
{
  let handle = runtime().spawn(fut);
  let abort = AbortOnDrop(handle.abort_handle());
  cx.background_spawn(async move {
    let result = handle.await;
    drop(abort);
    result.expect("core task panicked")
  })
}

struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
  fn drop(&mut self) {
    self.0.abort();
  }
}

#[derive(Clone)]
pub struct Db(Arc<dyn Connection>, ConnectorKind);

impl Db {
  pub fn server_version(&self) -> Option<String> {
    self.0.server_version()
  }

  pub fn kind(&self) -> ConnectorKind {
    self.1
  }
}

/// The identifier is already on installed disks: keep it stable. Debug builds
/// get a /dev subtree so a dev run never touches real data.
pub fn resolve_data_dir(
  override_dir: Option<&str>,
  xdg_data_home: Option<&std::path::Path>,
  home: Option<&std::path::Path>,
  debug: bool,
) -> std::path::PathBuf {
  if let Some(dir) = override_dir {
    return std::path::PathBuf::from(dir);
  }
  let base = xdg_data_home
    .map(std::path::Path::to_path_buf)
    .or_else(|| home.map(|home| home.join(".local/share")))
    .unwrap_or_default();
  let root = base.join("dev.soquel.app");
  if debug { root.join("dev") } else { root }
}

fn data_dir_from_env() -> std::path::PathBuf {
  let override_dir = std::env::var("SOQUEL_DATA_DIR").ok();
  let xdg = std::env::var_os("XDG_DATA_HOME").map(std::path::PathBuf::from);
  let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
  resolve_data_dir(
    override_dir.as_deref(),
    xdg.as_deref(),
    home.as_deref(),
    cfg!(debug_assertions),
  )
}

pub fn init_state() -> Result<Arc<AppState>, Error> {
  let data_dir = data_dir_from_env();
  std::fs::create_dir_all(&data_dir)?;
  let secrets = soquel_core::secrets::store_from_env(&data_dir)?;
  Ok(Arc::new(AppState::load(&data_dir, secrets)?))
}

/// The plugin defaults to 40 kB, which holds about one session.
const MAX_LOG_SIZE: u64 = 2 * 1024 * 1024;

/// No size rotation, so cap growth across sessions: start fresh once when the
/// file has grown past the ceiling.
fn over_size_cap(len: u64) -> bool {
  len > MAX_LOG_SIZE
}

/// A file logger to `<data dir>/logs/soquel[-dev].log`: Warn everywhere, Info
/// for our own crates, so our lines are not buried under russh, hyper and
/// rustls. Call before `init_state` so the keyring probe is captured.
/// Failures are non-fatal: the app runs without logs.
pub fn init_logging() {
  let dir = data_dir_from_env().join("logs");
  if std::fs::create_dir_all(&dir).is_err() {
    return;
  }
  let path = dir.join(LOG_FILE_NAME).with_extension("log");
  if std::fs::metadata(&path).is_ok_and(|meta| over_size_cap(meta.len())) {
    let _ = std::fs::remove_file(&path);
  }
  let Ok(file) = fern::log_file(&path) else {
    return;
  };
  let mut dispatch = fern::Dispatch::new()
    .format(|out, message, record| {
      out.finish(format_args!(
        "{} {} {} {}",
        humantime::format_rfc3339_seconds(std::time::SystemTime::now()),
        record.level(),
        record.target(),
        message
      ))
    })
    .level(log::LevelFilter::Warn)
    .level_for("soquel_core", log::LevelFilter::Info)
    .level_for("soquel_app", log::LevelFilter::Info)
    .chain(file);
  // A bundle has no console; a debug run does.
  if cfg!(debug_assertions) {
    dispatch = dispatch.chain(std::io::stdout());
  }
  // An already-set logger is not an error worth failing the app over.
  let _ = dispatch.apply();
}

pub fn list_connections(state: &AppState) -> Vec<ConnectionProfile> {
  state.profiles.lock().unwrap().list()
}

/// ops::connect fills the state's map; the grid plumbing rides a Db handle.
pub fn connect_id(
  state: Arc<AppState>,
  id: String,
  cx: &impl AppContext,
) -> Task<Result<Db, Error>> {
  bridge(cx, async move {
    soquel_core::ops::connect(&state, id.clone()).await?;
    let kind = state.profiles.lock().unwrap().get(&id)?.params.kind();
    let connection = soquel_core::ops::active(&state, &id).await?;
    Ok(Db(connection, kind))
  })
}

pub fn disconnect_id(state: Arc<AppState>, id: String) {
  runtime().spawn(async move {
    let _ = soquel_core::ops::disconnect(&state, &id).await;
  });
}

pub fn test_input(
  state: Arc<AppState>,
  input: ConnectionInput,
  existing_id: Option<String>,
  cx: &impl AppContext,
) -> Task<Result<(), Error>> {
  bridge(cx, async move {
    soquel_core::ops::test_connection(&state, &input, existing_id.as_deref()).await
  })
}

/// Store writes can touch the OS keychain: off the UI thread like everything else.
pub fn save_connection(
  state: Arc<AppState>,
  editing: Option<String>,
  input: ConnectionInput,
  cx: &impl AppContext,
) -> Task<Result<ConnectionProfile, Error>> {
  bridge(cx, async move {
    match editing {
      Some(id) => soquel_core::ops::update_connection(&state, &id, &input),
      None => soquel_core::ops::create_connection(&state, &input),
    }
  })
}

pub fn delete_connection(
  state: Arc<AppState>,
  id: String,
  cx: &impl AppContext,
) -> Task<Result<(), Error>> {
  bridge(cx, async move {
    soquel_core::ops::delete_connection(&state, &id)
  })
}

pub fn unlock_secret(
  state: &AppState,
  subject: SecretSubject,
  id: String,
  secret: String,
  remember: bool,
) {
  soquel_core::ops::unlock_secret(state, subject, id, secret, remember);
}

pub fn list_tunnels(state: &AppState) -> Vec<TunnelProfile> {
  state.tunnels.lock().unwrap().list()
}

pub fn save_tunnel(
  state: Arc<AppState>,
  editing: Option<String>,
  input: TunnelInput,
  cx: &impl AppContext,
) -> Task<Result<TunnelProfile, Error>> {
  bridge(cx, async move {
    match editing {
      Some(id) => soquel_core::ops::update_tunnel(&state, &id, &input),
      None => soquel_core::ops::create_tunnel(&state, &input),
    }
  })
}

pub fn delete_tunnel(
  state: Arc<AppState>,
  id: String,
  cx: &impl AppContext,
) -> Task<Result<(), Error>> {
  bridge(
    cx,
    async move { soquel_core::ops::delete_tunnel(&state, &id) },
  )
}

pub fn test_tunnel(
  state: Arc<AppState>,
  input: TunnelInput,
  existing_id: Option<String>,
  cx: &impl AppContext,
) -> Task<Result<(), Error>> {
  bridge(cx, async move {
    soquel_core::ops::test_tunnel(&state, &input, existing_id.as_deref()).await
  })
}

/// A small JSON write, but still disk: off the frame thread like every write.
pub fn trust_host_key(
  state: Arc<AppState>,
  host: String,
  port: u16,
  key: String,
  cx: &impl AppContext,
) -> Task<Result<(), Error>> {
  bridge(cx, async move {
    soquel_core::ops::trust_host_key(&state, &host, port, &key)
  })
}

pub fn approve_credential_command(
  state: Arc<AppState>,
  subject: SecretSubject,
  id: String,
  cx: &impl AppContext,
) -> Task<Result<(), Error>> {
  bridge(cx, async move {
    soquel_core::ops::approve_credential_command(&state, subject, id)
  })
}

pub fn revoke_credential_command(
  state: Arc<AppState>,
  subject: SecretSubject,
  id: String,
  cx: &impl AppContext,
) -> Task<Result<(), Error>> {
  bridge(cx, async move {
    soquel_core::ops::revoke_credential_command(&state, subject, id)
  })
}

/// Stats a handful of ~/.ssh paths: cheap, but a slow home dir is disk too.
pub fn default_ssh_keys(cx: &impl AppContext) -> Task<Vec<String>> {
  bridge(cx, async move { soquel_core::ssh::default_key_paths() })
}

/// Each frontend injects how a blocked write gets its yes; gpui sends it through
/// a channel the App drains (see `mcp::GpuiApprover`).
pub type ApproverFactory = Arc<dyn Fn() -> Arc<dyn soquel_core::mcp::Approver> + Send + Sync>;

pub fn mcp_configured_port(state: &AppState) -> u16 {
  soquel_core::mcp::configured_port(state)
}

pub fn mcp_status(
  state: Arc<AppState>,
  cx: &impl AppContext,
) -> Task<Result<soquel_core::mcp::McpStatus, Error>> {
  bridge(cx, async move { soquel_core::mcp::status(&state).await })
}

/// The server runs on `runtime()`, off the UI thread; `make_approver` is the
/// gpui seam, its answers arriving through the App's approval channel.
pub fn mcp_start(
  state: Arc<AppState>,
  port: u16,
  make_approver: ApproverFactory,
  cx: &impl AppContext,
) -> Task<Result<(), Error>> {
  bridge(cx, async move {
    soquel_core::mcp::start(state, port, make_approver).await
  })
}

/// Fire-and-forget: launch reads the persisted toggle and brings the server back.
pub fn mcp_autostart(state: Arc<AppState>, make_approver: ApproverFactory) {
  runtime().spawn(async move {
    soquel_core::mcp::autostart(state, make_approver).await;
  });
}

pub fn mcp_stop(state: Arc<AppState>, cx: &impl AppContext) -> Task<Result<(), Error>> {
  bridge(cx, async move { soquel_core::mcp::stop(&state).await })
}

pub fn mcp_set_port(
  state: Arc<AppState>,
  port: u16,
  cx: &impl AppContext,
) -> Task<Result<(), Error>> {
  bridge(
    cx,
    async move { soquel_core::mcp::set_port(&state, port).await },
  )
}

pub fn mcp_regenerate_token(
  state: Arc<AppState>,
  cx: &impl AppContext,
) -> Task<Result<String, Error>> {
  bridge(cx, async move {
    soquel_core::mcp::regenerate_token(&state).await
  })
}

pub fn mcp_audit_log(
  state: Arc<AppState>,
  limit: usize,
  cx: &impl AppContext,
) -> Task<Result<Vec<soquel_core::mcp::AuditEntry>, Error>> {
  bridge(
    cx,
    async move { soquel_core::mcp::audit_log(&state, limit) },
  )
}

/// Fires the oneshot the server thread is parked on; NotFound (already expired)
/// is fine, the write is denied either way.
pub fn mcp_resolve_approval(state: Arc<AppState>, id: String, answer: ApprovalAnswer) {
  runtime().spawn(async move {
    let _ = soquel_core::mcp::resolve_approval(&state, &id, answer).await;
  });
}

pub fn mcp_trust_windows(
  state: Arc<AppState>,
  cx: &impl AppContext,
) -> Task<Vec<soquel_core::mcp::TrustWindowInfo>> {
  bridge(
    cx,
    async move { soquel_core::mcp::trust_windows(&state).await },
  )
}

pub fn mcp_revoke_trust(
  state: Arc<AppState>,
  session: String,
  connection_id: String,
  cx: &impl AppContext,
) -> Task<()> {
  bridge(cx, async move {
    soquel_core::mcp::revoke_trust(&state, &session, &connection_id).await;
  })
}

/// Sync: reads the installed licence file. Called on the dialog opening and per
/// tab-open, not per frame.
pub fn licence_status(state: &AppState) -> soquel_core::licence::LicenceStatus {
  soquel_core::licence::read(&soquel_core::licence::path(&state.data_dir))
}

/// A pasted file: validated before it replaces a working licence.
pub fn licence_install(
  state: Arc<AppState>,
  token: String,
  cx: &impl AppContext,
) -> Task<Result<soquel_core::licence::LicenceStatus, Error>> {
  bridge(cx, async move {
    soquel_core::licence::install(&soquel_core::licence::path(&state.data_dir), &token)
  })
}

/// The normal path: the key goes out, a signed file comes back and installs
/// through the same validation as a pasted one. HTTP lives in the core.
pub fn licence_activate(
  state: Arc<AppState>,
  key: String,
  cx: &impl AppContext,
) -> Task<Result<soquel_core::licence::LicenceStatus, Error>> {
  bridge(cx, async move {
    let token = soquel_core::activation::activate(key.trim()).await?;
    soquel_core::licence::install(&soquel_core::licence::path(&state.data_dir), &token)
  })
}

/// Logs land under the data dir, whether or not SOQUEL_DATA_DIR is set: one
/// place the diagnostics path names and the folder opens.
fn log_dir(state: &AppState) -> std::path::PathBuf {
  state.data_dir.join("logs")
}

const LOG_FILE_NAME: &str = if cfg!(debug_assertions) {
  "soquel-dev"
} else {
  "soquel"
};

/// The pasteable support block, built in the core. Reads state locks, so off the
/// UI thread; carries no names, hosts or paths beyond the log's own.
pub fn diagnostics(state: Arc<AppState>, cx: &impl AppContext) -> Task<String> {
  let log_path = log_dir(&state)
    .join(LOG_FILE_NAME)
    .with_extension("log")
    .display()
    .to_string();
  let build = if cfg!(debug_assertions) {
    "debug"
  } else {
    "release"
  };
  bridge(cx, async move {
    soquel_core::diagnostics::block(&state, env!("CARGO_PKG_VERSION"), build, &log_path).await
  })
}

/// Opens the folder, not the file: someone about to attach a log wants the
/// folder. Detached, so a session with no file manager reports success while
/// doing nothing - the path stays on screen as the fallback.
pub fn open_log_folder(state: &AppState) -> Result<String, Error> {
  let dir = log_dir(state);
  let _ = std::fs::create_dir_all(&dir);
  open::that_detached(&dir).map_err(|err| Error::Unsupported {
    message: format!("could not open the log folder: {err}"),
  })?;
  Ok(dir.display().to_string())
}

/// Off the UI thread: exporting reads the keychain.
pub fn export_connections(
  state: Arc<AppState>,
  path: std::path::PathBuf,
  include_secrets: bool,
  passphrase: Option<String>,
  cx: &impl AppContext,
) -> Task<Result<soquel_core::transfer::ExportSummary, Error>> {
  bridge(cx, async move {
    soquel_core::transfer::export(&state, &path, include_secrets, passphrase.as_deref())
  })
}

/// Off the UI thread: an encrypted file derives an argon2 key to open.
pub fn preview_import(
  state: Arc<AppState>,
  path: std::path::PathBuf,
  passphrase: Option<String>,
  cx: &impl AppContext,
) -> Task<Result<soquel_core::transfer::ImportPreview, Error>> {
  bridge(cx, async move {
    soquel_core::transfer::preview_file(&state, &path, passphrase.as_deref())
  })
}

pub fn import_connections(
  state: Arc<AppState>,
  path: std::path::PathBuf,
  passphrase: Option<String>,
  with_secrets: bool,
  strategy: soquel_core::transfer::DuplicateStrategy,
  cx: &impl AppContext,
) -> Task<Result<soquel_core::transfer::ImportOutcome, Error>> {
  bridge(cx, async move {
    soquel_core::transfer::import_file(&state, &path, passphrase.as_deref(), with_secrets, strategy)
  })
}

/// Test-only direct connect, bypassing the stores.
#[cfg(test)]
pub fn connect_with(
  profile: ConnectionProfile,
  secret: Arc<soquel_core::credentials::Credentials>,
  cx: &impl AppContext,
) -> Task<Result<Db, Error>> {
  bridge(cx, async move {
    let kind = profile.params.kind();
    soquel_core::connectors::connector_for(kind)
      .connect(&profile, secret, None)
      .await
      .map(|conn| Db(Arc::from(conn), kind))
  })
}

/// Test-only sync variants for setup before the executor runs: blocking the
/// test thread on a gpui task would deadlock the test scheduler.
#[cfg(test)]
pub fn connect_with_blocking(
  profile: ConnectionProfile,
  secret: Arc<soquel_core::credentials::Credentials>,
) -> Result<Db, Error> {
  runtime().block_on(async move {
    let kind = profile.params.kind();
    soquel_core::connectors::connector_for(kind)
      .connect(&profile, secret, None)
      .await
      .map(|conn| Db(Arc::from(conn), kind))
  })
}

#[cfg(test)]
pub fn kv_run_command_blocking(db: &Db, command: String) -> Result<Vec<String>, Error> {
  let conn = db.0.clone();
  runtime().block_on(async move {
    match conn.kv() {
      Some(kv) => kv.run_command(&command).await,
      None => Err(no_kv()),
    }
  })
}

pub fn fetch_rows(
  db: &Db,
  request: TableRowsRequest,
  cx: &impl AppContext,
) -> Task<Result<QueryResult, Error>> {
  let conn = db.0.clone();
  bridge_abortable(cx, async move {
    match conn.sql() {
      Some(sql) => sql.table_rows(&request).await,
      None => Err(Error::Unsupported {
        message: "connection has no sql surface".to_string(),
      }),
    }
  })
}

pub fn apply_changes(
  db: &Db,
  changes: soquel_core::connectors::TableChanges,
  cx: &impl AppContext,
) -> Task<Result<soquel_core::connectors::ApplyResult, Error>> {
  let conn = db.0.clone();
  bridge(cx, async move {
    match conn.sql() {
      Some(surface) => surface.apply_changes(&changes).await,
      None => Err(Error::Unsupported {
        message: "connection has no sql surface".to_string(),
      }),
    }
  })
}

/// A dedicated client outside the pool: SET and transactions stick,
/// and cancel targets only this session.
#[derive(Clone)]
pub struct Session(Arc<dyn soquel_core::connectors::SqlSession>);

pub fn open_session(db: &Db, cx: &impl AppContext) -> Task<Result<Session, Error>> {
  let conn = db.0.clone();
  bridge(cx, async move {
    match conn.sql() {
      Some(surface) => surface.open_session().await.map(|s| Session(Arc::from(s))),
      None => Err(Error::Unsupported {
        message: "connection has no sql surface".to_string(),
      }),
    }
  })
}

pub fn run_session_query(
  session: &Session,
  sql: String,
  cx: &impl AppContext,
) -> Task<Result<QueryResult, Error>> {
  let session = session.0.clone();
  bridge(cx, async move { session.run_query(&sql).await })
}

pub fn cancel_session(session: &Session) {
  let session = session.0.clone();
  runtime().spawn(async move {
    // The query may have finished in the meantime; the run itself reports.
    let _ = session.cancel().await;
  });
}

pub fn close_session(session: Session) {
  runtime().spawn(async move {
    let _ = session.0.close().await;
  });
}

pub fn export_rows(
  db: &Db,
  request: TableRowsRequest,
  format: soquel_core::export::ExportFormat,
  path: String,
  cx: &impl AppContext,
) -> (
  futures::channel::mpsc::UnboundedReceiver<u64>,
  Task<Result<soquel_core::connectors::StreamSummary, Error>>,
) {
  let (progress_tx, progress_rx) = futures::channel::mpsc::unbounded();
  let conn = db.0.clone();
  let kind = db.1;
  let task = bridge(cx, async move {
    match conn.sql() {
      Some(surface) => {
        soquel_core::export::run_export(surface, &request, format, kind, &path, move |rows| {
          let _ = progress_tx.unbounded_send(rows);
        })
        .await
      }
      None => Err(Error::Unsupported {
        message: "connection has no sql surface".to_string(),
      }),
    }
  });
  (progress_rx, task)
}

pub fn table_ddl(
  db: &Db,
  schema: String,
  table: String,
  cx: &impl AppContext,
) -> Task<Result<String, Error>> {
  let conn = db.0.clone();
  bridge_abortable(cx, async move {
    match conn.introspect() {
      Some(introspect) => introspect.table_ddl(&schema, &table).await,
      None => Err(Error::Unsupported {
        message: "connection has no introspection surface".to_string(),
      }),
    }
  })
}

pub fn schema_snapshot(db: &Db, cx: &impl AppContext) -> Task<Result<SchemaSnapshot, Error>> {
  let conn = db.0.clone();
  bridge_abortable(cx, async move {
    match conn.introspect() {
      Some(introspect) => introspect.schema_snapshot().await,
      None => Err(Error::Unsupported {
        message: "connection has no introspection surface".to_string(),
      }),
    }
  })
}

fn no_kv() -> Error {
  Error::Unsupported {
    message: "connection does not browse keys".to_string(),
  }
}

pub fn kv_scan(
  db: &Db,
  pattern: String,
  cursor: Option<String>,
  count: u32,
  cx: &impl AppContext,
) -> Task<Result<KeyScanPage, Error>> {
  let conn = db.0.clone();
  bridge_abortable(cx, async move {
    match conn.kv() {
      Some(kv) => kv.scan_keys(&pattern, cursor.as_deref(), count).await,
      None => Err(no_kv()),
    }
  })
}

pub fn kv_key_detail(db: &Db, key: String, cx: &impl AppContext) -> Task<Result<KeyDetail, Error>> {
  let conn = db.0.clone();
  bridge_abortable(cx, async move {
    match conn.kv() {
      Some(kv) => kv.key_detail(&key).await,
      None => Err(no_kv()),
    }
  })
}

pub fn kv_databases(db: &Db, cx: &impl AppContext) -> Task<Result<KvDatabases, Error>> {
  let conn = db.0.clone();
  bridge_abortable(cx, async move {
    match conn.kv() {
      Some(kv) => kv.databases().await,
      None => Err(no_kv()),
    }
  })
}

pub fn kv_run_command(
  db: &Db,
  command: String,
  cx: &impl AppContext,
) -> Task<Result<Vec<String>, Error>> {
  let conn = db.0.clone();
  bridge(cx, async move {
    match conn.kv() {
      Some(kv) => kv.run_command(&command).await,
      None => Err(no_kv()),
    }
  })
}

pub fn kv_set_string(
  db: &Db,
  key: String,
  value: String,
  cx: &impl AppContext,
) -> Task<Result<(), Error>> {
  let conn = db.0.clone();
  bridge(cx, async move {
    match conn.kv() {
      Some(kv) => kv.set_string(&key, &value).await,
      None => Err(no_kv()),
    }
  })
}

pub fn kv_delete_key(db: &Db, key: String, cx: &impl AppContext) -> Task<Result<(), Error>> {
  let conn = db.0.clone();
  bridge(cx, async move {
    match conn.kv() {
      Some(kv) => kv.delete_key(&key).await,
      None => Err(no_kv()),
    }
  })
}

pub fn kv_set_ttl(
  db: &Db,
  key: String,
  ttl_ms: Option<f64>,
  cx: &impl AppContext,
) -> Task<Result<(), Error>> {
  let conn = db.0.clone();
  bridge(cx, async move {
    match conn.kv() {
      Some(kv) => kv.set_ttl(&key, ttl_ms).await,
      None => Err(no_kv()),
    }
  })
}

/// Switching db is a reconnect (a SELECT reverts on the multiplexed socket):
/// the core swaps the stored connection, we hand back a fresh Db.
pub fn kv_select_db(
  state: Arc<AppState>,
  id: String,
  db: u32,
  cx: &impl AppContext,
) -> Task<Result<Db, Error>> {
  bridge(cx, async move {
    state.select_kv_db(&id, db).await?;
    let kind = state.profiles.lock().unwrap().get(&id)?.params.kind();
    let connection = soquel_core::ops::active(&state, &id).await?;
    Ok(Db(connection, kind))
  })
}

fn no_doc() -> Error {
  Error::Unsupported {
    message: "connection does not browse documents".to_string(),
  }
}

pub fn doc_databases(db: &Db, cx: &impl AppContext) -> Task<Result<Vec<DocDatabase>, Error>> {
  let conn = db.0.clone();
  bridge_abortable(cx, async move {
    match conn.doc() {
      Some(doc) => doc.databases().await,
      None => Err(no_doc()),
    }
  })
}

pub fn doc_collections(
  db: &Db,
  database: String,
  cx: &impl AppContext,
) -> Task<Result<Vec<DocCollection>, Error>> {
  let conn = db.0.clone();
  bridge_abortable(cx, async move {
    match conn.doc() {
      Some(doc) => doc.collections(&database).await,
      None => Err(no_doc()),
    }
  })
}

pub fn doc_find(
  db: &Db,
  request: DocFindRequest,
  cx: &impl AppContext,
) -> Task<Result<DocPage, Error>> {
  let conn = db.0.clone();
  bridge_abortable(cx, async move {
    match conn.doc() {
      Some(doc) => doc.find_docs(&request).await,
      None => Err(no_doc()),
    }
  })
}

pub fn doc_detail(
  db: &Db,
  database: String,
  collection: String,
  id: String,
  cx: &impl AppContext,
) -> Task<Result<DocDetail, Error>> {
  let conn = db.0.clone();
  bridge_abortable(cx, async move {
    match conn.doc() {
      Some(doc) => doc.doc_detail(&database, &collection, &id).await,
      None => Err(no_doc()),
    }
  })
}

pub fn doc_indexes(
  db: &Db,
  database: String,
  collection: String,
  cx: &impl AppContext,
) -> Task<Result<Vec<IndexInfo>, Error>> {
  let conn = db.0.clone();
  bridge_abortable(cx, async move {
    match conn.doc() {
      Some(doc) => doc.indexes(&database, &collection).await,
      None => Err(no_doc()),
    }
  })
}

pub fn doc_count(
  db: &Db,
  database: String,
  collection: String,
  filter: Option<String>,
  cx: &impl AppContext,
) -> Task<Result<DocCount, Error>> {
  let conn = db.0.clone();
  bridge_abortable(cx, async move {
    match conn.doc() {
      Some(doc) => {
        doc
          .count_docs(&database, &collection, filter.as_deref())
          .await
      }
      None => Err(no_doc()),
    }
  })
}

pub fn doc_run_query(
  db: &Db,
  database: String,
  collection: String,
  source: String,
  cx: &impl AppContext,
) -> Task<Result<DocQueryResult, Error>> {
  let conn = db.0.clone();
  bridge(cx, async move {
    match conn.doc() {
      Some(doc) => doc.run_query(&database, &collection, &source).await,
      None => Err(no_doc()),
    }
  })
}

pub fn doc_replace(
  db: &Db,
  database: String,
  collection: String,
  id: String,
  doc_json: String,
  cx: &impl AppContext,
) -> Task<Result<(), Error>> {
  let conn = db.0.clone();
  bridge(cx, async move {
    match conn.doc() {
      Some(doc) => {
        doc
          .replace_doc(&database, &collection, &id, &doc_json)
          .await
      }
      None => Err(no_doc()),
    }
  })
}

pub fn doc_delete(
  db: &Db,
  database: String,
  collection: String,
  id: String,
  cx: &impl AppContext,
) -> Task<Result<(), Error>> {
  let conn = db.0.clone();
  bridge(cx, async move {
    match conn.doc() {
      Some(doc) => doc.delete_doc(&database, &collection, &id).await,
      None => Err(no_doc()),
    }
  })
}

pub fn page_request(schema: &str, table: &str, offset: u32, limit: u32) -> TableRowsRequest {
  TableRowsRequest {
    schema: schema.to_string(),
    table: table.to_string(),
    limit: Some(limit),
    offset,
    sort: None,
    filters: Vec::new(),
    include_ctid: false,
    include_xmin: false,
  }
}

#[cfg(test)]
mod tests {
  use soquel_core::credentials::Credentials;
  use soquel_core::profiles::{
    AgentAccess, ConnectorParams, CredentialSource, Env, SqlServerParams,
  };

  use super::*;
  use crate::staged::StagedChanges;

  #[test]
  fn the_log_resets_only_past_the_ceiling() {
    assert!(!over_size_cap(0));
    assert!(!over_size_cap(MAX_LOG_SIZE));
    assert!(over_size_cap(MAX_LOG_SIZE + 1));
  }

  /// postgres://user:pass@host:port/db, the shape test-integration.sh exports.
  fn parse_pg_url(url: &str) -> Option<(String, u16, String, String, String)> {
    let rest = url.strip_prefix("postgres://")?;
    let (creds, host_db) = rest.split_once('@')?;
    let (user, pass) = creds.split_once(':')?;
    let (host_port, db) = host_db.split_once('/')?;
    let (host, port) = host_port.split_once(':')?;
    Some((
      host.to_string(),
      port.parse().ok()?,
      db.to_string(),
      user.to_string(),
      pass.to_string(),
    ))
  }

  #[test]
  fn data_dir_override_wins_and_dev_gets_its_subtree() {
    use std::path::{Path, PathBuf};
    assert_eq!(
      resolve_data_dir(Some("/custom"), Some(Path::new("/xdg")), None, true),
      PathBuf::from("/custom")
    );
    // The identifier and the /dev split must match what installs have on disk.
    assert_eq!(
      resolve_data_dir(None, Some(Path::new("/xdg")), None, true),
      PathBuf::from("/xdg/dev.soquel.app/dev")
    );
    assert_eq!(
      resolve_data_dir(None, None, Some(Path::new("/home/u")), false),
      PathBuf::from("/home/u/.local/share/dev.soquel.app")
    );
  }

  /// The whole grid path against a real postgres: browse, stage, apply, reread.
  /// Skipped silently without SOQUEL_TEST_PG, like the core suites.
  #[gpui::test]
  async fn integration_flow_browse_stage_apply(cx: &mut gpui::TestAppContext) {
    cx.executor().allow_parking();
    let Some(url) = soquel_core::integration_env("SOQUEL_TEST_PG") else {
      return;
    };
    let (host, port, database, user, pass) = parse_pg_url(&url).expect("parsable test url");
    let profile = ConnectionProfile {
      id: "flow-test".to_string(),
      name: "flow test".to_string(),
      env: Env::Dev,
      group: None,
      agent_access: AgentAccess::None,
      credential: CredentialSource::Prompt,
      params: ConnectorParams::Postgres(SqlServerParams {
        host,
        port,
        database,
        user,
        ssl_mode: Default::default(),
        ssl_root_cert: None,
        tunnel_id: None,
      }),
    };
    let db = connect_with(profile, Credentials::fixed(Some(pass)), cx)
      .await
      .expect("connects to the compose postgres");

    let table = format!("gpui_flow_{}", std::process::id());
    let session = open_session(&db, cx).await.expect("session");
    run_session_query(
      &session,
      format!("create table {table} (id int primary key, name text)"),
      cx,
    )
    .await
    .expect("create");
    run_session_query(
      &session,
      format!("insert into {table} values (1, 'Ada'), (2, 'Alan')"),
      cx,
    )
    .await
    .expect("insert");

    // Browse the first page like the grid does; an update moves the row in the
    // heap, so ordering must be explicit like the grid's sort would be.
    let sorted = |offset| {
      let mut request = page_request("public", &table, offset, 10);
      request.sort = Some(soquel_core::connectors::SortSpec {
        column: "id".to_string(),
        direction: soquel_core::connectors::SortDirection::Asc,
      });
      request
    };
    let page = fetch_rows(&db, sorted(0), cx).await.expect("fetch");
    let statement = &page.statements[0];
    assert_eq!(statement.rows.len(), 2);

    // Stage an edit and apply it through the shared builder.
    let mut staged = StagedChanges::default();
    staged.edits.insert(
      0,
      [("name".to_string(), Some("Ada II".to_string()))]
        .into_iter()
        .collect(),
    );
    let changes = crate::staged::build_table_changes(
      &staged,
      &statement.rows,
      &statement.columns,
      &["id".to_string()],
      "public",
      &table,
    );
    assert_eq!(changes.updates.len(), 1);
    assert_eq!(changes.updates[0].key[0].column, "id");
    assert_eq!(changes.updates[0].key[0].value, Some("1".to_string()));
    let applied = apply_changes(&db, changes, cx).await.expect("apply");
    assert_eq!(applied.updated, 1);

    // The reread sees the committed value.
    let page = fetch_rows(&db, sorted(0), cx).await.expect("refetch");
    assert_eq!(page.statements[0].rows[0][1], Some("Ada II".to_string()));

    let _ = run_session_query(&session, format!("drop table {table}"), cx).await;
    close_session(session);
  }

  /// The same grid path against a real mysql/mariadb; the schema is the database.
  /// Skipped silently without SOQUEL_TEST_MYSQL.
  #[gpui::test]
  async fn integration_flow_mysql(cx: &mut gpui::TestAppContext) {
    cx.executor().allow_parking();
    let Some(addr) = soquel_core::integration_env("SOQUEL_TEST_MYSQL") else {
      return;
    };
    let (host, port) = addr
      .split_once(':')
      .expect("SOQUEL_TEST_MYSQL is host:port");
    let database = "soquel_test";
    let profile = ConnectionProfile {
      id: "flow-mysql".to_string(),
      name: "flow mysql".to_string(),
      env: Env::Dev,
      group: None,
      agent_access: AgentAccess::None,
      credential: CredentialSource::Prompt,
      params: ConnectorParams::Mysql(SqlServerParams {
        host: host.to_string(),
        port: port.parse().unwrap(),
        database: database.to_string(),
        user: "soquel".to_string(),
        ssl_mode: Default::default(),
        ssl_root_cert: None,
        tunnel_id: None,
      }),
    };
    let db = connect_with(profile, Credentials::fixed(Some("soquel".to_string())), cx)
      .await
      .expect("connects to the compose mysql");

    let table = format!("gpui_flow_{}", std::process::id());
    let session = open_session(&db, cx).await.expect("session");
    run_session_query(
      &session,
      format!("create table {table} (id int primary key, name text)"),
      cx,
    )
    .await
    .expect("create");
    run_session_query(
      &session,
      format!("insert into {table} values (1, 'Ada'), (2, 'Alan')"),
      cx,
    )
    .await
    .expect("insert");

    let sorted = |offset| {
      let mut request = page_request(database, &table, offset, 10);
      request.sort = Some(soquel_core::connectors::SortSpec {
        column: "id".to_string(),
        direction: soquel_core::connectors::SortDirection::Asc,
      });
      request
    };
    let page = fetch_rows(&db, sorted(0), cx).await.expect("fetch");
    let statement = &page.statements[0];
    assert_eq!(statement.rows.len(), 2);

    let mut staged = StagedChanges::default();
    staged.edits.insert(
      0,
      [("name".to_string(), Some("Ada II".to_string()))]
        .into_iter()
        .collect(),
    );
    let changes = crate::staged::build_table_changes(
      &staged,
      &statement.rows,
      &statement.columns,
      &["id".to_string()],
      database,
      &table,
    );
    let applied = apply_changes(&db, changes, cx).await.expect("apply");
    assert_eq!(applied.updated, 1);

    let page = fetch_rows(&db, sorted(0), cx).await.expect("refetch");
    assert_eq!(page.statements[0].rows[0][1], Some("Ada II".to_string()));

    let _ = run_session_query(&session, format!("drop table {table}"), cx).await;
    close_session(session);
  }

  /// The same grid path against a fresh sqlite file (no server). Gated on
  /// SOQUEL_TEST_SQLITE so it runs in the integration leg, not the unit suite.
  #[gpui::test]
  async fn integration_flow_sqlite(cx: &mut gpui::TestAppContext) {
    cx.executor().allow_parking();
    if soquel_core::integration_env("SOQUEL_TEST_SQLITE").is_none() {
      return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("flow.db");
    // The connector never mints a file (a typo'd path must error); a real user
    // browses to an existing db, so the test opens an empty one it made.
    std::fs::File::create(&path).unwrap();
    let profile = ConnectionProfile {
      id: "flow-sqlite".to_string(),
      name: "flow sqlite".to_string(),
      env: Env::Dev,
      group: None,
      agent_access: AgentAccess::None,
      credential: CredentialSource::Keychain,
      params: ConnectorParams::Sqlite {
        path: path.to_string_lossy().into_owned(),
      },
    };
    let db = connect_with(profile, Credentials::fixed(None), cx)
      .await
      .expect("opens the sqlite file");

    let session = open_session(&db, cx).await.expect("session");
    run_session_query(
      &session,
      "create table t (id integer primary key, name text)".to_string(),
      cx,
    )
    .await
    .expect("create");
    run_session_query(
      &session,
      "insert into t values (1, 'Ada'), (2, 'Alan')".to_string(),
      cx,
    )
    .await
    .expect("insert");

    // sqlite's schema is "main".
    let sorted = |offset| {
      let mut request = page_request("main", "t", offset, 10);
      request.sort = Some(soquel_core::connectors::SortSpec {
        column: "id".to_string(),
        direction: soquel_core::connectors::SortDirection::Asc,
      });
      request
    };
    let page = fetch_rows(&db, sorted(0), cx).await.expect("fetch");
    let statement = &page.statements[0];
    assert_eq!(statement.rows.len(), 2);

    let mut staged = StagedChanges::default();
    staged.edits.insert(
      0,
      [("name".to_string(), Some("Ada II".to_string()))]
        .into_iter()
        .collect(),
    );
    let changes = crate::staged::build_table_changes(
      &staged,
      &statement.rows,
      &statement.columns,
      &["id".to_string()],
      "main",
      "t",
    );
    let applied = apply_changes(&db, changes, cx).await.expect("apply");
    assert_eq!(applied.updated, 1);

    let page = fetch_rows(&db, sorted(0), cx).await.expect("refetch");
    assert_eq!(page.statements[0].rows[0][1], Some("Ada II".to_string()));
    close_session(session);
  }

  /// The tunnel round end to end: TOFU refusal, the trust the dialog writes,
  /// then a query through the forward. Skipped silently without SOQUEL_TEST_SSH.
  #[gpui::test]
  async fn integration_flow_tunnel_trust_and_connect(cx: &mut gpui::TestAppContext) {
    cx.executor().allow_parking();
    let Some(ssh) = soquel_core::integration_env("SOQUEL_TEST_SSH") else {
      return;
    };
    let (ssh_host, ssh_port) = ssh.split_once(':').expect("host:port");
    let ssh_port: u16 = ssh_port.parse().expect("ssh port");
    let dir = tempfile::tempdir().unwrap();
    let state = std::sync::Arc::new(soquel_core::AppState::for_tests(
      dir.path(),
      Box::new(soquel_core::secrets::InMemoryStore::default()),
    ));

    let tunnel = soquel_core::ops::create_tunnel(
      &state,
      &soquel_core::tunnels::TunnelInput {
        name: "compose sshd".to_string(),
        host: ssh_host.to_string(),
        port: ssh_port,
        user: "tunnel".to_string(),
        auth: soquel_core::tunnels::SshAuth::KeyFile {
          path: concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../scripts/test-ssh/id_ed25519"
          )
          .to_string(),
        },
        credential: CredentialSource::Keychain,
        secret: None,
      },
    )
    .unwrap();

    let input = ConnectionInput {
      name: "through the tunnel".to_string(),
      env: Env::Dev,
      group: None,
      agent_access: AgentAccess::None,
      credential: CredentialSource::Keychain,
      params: ConnectorParams::Postgres(SqlServerParams {
        // The database target as seen from the sshd container.
        host: "postgres".to_string(),
        port: 5432,
        database: "soquel_test".to_string(),
        user: "soquel".to_string(),
        ssl_mode: Default::default(),
        ssl_root_cert: None,
        tunnel_id: Some(tunnel.id.clone()),
      }),
      password: Some("soquel".to_string()),
    };
    let profile = soquel_core::ops::create_connection(&state, &input).unwrap();

    // First contact: the connect is refused with the fields the dialog shows.
    let refused = connect_id(state.clone(), profile.id.clone(), cx).await;
    let Err(soquel_core::error::Error::HostKeyUntrusted {
      host,
      port,
      fingerprint,
      key,
      previously_trusted,
      ..
    }) = refused
    else {
      panic!("first contact must be refused");
    };
    assert!(!previously_trusted);
    assert!(fingerprint.starts_with("SHA256:"));

    // The exact call the trust button makes, then the same retry.
    trust_host_key(state.clone(), host.clone(), port, key.clone(), cx)
      .await
      .unwrap();
    let db = connect_id(state.clone(), profile.id.clone(), cx)
      .await
      .expect("connects through the tunnel once trusted");
    let session = open_session(&db, cx).await.expect("session");
    let result = run_session_query(&session, "select 1".to_string(), cx)
      .await
      .expect("select through the forward");
    assert_eq!(result.statements[0].rows.len(), 1);
    close_session(session);
    // Direct ops need the runtime's reactor: route them through the bridge.
    bridge(cx, {
      let state = state.clone();
      let id = profile.id.clone();
      async move { soquel_core::ops::disconnect(&state, &id).await }
    })
    .await
    .unwrap();

    // The form's Test connection on the stored tunnel now passes.
    test_tunnel(
      state.clone(),
      soquel_core::tunnels::TunnelInput {
        name: tunnel.name.clone(),
        host: tunnel.host.clone(),
        port: tunnel.port,
        user: tunnel.user.clone(),
        auth: tunnel.auth.clone(),
        credential: tunnel.credential.clone(),
        secret: None,
      },
      Some(tunnel.id.clone()),
      cx,
    )
    .await
    .expect("Tunnel OK");
  }

  /// The key browser end to end against a live redis: seed via the console,
  /// scan, read a value per type, isolate a db, ttl + delete. Skipped without
  /// SOQUEL_TEST_REDIS.
  #[gpui::test]
  async fn integration_flow_redis_browse(cx: &mut gpui::TestAppContext) {
    cx.executor().allow_parking();
    use soquel_core::connectors::KeyValue;
    use soquel_core::credentials::Credentials;
    use soquel_core::profiles::RedisParams;

    let Some(coord) = soquel_core::integration_env("SOQUEL_TEST_REDIS") else {
      return;
    };
    let (host, port) = coord.split_once(':').expect("host:port");
    let profile = |db: u32| ConnectionProfile {
      id: format!("redis-flow-{db}"),
      name: "redis flow".to_string(),
      env: Env::Dev,
      group: None,
      agent_access: AgentAccess::None,
      credential: CredentialSource::Prompt,
      params: ConnectorParams::Redis(RedisParams {
        host: host.to_string(),
        port: port.parse().expect("port"),
        db,
        username: None,
        tls: false,
        tunnel_id: None,
      }),
    };
    let db0 = connect_with(
      profile(0),
      Credentials::fixed(Some("soquel".to_string())),
      cx,
    )
    .await
    .expect("connects to the compose redis");

    let prefix = format!("gpui_kv_{}", std::process::id());
    let cmd = |db: &Db, line: String| kv_run_command(db, line, cx);
    for line in [
      format!("DEL {prefix}:str {prefix}:list {prefix}:hash"),
      format!("SET {prefix}:str hello"),
      format!("RPUSH {prefix}:list a b c"),
      format!("HSET {prefix}:hash field1 v1 field2 v2"),
      format!("EXPIRE {prefix}:str 7200"),
    ] {
      cmd(&db0, line).await.expect("seed");
    }

    // The contains-search reaches all three seeded keys.
    let page = kv_scan(&db0, format!("*{prefix}*"), None, 500, cx)
      .await
      .expect("scan");
    let names: Vec<&str> = page.keys.iter().map(|k| k.key.as_str()).collect();
    assert!(names.contains(&format!("{prefix}:str").as_str()));
    assert!(names.contains(&format!("{prefix}:hash").as_str()));

    let str_detail = kv_key_detail(&db0, format!("{prefix}:str"), cx)
      .await
      .expect("detail");
    assert!(matches!(&str_detail.value, KeyValue::String { value } if value == "hello"));
    assert!(str_detail.ttl_ms.is_some());

    let hash = kv_key_detail(&db0, format!("{prefix}:hash"), cx)
      .await
      .expect("detail");
    let KeyValue::Hash { entries } = &hash.value else {
      panic!("hash value");
    };
    assert_eq!(entries.len(), 2);

    // ttl clear + delete roundtrip (db isolation + select are covered by the
    // core suite, which owns the AppState the reconnect needs).
    kv_set_ttl(&db0, format!("{prefix}:str"), None, cx)
      .await
      .expect("persist");
    let persisted = kv_key_detail(&db0, format!("{prefix}:str"), cx)
      .await
      .expect("detail");
    assert!(persisted.ttl_ms.is_none());

    kv_delete_key(&db0, format!("{prefix}:str"), cx)
      .await
      .expect("delete");
    let gone = kv_key_detail(&db0, format!("{prefix}:str"), cx).await;
    assert!(gone.is_err(), "the deleted key is gone");

    // Cleanup.
    for suffix in ["list", "hash"] {
      let _ = cmd(&db0, format!("DEL {prefix}:{suffix}")).await;
    }
  }

  /// The document browser end to end against a live mongo, reading the
  /// compose-seeded `soquel_e2e`. Skipped without SOQUEL_TEST_MONGO.
  #[gpui::test]
  async fn integration_flow_mongo_browse(cx: &mut gpui::TestAppContext) {
    cx.executor().allow_parking();
    use soquel_core::connectors::DocFindRequest;
    use soquel_core::credentials::Credentials;
    use soquel_core::profiles::MongoParams;

    let Some(coord) = soquel_core::integration_env("SOQUEL_TEST_MONGO") else {
      return;
    };
    let (host, port) = coord.split_once(':').expect("host:port");
    let profile = ConnectionProfile {
      id: "mongo-flow".to_string(),
      name: "mongo flow".to_string(),
      env: Env::Dev,
      group: None,
      agent_access: AgentAccess::None,
      credential: CredentialSource::Prompt,
      params: ConnectorParams::Mongo(MongoParams {
        host: host.to_string(),
        port: port.parse().expect("port"),
        database: Some("soquel_e2e".to_string()),
        username: Some("soquel".to_string()),
        auth_source: Some("admin".to_string()),
        tls: false,
        tunnel_id: None,
      }),
    };
    let db = connect_with(profile, Credentials::fixed(Some("soquel".to_string())), cx)
      .await
      .expect("connects to the compose mongo");

    let e2e = "soquel_e2e".to_string();
    let databases = doc_databases(&db, cx).await.expect("databases");
    assert!(databases.iter().any(|d| d.name == e2e));

    let collections = doc_collections(&db, e2e.clone(), cx)
      .await
      .expect("collections");
    assert!(collections.iter().any(|c| c.name == "users"));

    // The seed puts 100 of 200 users on the "pro" plan.
    let filter = Some("{\"plan\":\"pro\"}".to_string());
    let count = doc_count(&db, e2e.clone(), "users".to_string(), filter.clone(), cx)
      .await
      .expect("count");
    assert!(count.exact && count.count == 100.0);

    let page = doc_find(
      &db,
      DocFindRequest {
        db: e2e.clone(),
        collection: "users".to_string(),
        filter: filter.clone(),
        sort: None,
        limit: 100,
        cursor: None,
      },
      cx,
    )
    .await
    .expect("find");
    assert_eq!(page.docs.len(), 100);
    let id = page.docs[0].id.clone().expect("a user has an _id");
    let detail = doc_detail(&db, e2e.clone(), "users".to_string(), id, cx)
      .await
      .expect("detail");
    assert!(detail.relaxed.contains("email"));

    let indexes = doc_indexes(&db, e2e.clone(), "users".to_string(), cx)
      .await
      .expect("indexes");
    assert!(indexes.iter().any(|i| i.name == "email_1" && i.unique));

    // Aggregate groups the two plans; $out is refused (a write stage).
    let grouped = doc_run_query(
      &db,
      e2e.clone(),
      "users".to_string(),
      "[{\"$group\":{\"_id\":\"$plan\",\"n\":{\"$sum\":1}}}]".to_string(),
      cx,
    )
    .await
    .expect("aggregate");
    assert_eq!(grouped.docs.len(), 2);
    let blocked = doc_run_query(
      &db,
      e2e.clone(),
      "users".to_string(),
      "[{\"$out\":\"evil\"}]".to_string(),
      cx,
    )
    .await;
    assert!(blocked.is_err(), "$out is a write stage");

    // Replace + delete a disposable doc (the seed keeps 50, reseeds on restart).
    let disposable = doc_find(
      &db,
      DocFindRequest {
        db: e2e.clone(),
        collection: "disposable".to_string(),
        filter: None,
        sort: None,
        limit: 1,
        cursor: None,
      },
      cx,
    )
    .await
    .expect("disposable");
    let entry = &disposable.docs[0];
    let doc_id = entry.id.clone().expect("string _id");
    doc_replace(
      &db,
      e2e.clone(),
      "disposable".to_string(),
      doc_id.clone(),
      format!("{{\"_id\": {doc_id}, \"note\": \"replaced by gpui\"}}"),
      cx,
    )
    .await
    .expect("replace");
    doc_delete(
      &db,
      e2e.clone(),
      "disposable".to_string(),
      doc_id.clone(),
      cx,
    )
    .await
    .expect("delete");
    let gone = doc_detail(&db, e2e, "disposable".to_string(), doc_id, cx).await;
    assert!(gone.is_err(), "the deleted doc is gone");
  }
}
