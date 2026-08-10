use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use deadpool::managed::{self, Metrics, Object, Pool, RecycleError, RecycleResult};
use tokio_postgres::types::{Kind as PgKind, Type as PgType};
use tokio_postgres::{AsyncMessage, CancelToken, Client, Config, SimpleQueryMessage};

use crate::connectors::{
  verify_exactly_one, ApplyResult, CancelRegistry, Capability, ColumnKind, Connection, Connector,
  Introspect, LocalForward, QueryColumn, QueryResult, RowsChunk, ServerNotice, SqlQuery,
  SqlSession, StatementResult, StreamSummary, TableChanges, TableRowsRequest, CHUNK_ROWS,
  POOL_MAX_SIZE,
};
use crate::credentials::Credentials;
use crate::error::Error;
use crate::profiles::{ConnectionProfile, SqlServerParams, SslMode};

mod browse;
mod introspect;
mod tls;

use browse::{bind_text, build_change_statements, plan_select, ChangeKind, ChangeStatement};

pub struct PostgresConnector;

#[async_trait::async_trait]
impl Connector for PostgresConnector {
  fn capabilities(&self) -> &'static [Capability] {
    &[Capability::SqlQuery, Capability::Introspection]
  }

  async fn connect(
    &self,
    profile: &ConnectionProfile,
    secret: Arc<Credentials>,
    forward: Option<LocalForward>,
  ) -> Result<Box<dyn Connection>, Error> {
    let params = profile
      .params
      .sql_server()
      .ok_or_else(|| Error::Unsupported {
        message: "this connector needs a TCP SQL server profile".to_string(),
      })?;
    let config = build_config(params, forward);
    let connection = PostgresConnection::new(
      config,
      secret,
      params.ssl_mode,
      params.ssl_root_cert.clone(),
    )?;
    // Surface auth/reachability/TLS errors now, not on the first query.
    drop(connection.checkout().await?);
    Ok(Box::new(connection))
  }
}

