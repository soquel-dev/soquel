use std::sync::{Arc, OnceLock};

use futures::channel::oneshot;
use soquel_core::AppState;
use soquel_core::connectors::{Connection, QueryResult, SchemaSnapshot, TableRowsRequest};
use soquel_core::error::{Error, SecretSubject};
use soquel_core::profiles::{ConnectionInput, ConnectionProfile, ConnectorKind};
use soquel_core::tunnels::{TunnelInput, TunnelProfile};

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

pub fn init_state() -> Result<Arc<AppState>, Error> {
  let override_dir = std::env::var("SOQUEL_DATA_DIR").ok();
  let xdg = std::env::var_os("XDG_DATA_HOME").map(std::path::PathBuf::from);
  let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
  let data_dir = resolve_data_dir(
    override_dir.as_deref(),
    xdg.as_deref(),
    home.as_deref(),
    cfg!(debug_assertions),
  );
  std::fs::create_dir_all(&data_dir)?;
  let secrets = soquel_core::secrets::store_from_env(&data_dir)?;
  Ok(Arc::new(AppState::load(&data_dir, secrets)?))
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

pub fn default_ssh_keys() -> Vec<String> {
  soquel_core::ssh::default_key_paths()
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
}
