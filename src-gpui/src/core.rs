use std::sync::{Arc, OnceLock};

use futures::channel::oneshot;
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

/// Same data layout as the tauri app: dev builds share its /dev subtree so the
/// two frontends see the same connections while both exist.
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

/// A file logger to `<data dir>/logs/soquel[-dev].log`, mirroring the tauri
/// build's levels: Warn everywhere, Info for our own crates, so our lines are
/// not buried under russh, hyper and rustls. Call before `init_state` so the
/// keyring probe is captured. Failures are non-fatal: the app runs without logs.
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
    .level_for("soquel_gpui", log::LevelFilter::Info)
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
pub fn connect_id(state: Arc<AppState>, id: String) -> oneshot::Receiver<Result<Db, Error>> {
  let (tx, rx) = oneshot::channel();
  runtime().spawn(async move {
    let result = async {
      soquel_core::ops::connect(&state, id.clone()).await?;
      let kind = state.profiles.lock().unwrap().get(&id)?.params.kind();
      let connection = soquel_core::ops::active(&state, &id).await?;
      Ok(Db(connection, kind))
    }
    .await;
    let _ = tx.send(result);
  });
  rx
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
) -> oneshot::Receiver<Result<(), Error>> {
  let (tx, rx) = oneshot::channel();
  runtime().spawn(async move {
    let result = soquel_core::ops::test_connection(&state, &input, existing_id.as_deref()).await;
    let _ = tx.send(result);
  });
  rx
}

/// Store writes can touch the OS keychain: off the UI thread like everything else.
pub fn save_connection(
  state: Arc<AppState>,
  editing: Option<String>,
  input: ConnectionInput,
) -> oneshot::Receiver<Result<ConnectionProfile, Error>> {
  let (tx, rx) = oneshot::channel();
  runtime().spawn(async move {
    let result = match editing {
      Some(id) => soquel_core::ops::update_connection(&state, &id, &input),
      None => soquel_core::ops::create_connection(&state, &input),
    };
    let _ = tx.send(result);
  });
  rx
}

pub fn delete_connection(state: Arc<AppState>, id: String) -> oneshot::Receiver<Result<(), Error>> {
  let (tx, rx) = oneshot::channel();
  runtime().spawn(async move {
    let _ = tx.send(soquel_core::ops::delete_connection(&state, &id));
  });
  rx
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
) -> oneshot::Receiver<Result<TunnelProfile, Error>> {
  let (tx, rx) = oneshot::channel();
  runtime().spawn(async move {
    let result = match editing {
      Some(id) => soquel_core::ops::update_tunnel(&state, &id, &input),
      None => soquel_core::ops::create_tunnel(&state, &input),
    };
    let _ = tx.send(result);
  });
  rx
}

pub fn delete_tunnel(state: Arc<AppState>, id: String) -> oneshot::Receiver<Result<(), Error>> {
  let (tx, rx) = oneshot::channel();
  runtime().spawn(async move {
    let _ = tx.send(soquel_core::ops::delete_tunnel(&state, &id));
  });
  rx
}

pub fn test_tunnel(
  state: Arc<AppState>,
  input: TunnelInput,
  existing_id: Option<String>,
) -> oneshot::Receiver<Result<(), Error>> {
  let (tx, rx) = oneshot::channel();
  runtime().spawn(async move {
    let result = soquel_core::ops::test_tunnel(&state, &input, existing_id.as_deref()).await;
    let _ = tx.send(result);
  });
  rx
}

/// Sync like `list_connections`: a mutex and a small JSON write.
pub fn trust_host_key(state: &AppState, host: &str, port: u16, key: &str) -> Result<(), Error> {
  soquel_core::ops::trust_host_key(state, host, port, key)
}

pub fn approve_credential_command(
  state: &AppState,
  subject: SecretSubject,
  id: String,
) -> Result<(), Error> {
  soquel_core::ops::approve_credential_command(state, subject, id)
}

pub fn revoke_credential_command(
  state: &AppState,
  subject: SecretSubject,
  id: String,
) -> Result<(), Error> {
  soquel_core::ops::revoke_credential_command(state, subject, id)
}

