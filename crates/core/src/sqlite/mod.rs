//! SQLite connector: rusqlite behind spawn_blocking, one shared connection.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use rusqlite::fallible_iterator::FallibleIterator;
use rusqlite::types::ValueRef;
use rusqlite::{Batch, InterruptHandle, OpenFlags, Statement};

use crate::connectors::{
  verify_exactly_one, ApplyResult, Capability, ColumnKind, Connection, Connector, Introspect,
  LocalForward, QueryColumn, QueryResult, RowsChunk, SqlQuery, SqlSession, StatementResult,
  StreamSummary, TableChanges, TableRowsRequest, CHUNK_ROWS,
};
use crate::credentials::Credentials;
use crate::error::Error;
use crate::profiles::{ConnectionProfile, ConnectorParams};

mod browse;
mod introspect;

use browse::{build_change_statements, build_select, ChangeKind};

// Identifiers come from the UI: double-quoting is the injection boundary.
pub(super) fn quote_ident(ident: &str) -> String {
  format!("\"{}\"", ident.replace('"', "\"\""))
}

pub struct SqliteConnector;

#[async_trait::async_trait]
impl Connector for SqliteConnector {
  fn capabilities(&self) -> &'static [Capability] {
    &[Capability::SqlQuery, Capability::Introspection]
  }

  async fn connect(
    &self,
    profile: &ConnectionProfile,
    _secret: Arc<Credentials>,
    _forward: Option<LocalForward>,
  ) -> Result<Box<dyn Connection>, Error> {
    let ConnectorParams::Sqlite { path } = &profile.params else {
      return Err(Error::Unsupported {
        message: "this connector needs a sqlite file profile".to_string(),
      });
    };
    let connection = SqliteConnection::open(path.clone()).await?;
    Ok(Box::new(connection))
  }
}

fn open_file(path: &str) -> Result<rusqlite::Connection, Error> {
  // No implicit CREATE: a typo'd path must error, not mint an empty database.
  let conn = rusqlite::Connection::open_with_flags(
    path,
    OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
  )
  .map_err(|err| Error::Database {
    message: format!("cannot open {path}: {err}"),
  })?;
  conn.busy_timeout(std::time::Duration::from_secs(5))?;
  // WAL lets sessions read while the shared connection writes.
  conn.pragma_update(None, "journal_mode", "WAL")?;
  conn.pragma_update(None, "foreign_keys", "ON")?;
  Ok(conn)
}

impl SqliteConnection {
  /// The timeout is a parameter so tests can watch it fire.
  pub(super) async fn read_only_within(
    &self,
    sql: &str,
    timeout: std::time::Duration,
  ) -> Result<QueryResult, Error> {
    let path = self.path.clone();
    let sql = sql.to_string();
    // A separate handle opened SQLITE_OPEN_READ_ONLY: file-level enforcement
    // that no SQL (PRAGMA included) can undo.
    let conn = tokio::task::spawn_blocking(move || open_file_read_only(&path))
      .await
      .map_err(join_error)??;
    // No statement_timeout in sqlite: a timer interrupts this handle only.
    let interrupt = conn.get_interrupt_handle();
    let timer = tokio::spawn(async move {
      tokio::time::sleep(timeout).await;
      interrupt.interrupt();
    });
    let result = tokio::task::spawn_blocking(move || {
      let mut conn = conn;
      run_script(&mut conn, &sql)
    })
    .await
    .map_err(join_error)?;
    timer.abort();
    result
  }
}

fn open_file_read_only(path: &str) -> Result<rusqlite::Connection, Error> {
  let conn = rusqlite::Connection::open_with_flags(
    path,
    OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
  )
  .map_err(|err| Error::Database {
    message: format!("cannot open {path}: {err}"),
  })?;
  conn.busy_timeout(std::time::Duration::from_secs(5))?;
  Ok(conn)
}

pub struct SqliteConnection {
  pub(super) conn: Arc<Mutex<rusqlite::Connection>>,
  /// Sessions open their own file handle.
  path: String,
  interrupt: InterruptHandle,
  server_version: String,
}

impl SqliteConnection {
  async fn open(path: String) -> Result<Self, Error> {
    let conn = tokio::task::spawn_blocking({
      let path = path.clone();
      move || open_file(&path)
    })
    .await
    .map_err(join_error)??;
    Ok(Self {
      interrupt: conn.get_interrupt_handle(),
      conn: Arc::new(Mutex::new(conn)),
      path,
      server_version: rusqlite::version().to_string(),
    })
  }

  /// All sqlite work runs off the async runtime; the mutex serializes it.
  pub(super) async fn exec<T, F>(&self, f: F) -> Result<T, Error>
  where
    T: Send + 'static,
    F: FnOnce(&mut rusqlite::Connection) -> Result<T, Error> + Send + 'static,
  {
    let conn = self.conn.clone();
    tokio::task::spawn_blocking(move || f(&mut conn.lock().unwrap()))
      .await
      .map_err(join_error)?
  }
}

