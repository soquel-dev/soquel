use std::sync::{Arc, OnceLock};

use futures::channel::oneshot;
use soquel_core::connectors::{
  Connection, QueryResult, SchemaSnapshot, TableRowsRequest, connector_for,
};
use soquel_core::credentials::Credentials;
use soquel_core::error::Error;
use soquel_core::profiles::{
  AgentAccess, ConnectionProfile, ConnectorKind, ConnectorParams, CredentialSource, Env,
  SqlServerParams,
};

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
pub struct Db(Arc<dyn Connection>);

impl Db {
  pub fn server_version(&self) -> Option<String> {
    self.0.server_version()
  }
}

fn env_or(key: &str, default: &str) -> String {
  std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn dev_profile() -> (ConnectionProfile, Arc<Credentials>) {
  let profile = ConnectionProfile {
    id: "gpui-dev".to_string(),
    name: "gpui dev".to_string(),
    env: Env::Dev,
    group: None,
    agent_access: AgentAccess::None,
    credential: CredentialSource::Prompt,
    params: ConnectorParams::Postgres(SqlServerParams {
      host: env_or("SOQUEL_PG_HOST", "localhost"),
      port: env_or("SOQUEL_PG_PORT", "5470").parse().unwrap_or(5470),
      database: env_or("SOQUEL_PG_DATABASE", "soquel_dev"),
      user: env_or("SOQUEL_PG_USER", "soquel"),
      ssl_mode: Default::default(),
      ssl_root_cert: None,
      tunnel_id: None,
    }),
  };
  let secret = Credentials::fixed(Some(env_or("SOQUEL_PG_PASSWORD", "soquel")));
  (profile, secret)
}

pub fn connect_dev() -> oneshot::Receiver<Result<Db, Error>> {
  let (tx, rx) = oneshot::channel();
  runtime().spawn(async move {
    let (profile, secret) = dev_profile();
    let result = connector_for(ConnectorKind::Postgres)
      .connect(&profile, secret, None)
      .await
      .map(|conn| Db(Arc::from(conn)));
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