pub fn default_ssh_keys() -> Vec<String> {
  soquel_core::ssh::default_key_paths()
}

/// Each frontend injects how a blocked write gets its yes; gpui sends it through
/// a channel the App drains (see `mcp::GpuiApprover`).
pub type ApproverFactory = Arc<dyn Fn() -> Arc<dyn soquel_core::mcp::Approver> + Send + Sync>;

pub fn mcp_configured_port(state: &AppState) -> u16 {
  soquel_core::mcp::configured_port(state)
}

pub fn mcp_status(
  state: Arc<AppState>,
) -> oneshot::Receiver<Result<soquel_core::mcp::McpStatus, Error>> {
  let (tx, rx) = oneshot::channel();
  runtime().spawn(async move {
    let _ = tx.send(soquel_core::mcp::status(&state).await);
  });
  rx
}

/// The server runs on `runtime()`, off the UI thread; `make_approver` is the
/// gpui seam, its answers arriving through the App's approval channel.
pub fn mcp_start(
  state: Arc<AppState>,
  port: u16,
  make_approver: ApproverFactory,
) -> oneshot::Receiver<Result<(), Error>> {
  let (tx, rx) = oneshot::channel();
  runtime().spawn(async move {
    let _ = tx.send(soquel_core::mcp::start(state, port, make_approver).await);
  });
  rx
}

/// Fire-and-forget: launch reads the persisted toggle and brings the server back.
pub fn mcp_autostart(state: Arc<AppState>, make_approver: ApproverFactory) {
  runtime().spawn(async move {
    soquel_core::mcp::autostart(state, make_approver).await;
  });
}

pub fn mcp_stop(state: Arc<AppState>) -> oneshot::Receiver<Result<(), Error>> {
  let (tx, rx) = oneshot::channel();
  runtime().spawn(async move {
    let _ = tx.send(soquel_core::mcp::stop(&state).await);
  });
  rx
}

pub fn mcp_set_port(state: Arc<AppState>, port: u16) -> oneshot::Receiver<Result<(), Error>> {
  let (tx, rx) = oneshot::channel();
  runtime().spawn(async move {
    let _ = tx.send(soquel_core::mcp::set_port(&state, port).await);
  });
  rx
}

pub fn mcp_regenerate_token(state: Arc<AppState>) -> oneshot::Receiver<Result<String, Error>> {
  let (tx, rx) = oneshot::channel();
  runtime().spawn(async move {
    let _ = tx.send(soquel_core::mcp::regenerate_token(&state).await);
  });
  rx
}

pub fn mcp_audit_log(
  state: &AppState,
  limit: usize,
) -> Result<Vec<soquel_core::mcp::AuditEntry>, Error> {
  soquel_core::mcp::audit_log(state, limit)
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
) -> oneshot::Receiver<Vec<soquel_core::mcp::TrustWindowInfo>> {
  let (tx, rx) = oneshot::channel();
  runtime().spawn(async move {
    let _ = tx.send(soquel_core::mcp::trust_windows(&state).await);
  });
  rx
}

pub fn mcp_revoke_trust(
  state: Arc<AppState>,
  session: String,
  connection_id: String,
) -> oneshot::Receiver<()> {
  let (tx, rx) = oneshot::channel();
  runtime().spawn(async move {
    soquel_core::mcp::revoke_trust(&state, &session, &connection_id).await;
    let _ = tx.send(());
  });
  rx
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
) -> oneshot::Receiver<Result<soquel_core::licence::LicenceStatus, Error>> {
  let (tx, rx) = oneshot::channel();
  runtime().spawn(async move {
    let result =
      soquel_core::licence::install(&soquel_core::licence::path(&state.data_dir), &token);
    let _ = tx.send(result);
  });
  rx
}

