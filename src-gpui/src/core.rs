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
pub struct Db(Arc<dyn Connection>, ConnectorKind);

impl Db {
  pub fn server_version(&self) -> Option<String> {
    self.0.server_version()
  }

  pub fn kind(&self) -> ConnectorKind {
    self.1
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

pub fn connect_with(
  profile: ConnectionProfile,
  secret: Arc<Credentials>,
) -> oneshot::Receiver<Result<Db, Error>> {
  let (tx, rx) = oneshot::channel();
  runtime().spawn(async move {
    let kind = profile.params.kind();
    let result = connector_for(kind)
      .connect(&profile, secret, None)
      .await
      .map(|conn| Db(Arc::from(conn), kind));
    let _ = tx.send(result);
  });
  rx
}

pub fn connect_dev() -> oneshot::Receiver<Result<Db, Error>> {
  let (profile, secret) = dev_profile();
  connect_with(profile, secret)
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
}