/// Through a tunnel, TCP dials the local forward (`hostaddr`) while `host`
/// stays the logical hostname: TLS SNI and verify-full target the real server,
/// not 127.0.0.1.
fn build_config(params: &SqlServerParams, forward: Option<LocalForward>) -> Config {
  let mut config = Config::new();
  config.host(&params.host);
  match forward {
    Some(forward) => {
      config.hostaddr(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
      config.port(forward.port);
    }
    None => {
      config.port(params.port);
    }
  }
  config
    .dbname(&params.database)
    .user(&params.user)
    .application_name("soquel")
    .ssl_mode(tls::config_ssl_mode(params.ssl_mode))
    .connect_timeout(Duration::from_secs(10));
  config
}

pub(super) struct PooledPg {
  pub(super) client: Client,
  notices: Arc<Mutex<Vec<ServerNotice>>>,
  server_version: Option<String>,
}

pub(super) struct PgManager {
  config: Config,
  credentials: Arc<Credentials>,
  ssl_mode: SslMode,
  ssl_root_cert: Option<String>,
  server_version: Arc<std::sync::OnceLock<String>>,
}

impl PgManager {
  /// Resolves the password per connection: a token that expired since the
  /// pool was built must not be reused.
  async fn connect(&self) -> Result<PooledPg, Error> {
    let mut config = self.config.clone();
    if let Some(secret) = self.credentials.resolve().await? {
      config.password(secret);
    }
    connect_pg(&config, self.ssl_mode, self.ssl_root_cert.as_deref()).await
  }
}

async fn connect_pg(
  config: &Config,
  ssl_mode: SslMode,
  ssl_root_cert: Option<&str>,
) -> Result<PooledPg, Error> {
  let tls = tls::connector(ssl_mode, ssl_root_cert)?;
  let (client, mut connection) = config.connect(tls).await?;
  let server_version = connection.parameter("server_version").map(str::to_string);
  let notices: Arc<Mutex<Vec<ServerNotice>>> = Arc::default();
  let sink = notices.clone();
  tokio::spawn(async move {
    while let Some(message) = std::future::poll_fn(|cx| connection.poll_message(cx)).await {
      match message {
        Ok(AsyncMessage::Notice(notice)) => sink.lock().unwrap().push(ServerNotice {
          severity: notice.severity().to_string(),
          message: notice.message().to_string(),
        }),
        Ok(_) => {}
        Err(err) => {
          log::warn!("postgres connection closed: {err}");
          break;
        }
      }
    }
  });
  Ok(PooledPg {
    client,
    notices,
    server_version,
  })
}

/// Prepare-for-types, run over the simple protocol, drain this client's notices.
async fn run_script(pg: &PooledPg, sql: &str) -> Result<QueryResult, Error> {
  pg.notices.lock().unwrap().clear();
  let start = Instant::now();
  // Single statements get type metadata from a prepare; multi-statement
  // scripts fail to prepare and degrade to names only.
  let types = pg.client.prepare(sql).await.ok().map(|statement| {
    statement
      .columns()
      .iter()
      .map(|c| c.type_().clone())
      .collect::<Vec<_>>()
  });
  let messages = pg.client.simple_query(sql).await?;
  let mut statements = collect_statements(messages);
  if let (Some(types), [statement]) = (&types, &mut statements[..]) {
    apply_types(statement, types);
  }
  let notices = std::mem::take(&mut *pg.notices.lock().unwrap());
  Ok(QueryResult {
    statements,
    notices,
    duration_ms: start.elapsed().as_secs_f64() * 1000.0,
  })
}

impl managed::Manager for PgManager {
  type Type = PooledPg;
  type Error = Error;

  async fn create(&self) -> Result<PooledPg, Error> {
    let pg = self.connect().await?;
    if let Some(version) = &pg.server_version {
      let _ = self.server_version.set(version.clone());
    }
    Ok(pg)
  }

  async fn recycle(&self, pg: &mut PooledPg, _: &Metrics) -> RecycleResult<Error> {
    if pg.client.is_closed() {
      return Err(RecycleError::message("connection closed"));
    }
    Ok(())
  }
}

pub struct PostgresConnection {
  pool: Pool<PgManager>,
  ssl_mode: SslMode,
  ssl_root_cert: Option<String>,
  server_version: Arc<std::sync::OnceLock<String>>,
  cancels: CancelRegistry<CancelToken>,
}

impl PostgresConnection {
  fn new(
    config: Config,
    credentials: Arc<Credentials>,
    ssl_mode: SslMode,
    ssl_root_cert: Option<String>,
  ) -> Result<Self, Error> {
    let server_version = Arc::new(std::sync::OnceLock::new());
    let pool = Pool::builder(PgManager {
      config,
      credentials,
      ssl_mode,
      ssl_root_cert: ssl_root_cert.clone(),
      server_version: server_version.clone(),
    })
    .max_size(POOL_MAX_SIZE)
    .build()
    .map_err(|err| Error::Database {
      message: format!("connection pool: {err}"),
    })?;
    Ok(Self {
      pool,
      ssl_mode,
      ssl_root_cert,
      server_version,
      cancels: CancelRegistry::default(),
    })
  }

  pub(super) async fn checkout(&self) -> Result<Object<PgManager>, Error> {
    self.pool.get().await.map_err(|err| match err {
      managed::PoolError::Backend(err) => err,
      other => Error::Database {
        message: format!("connection pool: {other}"),
      },
    })
  }

  async fn execute_script(&self, sql: &str) -> Result<QueryResult, Error> {
    let pg = self.checkout().await?;
    let _guard = self.cancels.register(pg.client.cancel_token());
    run_script(&pg, sql).await
  }
}

pub struct PostgresSession {
  pg: PooledPg,
  ssl_mode: SslMode,
  ssl_root_cert: Option<String>,
  cancel: CancelToken,
}

#[async_trait::async_trait]
impl SqlSession for PostgresSession {
  async fn run_query(&self, sql: &str) -> Result<QueryResult, Error> {
    run_script(&self.pg, sql).await
  }

  async fn cancel(&self) -> Result<(), Error> {
    self
      .cancel
      .clone()
      .cancel_query(tls::connector(
        self.ssl_mode,
        self.ssl_root_cert.as_deref(),
      )?)
      .await?;
    Ok(())
  }

  // Dropping the client terminates the connection task.
  async fn close(&self) -> Result<(), Error> {
    Ok(())
  }
}

#[async_trait::async_trait]
impl Connection for PostgresConnection {
  async fn health(&self) -> Result<(), Error> {
    let pg = self.checkout().await?;
    pg.client.simple_query("SELECT 1").await?;
    Ok(())
  }

  async fn close(&self) -> Result<(), Error> {
    self.pool.close();
    Ok(())
  }

  fn server_version(&self) -> Option<String> {
    self.server_version.get().cloned()
  }

  fn sql(&self) -> Option<&dyn SqlQuery> {
    Some(self)
  }

  fn introspect(&self) -> Option<&dyn Introspect> {
    Some(self)
  }
}

#[async_trait::async_trait]
impl SqlQuery for PostgresConnection {
  async fn run_query(&self, sql: &str) -> Result<QueryResult, Error> {
    self.execute_script(sql).await
  }

  async fn run_read_only_query(&self, sql: &str) -> Result<QueryResult, Error> {
    let pg = self.checkout().await?;
    let _guard = self.cancels.register(pg.client.cancel_token());
    // A failed prepare means multiple statements: over the simple protocol
    // `COMMIT; INSERT ...` would escape the read-only transaction.
    pg.client.prepare(sql).await?;
    // SET LOCAL: the cap dies with the transaction, so the pooled connection
    // goes back unchanged.
    pg.client
      .batch_execute(&format!(
        "BEGIN READ ONLY; SET LOCAL statement_timeout = {}",
        crate::connectors::AGENT_STATEMENT_TIMEOUT_MS
      ))
      .await?;
    let result = run_script(&pg, sql).await;
    // Unconditional: a pooled connection must never keep a transaction open.
    let rollback = pg.client.batch_execute("ROLLBACK").await;
    let result = result?;
    rollback?;
    Ok(result)
  }

  async fn cancel(&self) -> Result<(), Error> {
    for token in self.cancels.tokens() {
      token
        .cancel_query(tls::connector(
          self.ssl_mode,
          self.ssl_root_cert.as_deref(),
        )?)
        .await?;
    }
    Ok(())
  }

  async fn table_rows(&self, request: &TableRowsRequest) -> Result<QueryResult, Error> {
    let pg = self.checkout().await?;
    let _guard = self.cancels.register(pg.client.cancel_token());
    pg.notices.lock().unwrap().clear();
    let start = Instant::now();

    let plan = plan_select(&pg, request).await?;
    let bind = bind_text(&plan.params);
    let rows = pg.client.query_typed(&plan.sql, &bind).await?;

    let statement = StatementResult {
      columns: plan.columns,
      rows_affected: rows.len() as f64,
      rows: rows
        .iter()
        .map(|row| (0..row.len()).map(|i| row.get(i)).collect())
        .collect(),
    };
    let notices = std::mem::take(&mut *pg.notices.lock().unwrap());
    Ok(QueryResult {
      statements: vec![statement],
      notices,
      duration_ms: start.elapsed().as_secs_f64() * 1000.0,
    })
  }

  async fn stream_rows(
    &self,
    request: &TableRowsRequest,
    on_chunk: Box<dyn Fn(RowsChunk) -> bool + Send>,
  ) -> Result<StreamSummary, Error> {
    use futures_util::TryStreamExt;

    let pg = self.checkout().await?;
    let _guard = self.cancels.register(pg.client.cancel_token());
    pg.notices.lock().unwrap().clear();
    let start = Instant::now();

    let plan = plan_select(&pg, request).await?;
    let params = plan.params.iter().map(|value| (value, PgType::TEXT));
    let stream = pg.client.query_typed_raw(&plan.sql, params).await?;
    futures_util::pin_mut!(stream);

    let mut columns = Some(plan.columns);
    let mut total = 0u64;
    let mut chunk: Vec<Vec<Option<String>>> = Vec::with_capacity(CHUNK_ROWS);
    while let Some(row) = stream.try_next().await? {
      chunk.push((0..row.len()).map(|i| row.get(i)).collect());
      total += 1;
      if chunk.len() == CHUNK_ROWS
        && !on_chunk(RowsChunk {
          columns: columns.take(),
          rows: std::mem::take(&mut chunk),
        })
      {
        // Receiver gone: stop reading; dropping the stream discards the rest.
        break;
      }
    }
    if columns.is_some() || !chunk.is_empty() {
      on_chunk(RowsChunk {
        columns: columns.take(),
        rows: std::mem::take(&mut chunk),
      });
    }

    let notices = std::mem::take(&mut *pg.notices.lock().unwrap());
    Ok(StreamSummary {
      rows: total as f64,
      duration_ms: start.elapsed().as_secs_f64() * 1000.0,
      notices,
    })
  }

  async fn apply_changes(&self, changes: &TableChanges) -> Result<ApplyResult, Error> {
    let pg = self.checkout().await?;
    let base = format!(
      "SELECT * FROM {}.{}",
      quote_ident(&changes.schema),
      quote_ident(&changes.table)
    );
    let prepared = pg.client.prepare(&base).await?;
    let columns: Vec<(String, PgType)> = prepared
      .columns()
      .iter()
      .map(|c| (c.name().to_string(), c.type_().clone()))
      .collect();
    let statements = build_change_statements(&changes.schema, &changes.table, &columns, changes)?;
    if statements.is_empty() {
      return Err(Error::Unsupported {
        message: "no changes to apply".to_string(),
      });
    }

    let start = Instant::now();
    pg.client.batch_execute("BEGIN").await?;
    match run_change_statements(&pg, &statements).await {
      Ok((updated, inserted, deleted)) => {
        pg.client.batch_execute("COMMIT").await?;
        Ok(ApplyResult {
          updated,
          inserted,
          deleted,
          duration_ms: start.elapsed().as_secs_f64() * 1000.0,
        })
      }
      Err(err) => {
        // The pooled client must go back clean.
        let _ = pg.client.batch_execute("ROLLBACK").await;
        Err(err)
      }
    }
  }

  async fn open_session(&self) -> Result<Box<dyn SqlSession>, Error> {
    let manager = self.pool.manager();
    let pg = manager.connect().await?;
    Ok(Box::new(PostgresSession {
      cancel: pg.client.cancel_token(),
      ssl_mode: manager.ssl_mode,
      ssl_root_cert: manager.ssl_root_cert.clone(),
      pg,
    }))
  }
}

// Identifiers come from the UI: quoting is the injection boundary.
fn quote_ident(ident: &str) -> String {
  format!("\"{}\"", ident.replace('"', "\"\""))
}

async fn run_change_statements(
  pg: &PooledPg,
  statements: &[ChangeStatement],
) -> Result<(u32, u32, u32), Error> {
  use futures_util::TryStreamExt;

  let (mut updated, mut inserted, mut deleted) = (0u32, 0u32, 0u32);
  for statement in statements {
    let params = statement.params.iter().map(|value| (value, PgType::TEXT));
    let stream = pg.client.query_typed_raw(&statement.sql, params).await?;
    futures_util::pin_mut!(stream);
    while stream.try_next().await?.is_some() {}
    let affected = stream.rows_affected().unwrap_or(0);
    match statement.kind {
      ChangeKind::Update | ChangeKind::Delete => {
        verify_exactly_one(
          if statement.kind == ChangeKind::Update {
            "row update"
          } else {
            "row delete"
          },
          affected,
        )?;
        if statement.kind == ChangeKind::Update {
          updated += 1;
        } else {
          deleted += 1;
        }
      }
      ChangeKind::Insert => inserted += u32::try_from(affected).unwrap_or(0),
    }
  }
  Ok((updated, inserted, deleted))
}

fn collect_statements(messages: Vec<SimpleQueryMessage>) -> Vec<StatementResult> {
  let mut statements = Vec::new();
  let mut current = StatementResult::default();
  for message in messages {
    match message {
      SimpleQueryMessage::RowDescription(columns) => {
        current.columns = columns
          .iter()
          .map(|c| QueryColumn {
            name: c.name().to_string(),
            data_type: None,
            kind: ColumnKind::Other,
          })
          .collect();
      }
      SimpleQueryMessage::Row(row) => {
        current.rows.push(
          (0..row.len())
            .map(|i| row.get(i).map(String::from))
            .collect(),
        );
      }
      SimpleQueryMessage::CommandComplete(count) => {
        current.rows_affected = count as f64;
        statements.push(std::mem::take(&mut current));
      }
      _ => {}
    }
  }
  statements
}

fn apply_types(statement: &mut StatementResult, types: &[PgType]) {
  if statement.columns.len() != types.len() {
    return;
  }
  for (column, ty) in statement.columns.iter_mut().zip(types) {
    column.data_type = Some(type_name(ty));
    column.kind = column_kind(ty);
  }
}

fn type_name(ty: &PgType) -> String {
  match ty.kind() {
    PgKind::Array(inner) => format!("{}[]", inner.name()),
    _ => ty.name().to_string(),
  }
}

fn column_kind(ty: &PgType) -> ColumnKind {
  if matches!(ty.kind(), PgKind::Array(_)) {
    ColumnKind::Array
  } else if *ty == PgType::BOOL {
    ColumnKind::Bool
  } else if [
    PgType::INT2,
    PgType::INT4,
    PgType::INT8,
    PgType::FLOAT4,
    PgType::FLOAT8,
    PgType::NUMERIC,
    PgType::OID,
  ]
  .contains(ty)
  {
    ColumnKind::Number
  } else if [PgType::JSON, PgType::JSONB].contains(ty) {
    ColumnKind::Json
  } else if *ty == PgType::BYTEA {
    ColumnKind::Bytes
  } else if [
    PgType::TIMESTAMP,
    PgType::TIMESTAMPTZ,
    PgType::DATE,
    PgType::TIME,
    PgType::TIMETZ,
    PgType::INTERVAL,
  ]
  .contains(ty)
  {
    ColumnKind::DateTime
  } else if *ty == PgType::UUID {
    ColumnKind::Uuid
  } else if [
    PgType::TEXT,
    PgType::VARCHAR,
    PgType::BPCHAR,
    PgType::NAME,
    PgType::CHAR,
  ]
  .contains(ty)
  {
    ColumnKind::Text
  } else {
    ColumnKind::Other
  }
}

#[cfg(test)]
pub mod tests;