/// The normal path: the key goes out, a signed file comes back and installs
/// through the same validation as a pasted one. HTTP lives in the core.
pub fn licence_activate(
  state: Arc<AppState>,
  key: String,
) -> oneshot::Receiver<Result<soquel_core::licence::LicenceStatus, Error>> {
  let (tx, rx) = oneshot::channel();
  runtime().spawn(async move {
    let result = async {
      let token = soquel_core::activation::activate(key.trim()).await?;
      soquel_core::licence::install(&soquel_core::licence::path(&state.data_dir), &token)
    }
    .await;
    let _ = tx.send(result);
  });
  rx
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
pub fn diagnostics(state: Arc<AppState>) -> oneshot::Receiver<String> {
  let (tx, rx) = oneshot::channel();
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
  runtime().spawn(async move {
    let block =
      soquel_core::diagnostics::block(&state, env!("CARGO_PKG_VERSION"), build, &log_path).await;
    let _ = tx.send(block);
  });
  rx
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
) -> oneshot::Receiver<Result<soquel_core::transfer::ExportSummary, Error>> {
  let (tx, rx) = oneshot::channel();
  runtime().spawn(async move {
    let _ = tx.send(soquel_core::transfer::export(
      &state,
      &path,
      include_secrets,
      passphrase.as_deref(),
    ));
  });
  rx
}

/// Off the UI thread: an encrypted file derives an argon2 key to open.
pub fn preview_import(
  state: Arc<AppState>,
  path: std::path::PathBuf,
  passphrase: Option<String>,
) -> oneshot::Receiver<Result<soquel_core::transfer::ImportPreview, Error>> {
  let (tx, rx) = oneshot::channel();
  runtime().spawn(async move {
    let _ = tx.send(soquel_core::transfer::preview_file(
      &state,
      &path,
      passphrase.as_deref(),
    ));
  });
  rx
}

pub fn import_connections(
  state: Arc<AppState>,
  path: std::path::PathBuf,
  passphrase: Option<String>,
  with_secrets: bool,
  strategy: soquel_core::transfer::DuplicateStrategy,
) -> oneshot::Receiver<Result<soquel_core::transfer::ImportOutcome, Error>> {
  let (tx, rx) = oneshot::channel();
  runtime().spawn(async move {
    let _ = tx.send(soquel_core::transfer::import_file(
      &state,
      &path,
      passphrase.as_deref(),
      with_secrets,
      strategy,
    ));
  });
  rx
}

/// Test-only direct connect, bypassing the stores.
#[cfg(test)]
pub fn connect_with(
  profile: ConnectionProfile,
  secret: Arc<soquel_core::credentials::Credentials>,
) -> oneshot::Receiver<Result<Db, Error>> {
  let (tx, rx) = oneshot::channel();
  runtime().spawn(async move {
    let kind = profile.params.kind();
    let result = soquel_core::connectors::connector_for(kind)
      .connect(&profile, secret, None)
      .await
      .map(|conn| Db(Arc::from(conn), kind));
    let _ = tx.send(result);
  });
  rx
}

pub fn fetch_rows(
  db: &Db,
  request: TableRowsRequest,
) -> oneshot::Receiver<Result<QueryResult, Error>> {
  let (tx, rx) = oneshot::channel();
  let conn = db.0.clone();
  runtime().spawn(async move {
    let result = match conn.sql() {
      Some(sql) => sql.table_rows(&request).await,
      None => Err(Error::Unsupported {
        message: "connection has no sql surface".to_string(),
      }),
    };
    let _ = tx.send(result);
  });
  rx
}

pub fn apply_changes(
  db: &Db,
  changes: soquel_core::connectors::TableChanges,
) -> oneshot::Receiver<Result<soquel_core::connectors::ApplyResult, Error>> {
  let (tx, rx) = oneshot::channel();
  let conn = db.0.clone();
  runtime().spawn(async move {
    let result = match conn.sql() {
      Some(surface) => surface.apply_changes(&changes).await,
      None => Err(Error::Unsupported {
        message: "connection has no sql surface".to_string(),
      }),
    };
    let _ = tx.send(result);
  });
  rx
}

/// A dedicated client outside the pool: SET and transactions stick,
/// and cancel targets only this session.
#[derive(Clone)]
pub struct Session(Arc<dyn soquel_core::connectors::SqlSession>);

pub fn open_session(db: &Db) -> oneshot::Receiver<Result<Session, Error>> {
  let (tx, rx) = oneshot::channel();
  let conn = db.0.clone();
  runtime().spawn(async move {
    let result = match conn.sql() {
      Some(surface) => surface.open_session().await.map(|s| Session(Arc::from(s))),
      None => Err(Error::Unsupported {
        message: "connection has no sql surface".to_string(),
      }),
    };
    let _ = tx.send(result);
  });
  rx
}

pub fn run_session_query(
  session: &Session,
  sql: String,
) -> oneshot::Receiver<Result<QueryResult, Error>> {
  let (tx, rx) = oneshot::channel();
  let session = session.0.clone();
  runtime().spawn(async move {
    let _ = tx.send(session.run_query(&sql).await);
  });
  rx
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
) -> (
  futures::channel::mpsc::UnboundedReceiver<u64>,
  oneshot::Receiver<Result<soquel_core::connectors::StreamSummary, Error>>,
) {
  let (progress_tx, progress_rx) = futures::channel::mpsc::unbounded();
  let (tx, rx) = oneshot::channel();
  let conn = db.0.clone();
  let kind = db.1;
  runtime().spawn(async move {
    let result = match conn.sql() {
      Some(surface) => {
        soquel_core::export::run_export(surface, &request, format, kind, &path, move |rows| {
          let _ = progress_tx.unbounded_send(rows);
        })
        .await
      }
      None => Err(Error::Unsupported {
        message: "connection has no sql surface".to_string(),
      }),
    };
    let _ = tx.send(result);
  });
  (progress_rx, rx)
}

pub fn table_ddl(
  db: &Db,
  schema: String,
  table: String,
) -> oneshot::Receiver<Result<String, Error>> {
  let (tx, rx) = oneshot::channel();
  let conn = db.0.clone();
  runtime().spawn(async move {
    let result = match conn.introspect() {
      Some(introspect) => introspect.table_ddl(&schema, &table).await,
      None => Err(Error::Unsupported {
        message: "connection has no introspection surface".to_string(),
      }),
    };
    let _ = tx.send(result);
  });
  rx
}

pub fn schema_snapshot(db: &Db) -> oneshot::Receiver<Result<SchemaSnapshot, Error>> {
  let (tx, rx) = oneshot::channel();
  let conn = db.0.clone();
  runtime().spawn(async move {
    let result = match conn.introspect() {
      Some(introspect) => introspect.schema_snapshot().await,
      None => Err(Error::Unsupported {
        message: "connection has no introspection surface".to_string(),
      }),
    };
    let _ = tx.send(result);
  });
  rx
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
) -> oneshot::Receiver<Result<KeyScanPage, Error>> {
  let (tx, rx) = oneshot::channel();
  let conn = db.0.clone();
  runtime().spawn(async move {
    let result = match conn.kv() {
      Some(kv) => kv.scan_keys(&pattern, cursor.as_deref(), count).await,
      None => Err(no_kv()),
    };
    let _ = tx.send(result);
  });
  rx
}

pub fn kv_key_detail(db: &Db, key: String) -> oneshot::Receiver<Result<KeyDetail, Error>> {
  let (tx, rx) = oneshot::channel();
  let conn = db.0.clone();
  runtime().spawn(async move {
    let result = match conn.kv() {
      Some(kv) => kv.key_detail(&key).await,
      None => Err(no_kv()),
    };
    let _ = tx.send(result);
  });
  rx
}

pub fn kv_databases(db: &Db) -> oneshot::Receiver<Result<KvDatabases, Error>> {
  let (tx, rx) = oneshot::channel();
  let conn = db.0.clone();
  runtime().spawn(async move {
    let result = match conn.kv() {
      Some(kv) => kv.databases().await,
      None => Err(no_kv()),
    };
    let _ = tx.send(result);
  });
  rx
}

pub fn kv_run_command(db: &Db, command: String) -> oneshot::Receiver<Result<Vec<String>, Error>> {
  let (tx, rx) = oneshot::channel();
  let conn = db.0.clone();
  runtime().spawn(async move {
    let result = match conn.kv() {
      Some(kv) => kv.run_command(&command).await,
      None => Err(no_kv()),
    };
    let _ = tx.send(result);
  });
  rx
}

pub fn kv_set_string(db: &Db, key: String, value: String) -> oneshot::Receiver<Result<(), Error>> {
  let (tx, rx) = oneshot::channel();
  let conn = db.0.clone();
  runtime().spawn(async move {
    let result = match conn.kv() {
      Some(kv) => kv.set_string(&key, &value).await,
      None => Err(no_kv()),
    };
    let _ = tx.send(result);
  });
  rx
}

pub fn kv_delete_key(db: &Db, key: String) -> oneshot::Receiver<Result<(), Error>> {
  let (tx, rx) = oneshot::channel();
  let conn = db.0.clone();
  runtime().spawn(async move {
    let result = match conn.kv() {
      Some(kv) => kv.delete_key(&key).await,
      None => Err(no_kv()),
    };
    let _ = tx.send(result);
  });
  rx
}

pub fn kv_set_ttl(
  db: &Db,
  key: String,
  ttl_ms: Option<f64>,
) -> oneshot::Receiver<Result<(), Error>> {
  let (tx, rx) = oneshot::channel();
  let conn = db.0.clone();
  runtime().spawn(async move {
    let result = match conn.kv() {
      Some(kv) => kv.set_ttl(&key, ttl_ms).await,
      None => Err(no_kv()),
    };
    let _ = tx.send(result);
  });
  rx
}

/// Switching db is a reconnect (a SELECT reverts on the multiplexed socket):
/// the core swaps the stored connection, we hand back a fresh Db.
pub fn kv_select_db(
  state: Arc<AppState>,
  id: String,
  db: u32,
) -> oneshot::Receiver<Result<Db, Error>> {
  let (tx, rx) = oneshot::channel();
  runtime().spawn(async move {
    let result = async {
      state.select_kv_db(&id, db).await?;
      let kind = state.profiles.lock().unwrap().get(&id)?.params.kind();
      let connection = soquel_core::ops::active(&state, &id).await?;
      Ok(Db(connection, kind))
    }
    .await;
    let _ = tx.send(result);
  });
  rx
}

fn no_doc() -> Error {
  Error::Unsupported {
    message: "connection does not browse documents".to_string(),
  }
}

pub fn doc_databases(db: &Db) -> oneshot::Receiver<Result<Vec<DocDatabase>, Error>> {
  let (tx, rx) = oneshot::channel();
  let conn = db.0.clone();
  runtime().spawn(async move {
    let result = match conn.doc() {
      Some(doc) => doc.databases().await,
      None => Err(no_doc()),
    };
    let _ = tx.send(result);
  });
  rx
}

pub fn doc_collections(
  db: &Db,
  database: String,
) -> oneshot::Receiver<Result<Vec<DocCollection>, Error>> {
  let (tx, rx) = oneshot::channel();
  let conn = db.0.clone();
  runtime().spawn(async move {
    let result = match conn.doc() {
      Some(doc) => doc.collections(&database).await,
      None => Err(no_doc()),
    };
    let _ = tx.send(result);
  });
  rx
}

pub fn doc_find(db: &Db, request: DocFindRequest) -> oneshot::Receiver<Result<DocPage, Error>> {
  let (tx, rx) = oneshot::channel();
  let conn = db.0.clone();
  runtime().spawn(async move {
    let result = match conn.doc() {
      Some(doc) => doc.find_docs(&request).await,
      None => Err(no_doc()),
    };
    let _ = tx.send(result);
  });
  rx
}

pub fn doc_detail(
  db: &Db,
  database: String,
  collection: String,
  id: String,
) -> oneshot::Receiver<Result<DocDetail, Error>> {
  let (tx, rx) = oneshot::channel();
  let conn = db.0.clone();
  runtime().spawn(async move {
    let result = match conn.doc() {
      Some(doc) => doc.doc_detail(&database, &collection, &id).await,
      None => Err(no_doc()),
    };
    let _ = tx.send(result);
  });
  rx
}

pub fn doc_indexes(
  db: &Db,
  database: String,
  collection: String,
) -> oneshot::Receiver<Result<Vec<IndexInfo>, Error>> {
  let (tx, rx) = oneshot::channel();
  let conn = db.0.clone();
  runtime().spawn(async move {
    let result = match conn.doc() {
      Some(doc) => doc.indexes(&database, &collection).await,
      None => Err(no_doc()),
    };
    let _ = tx.send(result);
  });
  rx
}

pub fn doc_count(
  db: &Db,
  database: String,
  collection: String,
  filter: Option<String>,
) -> oneshot::Receiver<Result<DocCount, Error>> {
  let (tx, rx) = oneshot::channel();
  let conn = db.0.clone();
  runtime().spawn(async move {
    let result = match conn.doc() {
      Some(doc) => {
        doc
          .count_docs(&database, &collection, filter.as_deref())
          .await
      }
      None => Err(no_doc()),
    };
    let _ = tx.send(result);
  });
  rx
}

pub fn doc_run_query(
  db: &Db,
  database: String,
  collection: String,
  source: String,
) -> oneshot::Receiver<Result<DocQueryResult, Error>> {
  let (tx, rx) = oneshot::channel();
  let conn = db.0.clone();
  runtime().spawn(async move {
    let result = match conn.doc() {
      Some(doc) => doc.run_query(&database, &collection, &source).await,
      None => Err(no_doc()),
    };
    let _ = tx.send(result);
  });
  rx
}

pub fn doc_replace(
  db: &Db,
  database: String,
  collection: String,
  id: String,
  doc_json: String,
) -> oneshot::Receiver<Result<(), Error>> {
  let (tx, rx) = oneshot::channel();
  let conn = db.0.clone();
  runtime().spawn(async move {
    let result = match conn.doc() {
      Some(doc) => {
        doc
          .replace_doc(&database, &collection, &id, &doc_json)
          .await
      }
      None => Err(no_doc()),
    };
    let _ = tx.send(result);
  });
  rx
}

pub fn doc_delete(
  db: &Db,
  database: String,
  collection: String,
  id: String,
) -> oneshot::Receiver<Result<(), Error>> {
  let (tx, rx) = oneshot::channel();
  let conn = db.0.clone();
  runtime().spawn(async move {
    let result = match conn.doc() {
      Some(doc) => doc.delete_doc(&database, &collection, &id).await,
      None => Err(no_doc()),
    };
    let _ = tx.send(result);
  });
  rx
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
    // The tauri identifier and the /dev split must match the installed app.
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
  #[tokio::test]
  async fn integration_flow_browse_stage_apply() {
    let Ok(url) = std::env::var("SOQUEL_TEST_PG") else {
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
    let db = connect_with(profile, Credentials::fixed(Some(pass)))
      .await
      .expect("channel")
      .expect("connects to the compose postgres");

    let table = format!("gpui_flow_{}", std::process::id());
    let session = open_session(&db).await.expect("channel").expect("session");
    run_session_query(
      &session,
      format!("create table {table} (id int primary key, name text)"),
    )
    .await
    .expect("channel")
    .expect("create");
    run_session_query(
      &session,
      format!("insert into {table} values (1, 'Ada'), (2, 'Alan')"),
    )
    .await
    .expect("channel")
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
    let page = fetch_rows(&db, sorted(0))
      .await
      .expect("channel")
      .expect("fetch");
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
    let applied = apply_changes(&db, changes)
      .await
      .expect("channel")
      .expect("apply");
    assert_eq!(applied.updated, 1);

    // The reread sees the committed value.
    let page = fetch_rows(&db, sorted(0))
      .await
      .expect("channel")
      .expect("refetch");
    assert_eq!(page.statements[0].rows[0][1], Some("Ada II".to_string()));

    let _ = run_session_query(&session, format!("drop table {table}"))
      .await
      .expect("channel");
    close_session(session);
  }

  /// The tunnel round end to end: TOFU refusal, the trust the dialog writes,
  /// then a query through the forward. Skipped silently without SOQUEL_TEST_SSH.
  #[tokio::test]
  async fn integration_flow_tunnel_trust_and_connect() {
    let Ok(ssh) = std::env::var("SOQUEL_TEST_SSH") else {
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
            "/../scripts/test-ssh/id_ed25519"
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
    let refused = connect_id(state.clone(), profile.id.clone())
      .await
      .expect("channel");
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
    trust_host_key(&state, &host, port, &key).unwrap();
    let db = connect_id(state.clone(), profile.id.clone())
      .await
      .expect("channel")
      .expect("connects through the tunnel once trusted");
    let session = open_session(&db).await.expect("channel").expect("session");
    let result = run_session_query(&session, "select 1".to_string())
      .await
      .expect("channel")
      .expect("select through the forward");
    assert_eq!(result.statements[0].rows.len(), 1);
    close_session(session);
    soquel_core::ops::disconnect(&state, &profile.id)
      .await
      .unwrap();

    // The form's Test connection on the stored tunnel now passes.
    soquel_core::ops::test_tunnel(
      &state,
      &soquel_core::tunnels::TunnelInput {
        name: tunnel.name.clone(),
        host: tunnel.host.clone(),
        port: tunnel.port,
        user: tunnel.user.clone(),
        auth: tunnel.auth.clone(),
        credential: tunnel.credential.clone(),
        secret: None,
      },
      Some(&tunnel.id),
    )
    .await
    .expect("Tunnel OK");
  }

  /// The key browser end to end against a live redis: seed via the console,
  /// scan, read a value per type, isolate a db, ttl + delete. Skipped without
  /// SOQUEL_TEST_REDIS.
  #[tokio::test]
  async fn integration_flow_redis_browse() {
    use soquel_core::connectors::KeyValue;
    use soquel_core::credentials::Credentials;
    use soquel_core::profiles::RedisParams;

    let Ok(coord) = std::env::var("SOQUEL_TEST_REDIS") else {
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
    let db0 = connect_with(profile(0), Credentials::fixed(Some("soquel".to_string())))
      .await
      .expect("channel")
      .expect("connects to the compose redis");

    let prefix = format!("gpui_kv_{}", std::process::id());
    let cmd = |db: &Db, line: String| kv_run_command(db, line);
    for line in [
      format!("DEL {prefix}:str {prefix}:list {prefix}:hash"),
      format!("SET {prefix}:str hello"),
      format!("RPUSH {prefix}:list a b c"),
      format!("HSET {prefix}:hash field1 v1 field2 v2"),
      format!("EXPIRE {prefix}:str 7200"),
    ] {
      cmd(&db0, line).await.expect("channel").expect("seed");
    }

    // The contains-search reaches all three seeded keys.
    let page = kv_scan(&db0, format!("*{prefix}*"), None, 500)
      .await
      .expect("channel")
      .expect("scan");
    let names: Vec<&str> = page.keys.iter().map(|k| k.key.as_str()).collect();
    assert!(names.contains(&format!("{prefix}:str").as_str()));
    assert!(names.contains(&format!("{prefix}:hash").as_str()));

    let str_detail = kv_key_detail(&db0, format!("{prefix}:str"))
      .await
      .expect("channel")
      .expect("detail");
    assert!(matches!(&str_detail.value, KeyValue::String { value } if value == "hello"));
    assert!(str_detail.ttl_ms.is_some());

    let hash = kv_key_detail(&db0, format!("{prefix}:hash"))
      .await
      .expect("channel")
      .expect("detail");
    let KeyValue::Hash { entries } = &hash.value else {
      panic!("hash value");
    };
    assert_eq!(entries.len(), 2);

    // ttl clear + delete roundtrip (db isolation + select are covered by the
    // core suite, which owns the AppState the reconnect needs).
    kv_set_ttl(&db0, format!("{prefix}:str"), None)
      .await
      .expect("channel")
      .expect("persist");
    let persisted = kv_key_detail(&db0, format!("{prefix}:str"))
      .await
      .expect("channel")
      .expect("detail");
    assert!(persisted.ttl_ms.is_none());

    kv_delete_key(&db0, format!("{prefix}:str"))
      .await
      .expect("channel")
      .expect("delete");
    let gone = kv_key_detail(&db0, format!("{prefix}:str"))
      .await
      .expect("channel");
    assert!(gone.is_err(), "the deleted key is gone");

    // Cleanup.
    for suffix in ["list", "hash"] {
      let _ = cmd(&db0, format!("DEL {prefix}:{suffix}")).await;
    }
  }

  /// The document browser end to end against a live mongo, reading the
  /// compose-seeded `soquel_e2e`. Skipped without SOQUEL_TEST_MONGO.
  #[tokio::test]
  async fn integration_flow_mongo_browse() {
    use soquel_core::connectors::DocFindRequest;
    use soquel_core::credentials::Credentials;
    use soquel_core::profiles::MongoParams;

    let Ok(coord) = std::env::var("SOQUEL_TEST_MONGO") else {
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
    let db = connect_with(profile, Credentials::fixed(Some("soquel".to_string())))
      .await
      .expect("channel")
      .expect("connects to the compose mongo");

    let e2e = "soquel_e2e".to_string();
    let databases = doc_databases(&db)
      .await
      .expect("channel")
      .expect("databases");
    assert!(databases.iter().any(|d| d.name == e2e));

    let collections = doc_collections(&db, e2e.clone())
      .await
      .expect("channel")
      .expect("collections");
    assert!(collections.iter().any(|c| c.name == "users"));

    // The seed puts 100 of 200 users on the "pro" plan.
    let filter = Some("{\"plan\":\"pro\"}".to_string());
    let count = doc_count(&db, e2e.clone(), "users".to_string(), filter.clone())
      .await
      .expect("channel")
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
    )
    .await
    .expect("channel")
    .expect("find");
    assert_eq!(page.docs.len(), 100);
    let id = page.docs[0].id.clone().expect("a user has an _id");
    let detail = doc_detail(&db, e2e.clone(), "users".to_string(), id)
      .await
      .expect("channel")
      .expect("detail");
    assert!(detail.relaxed.contains("email"));

    let indexes = doc_indexes(&db, e2e.clone(), "users".to_string())
      .await
      .expect("channel")
      .expect("indexes");
    assert!(indexes.iter().any(|i| i.name == "email_1" && i.unique));

    // Aggregate groups the two plans; $out is refused (a write stage).
    let grouped = doc_run_query(
      &db,
      e2e.clone(),
      "users".to_string(),
      "[{\"$group\":{\"_id\":\"$plan\",\"n\":{\"$sum\":1}}}]".to_string(),
    )
    .await
    .expect("channel")
    .expect("aggregate");
    assert_eq!(grouped.docs.len(), 2);
    let blocked = doc_run_query(
      &db,
      e2e.clone(),
      "users".to_string(),
      "[{\"$out\":\"evil\"}]".to_string(),
    )
    .await
    .expect("channel");
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
    )
    .await
    .expect("channel")
    .expect("disposable");
    let entry = &disposable.docs[0];
    let doc_id = entry.id.clone().expect("string _id");
    doc_replace(
      &db,
      e2e.clone(),
      "disposable".to_string(),
      doc_id.clone(),
      format!("{{\"_id\": {doc_id}, \"note\": \"replaced by gpui\"}}"),
    )
    .await
    .expect("channel")
    .expect("replace");
    doc_delete(&db, e2e.clone(), "disposable".to_string(), doc_id.clone())
      .await
      .expect("channel")
      .expect("delete");
    let gone = doc_detail(&db, e2e, "disposable".to_string(), doc_id)
      .await
      .expect("channel");
    assert!(gone.is_err(), "the deleted doc is gone");
  }
}