fn join_error(err: tokio::task::JoinError) -> Error {
  Error::Database {
    message: format!("sqlite worker failed: {err}"),
  }
}

#[async_trait::async_trait]
impl Connection for SqliteConnection {
  async fn health(&self) -> Result<(), Error> {
    self
      .exec(|conn| {
        conn.query_row("SELECT 1", [], |_| Ok(()))?;
        Ok(())
      })
      .await
  }

  async fn close(&self) -> Result<(), Error> {
    // The file handle closes when the last Arc drops.
    Ok(())
  }

  fn server_version(&self) -> Option<String> {
    Some(self.server_version.clone())
  }

  fn sql(&self) -> Option<&dyn SqlQuery> {
    Some(self)
  }

  fn introspect(&self) -> Option<&dyn Introspect> {
    Some(self)
  }
}

#[async_trait::async_trait]
impl SqlQuery for SqliteConnection {
  async fn run_query(&self, sql: &str) -> Result<QueryResult, Error> {
    let sql = sql.to_string();
    self.exec(move |conn| run_script(conn, &sql)).await
  }

  async fn run_read_only_query(&self, sql: &str) -> Result<QueryResult, Error> {
    self
      .read_only_within(
        sql,
        std::time::Duration::from_millis(crate::connectors::AGENT_STATEMENT_TIMEOUT_MS.into()),
      )
      .await
  }

  async fn cancel(&self) -> Result<(), Error> {
    self.interrupt.interrupt();
    Ok(())
  }

  async fn table_rows(&self, request: &TableRowsRequest) -> Result<QueryResult, Error> {
    let request = request.clone();
    self
      .exec(move |conn| {
        let start = Instant::now();
        let names = base_columns(conn, &request.schema, &request.table)?;
        let plan = build_select(&names, &request)?;
        let mut stmt = conn.prepare(&plan.sql)?;
        let columns = statement_columns(&stmt);
        let rows = collect_rows(&mut stmt, &plan.params)?;
        Ok(QueryResult {
          statements: vec![StatementResult {
            columns,
            rows_affected: rows.len() as f64,
            rows,
          }],
          notices: Vec::new(),
          duration_ms: start.elapsed().as_secs_f64() * 1000.0,
        })
      })
      .await
  }

  async fn stream_rows(
    &self,
    request: &TableRowsRequest,
    on_chunk: Box<dyn Fn(RowsChunk) -> bool + Send>,
  ) -> Result<StreamSummary, Error> {
    let request = request.clone();
    self
      .exec(move |conn| {
        let start = Instant::now();
        let names = base_columns(conn, &request.schema, &request.table)?;
        let plan = build_select(&names, &request)?;
        let mut stmt = conn.prepare(&plan.sql)?;
        let mut columns = Some(statement_columns(&stmt));
        let width = stmt.column_count();

        let mut total = 0u64;
        let mut chunk: Vec<Vec<Option<String>>> = Vec::with_capacity(CHUNK_ROWS);
        let mut rows = stmt.query(rusqlite::params_from_iter(plan.params.iter()))?;
        while let Some(row) = rows.next()? {
          chunk.push(row_text(row, width)?);
          total += 1;
          if chunk.len() == CHUNK_ROWS
            && !on_chunk(RowsChunk {
              columns: columns.take(),
              rows: std::mem::take(&mut chunk),
            })
          {
            // Receiver gone: stop reading.
            break;
          }
        }
        if columns.is_some() || !chunk.is_empty() {
          on_chunk(RowsChunk {
            columns: columns.take(),
            rows: std::mem::take(&mut chunk),
          });
        }

        Ok(StreamSummary {
          rows: total as f64,
          duration_ms: start.elapsed().as_secs_f64() * 1000.0,
          notices: Vec::new(),
        })
      })
      .await
  }

  async fn apply_changes(&self, changes: &TableChanges) -> Result<ApplyResult, Error> {
    let changes = changes.clone();
    self
      .exec(move |conn| {
        let names = base_columns(conn, &changes.schema, &changes.table)?;
        let statements = build_change_statements(&names, &changes)?;
        if statements.is_empty() {
          return Err(Error::Unsupported {
            message: "no changes to apply".to_string(),
          });
        }

        let start = Instant::now();
        let tx = conn.transaction()?;
        let (mut updated, mut inserted, mut deleted) = (0u32, 0u32, 0u32);
        for statement in &statements {
          let affected = tx.execute(
            &statement.sql,
            rusqlite::params_from_iter(statement.params.iter()),
          )? as u64;
          match statement.kind {
            ChangeKind::Update => {
              verify_exactly_one("row update", affected)?;
              updated += 1;
            }
            ChangeKind::Delete => {
              verify_exactly_one("row delete", affected)?;
              deleted += 1;
            }
            ChangeKind::Insert => inserted += u32::try_from(affected).unwrap_or(0),
          }
        }
        tx.commit()?;
        Ok(ApplyResult {
          updated,
          inserted,
          deleted,
          duration_ms: start.elapsed().as_secs_f64() * 1000.0,
        })
      })
      .await
  }

  async fn open_session(&self) -> Result<Box<dyn SqlSession>, Error> {
    let path = self.path.clone();
    let conn = tokio::task::spawn_blocking(move || open_file(&path))
      .await
      .map_err(join_error)??;
    Ok(Box::new(SqliteSession {
      interrupt: conn.get_interrupt_handle(),
      conn: Arc::new(Mutex::new(conn)),
    }))
  }
}

/// Column identity for filter/sort/change validation, without executing.
fn base_columns(
  conn: &rusqlite::Connection,
  schema: &str,
  table: &str,
) -> Result<Vec<String>, Error> {
  let stmt = conn.prepare(&format!(
    "SELECT * FROM {}.{}",
    quote_ident(schema),
    quote_ident(table)
  ))?;
  Ok(stmt.column_names().iter().map(|s| s.to_string()).collect())
}

pub struct SqliteSession {
  conn: Arc<Mutex<rusqlite::Connection>>,
  interrupt: InterruptHandle,
}

#[async_trait::async_trait]
impl SqlSession for SqliteSession {
  async fn run_query(&self, sql: &str) -> Result<QueryResult, Error> {
    let sql = sql.to_string();
    let conn = self.conn.clone();
    tokio::task::spawn_blocking(move || run_script(&mut conn.lock().unwrap(), &sql))
      .await
      .map_err(join_error)?
  }

  async fn cancel(&self) -> Result<(), Error> {
    self.interrupt.interrupt();
    Ok(())
  }

  async fn close(&self) -> Result<(), Error> {
    Ok(())
  }
}

fn run_script(conn: &mut rusqlite::Connection, sql: &str) -> Result<QueryResult, Error> {
  let start = Instant::now();
  let mut statements = Vec::new();
  let conn = &*conn;
  let mut batch = Batch::new(conn, sql);
  while let Some(mut stmt) = batch.next()? {
    if stmt.column_count() == 0 {
      // changes() is sticky across DDL: only the total_changes delta is real.
      let before = conn.total_changes();
      stmt.raw_execute()?;
      let affected = conn.total_changes().saturating_sub(before);
      statements.push(StatementResult {
        columns: Vec::new(),
        rows: Vec::new(),
        rows_affected: affected as f64,
      });
    } else {
      let columns = statement_columns(&stmt);
      let rows = collect_rows(&mut stmt, &[])?;
      statements.push(StatementResult {
        columns,
        rows_affected: rows.len() as f64,
        rows,
      });
    }
  }
  Ok(QueryResult {
    statements,
    notices: Vec::new(),
    duration_ms: start.elapsed().as_secs_f64() * 1000.0,
  })
}

fn collect_rows(
  stmt: &mut Statement<'_>,
  params: &[String],
) -> Result<Vec<Vec<Option<String>>>, Error> {
  let width = stmt.column_count();
  let mut out = Vec::new();
  let mut rows = stmt.query(rusqlite::params_from_iter(params.iter()))?;
  while let Some(row) = rows.next()? {
    out.push(row_text(row, width)?);
  }
  Ok(out)
}

fn row_text(row: &rusqlite::Row<'_>, width: usize) -> Result<Vec<Option<String>>, Error> {
  (0..width)
    .map(|index| Ok(value_text(row.get_ref(index)?)))
    .collect()
}

fn value_text(value: ValueRef<'_>) -> Option<String> {
  match value {
    ValueRef::Null => None,
    ValueRef::Integer(value) => Some(value.to_string()),
    ValueRef::Real(value) => Some(value.to_string()),
    ValueRef::Text(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
    ValueRef::Blob(bytes) => {
      let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
      Some(format!("0x{hex}"))
    }
  }
}

fn statement_columns(stmt: &Statement<'_>) -> Vec<QueryColumn> {
  stmt
    .columns()
    .iter()
    .map(|column| {
      let decl = column.decl_type().map(str::to_lowercase);
      QueryColumn {
        name: column.name().to_string(),
        kind: column_kind(decl.as_deref()),
        data_type: decl,
      }
    })
    .collect()
}

/// Declared type to display kind, following sqlite's affinity heuristics;
/// expression columns have no declared type.
fn column_kind(decl: Option<&str>) -> ColumnKind {
  let Some(decl) = decl else {
    return ColumnKind::Other;
  };
  if decl.contains("bool") {
    ColumnKind::Bool
  } else if decl.contains("int")
    || decl.contains("real")
    || decl.contains("floa")
    || decl.contains("doub")
    || decl.contains("num")
    || decl.contains("dec")
  {
    ColumnKind::Number
  } else if decl.contains("json") {
    ColumnKind::Json
  } else if decl.contains("date") || decl.contains("time") {
    ColumnKind::DateTime
  } else if decl.contains("char") || decl.contains("clob") || decl.contains("text") {
    ColumnKind::Text
  } else if decl.contains("blob") {
    ColumnKind::Bytes
  } else {
    ColumnKind::Other
  }
}

#[cfg(test)]
mod tests;
