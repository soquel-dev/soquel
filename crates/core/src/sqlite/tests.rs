//! Driver tests against temp-file databases: no external service, always on.

use std::sync::Arc;

use super::browse::{build_change_statements, build_select};
use super::*;
use crate::connectors::{
  CellValue, ColumnFilter, FilterOp, RowDelete, RowInsert, RowUpdate, SortDirection, SortSpec,
  TableKind,
};

fn seed(path: &std::path::Path) {
  let conn = rusqlite::Connection::open(path).unwrap();
  conn
    .execute_batch(
      "CREATE TABLE customers (
         id INTEGER PRIMARY KEY,
         name TEXT NOT NULL,
         email TEXT,
         balance REAL DEFAULT 0,
         avatar BLOB,
         created_at TEXT DEFAULT CURRENT_TIMESTAMP
       );
       CREATE INDEX customers_name_idx ON customers (name);
       CREATE TABLE orders (
         id INTEGER PRIMARY KEY,
         customer_id INTEGER NOT NULL REFERENCES customers (id),
         label TEXT UNIQUE
       );
       CREATE TABLE logs (message TEXT, level TEXT);
       CREATE VIEW recent_orders AS SELECT id, label FROM orders;
       INSERT INTO customers (id, name, email, balance, avatar) VALUES
         (1, 'Ada', 'ada@example.com', 12.5, x'cafe'),
         (2, 'Grace', NULL, 0, NULL),
         (3, 'Linus 50%_off', 'linus@example.com', -3, NULL);
       INSERT INTO orders (id, customer_id, label) VALUES (1, 1, 'first');
       INSERT INTO logs (message, level) VALUES ('boot', 'info'), ('boom', 'error');",
    )
    .unwrap();
}

async fn fixture() -> (tempfile::TempDir, SqliteConnection) {
  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("app.db");
  seed(&path);
  let connection = SqliteConnection::open(path.to_string_lossy().into_owned())
    .await
    .unwrap();
  (dir, connection)
}

fn request(table: &str) -> TableRowsRequest {
  TableRowsRequest {
    schema: "main".to_string(),
    table: table.to_string(),
    limit: Some(100),
    offset: 0,
    sort: None,
    filters: Vec::new(),
    include_ctid: false,
    include_xmin: false,
  }
}

fn cell(column: &str, value: Option<&str>) -> CellValue {
  CellValue {
    column: column.to_string(),
    value: value.map(str::to_string),
  }
}

#[tokio::test]
async fn open_rejects_a_missing_file() {
  let dir = tempfile::tempdir().unwrap();
  let missing = dir.path().join("nope.db");
  let Err(err) = SqliteConnection::open(missing.to_string_lossy().into_owned()).await else {
    panic!("open must fail on a missing file");
  };
  assert!(matches!(err, Error::Database { .. }), "{err:?}");
  assert!(!missing.exists(), "open must not create the file");
}

#[tokio::test]
async fn health_and_version_report() {
  let (_dir, connection) = fixture().await;
  connection.health().await.unwrap();
  let version = connection.server_version().unwrap();
  assert!(version.starts_with('3'), "{version}");
}

#[tokio::test]
async fn run_query_splits_statements_and_renders_values() {
  let (_dir, connection) = fixture().await;
  let result = connection
    .run_query(
      "UPDATE customers SET balance = 99 WHERE id = 2;
       SELECT id, name, email, balance, avatar FROM customers ORDER BY id;
       CREATE TABLE scratch (x);",
    )
    .await
    .unwrap();

  assert_eq!(result.statements.len(), 3);
  assert_eq!(result.statements[0].rows_affected, 1.0);
  assert!(result.statements[0].columns.is_empty());
  // DDL after DML: changes() would leak the update's count here.
  assert_eq!(result.statements[2].rows_affected, 0.0);

  let select = &result.statements[1];
  let names: Vec<&str> = select.columns.iter().map(|c| c.name.as_str()).collect();
  assert_eq!(names, ["id", "name", "email", "balance", "avatar"]);
  assert_eq!(select.columns[0].kind, ColumnKind::Number);
  assert_eq!(select.columns[1].kind, ColumnKind::Text);
  assert_eq!(select.columns[4].kind, ColumnKind::Bytes);
  assert_eq!(select.rows.len(), 3);
  // integer, text, NULL, real, blob as 0xhex
  assert_eq!(select.rows[0][0].as_deref(), Some("1"));
  assert_eq!(select.rows[0][4].as_deref(), Some("0xcafe"));
  assert_eq!(select.rows[1][2], None);
  assert_eq!(select.rows[1][3].as_deref(), Some("99"));
}

#[tokio::test]
async fn run_query_surfaces_sql_errors() {
  let (_dir, connection) = fixture().await;
  let err = connection
    .run_query("SELECT * FROM nope")
    .await
    .unwrap_err();
  assert!(err.to_string().contains("no such table"), "{err}");
}

#[tokio::test]
async fn expression_columns_have_no_declared_type() {
  let (_dir, connection) = fixture().await;
  let result = connection.run_query("SELECT 1 + 1 AS sum").await.unwrap();
  let column = &result.statements[0].columns[0];
  assert_eq!(column.data_type, None);
  assert_eq!(column.kind, ColumnKind::Other);
}

#[tokio::test]
async fn cancel_interrupts_a_running_query() {
  let (_dir, connection) = fixture().await;
  let connection = Arc::new(connection);
  let runner = {
    let connection = connection.clone();
    tokio::spawn(async move {
      connection
        .run_query(
          "WITH RECURSIVE cnt(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM cnt WHERE x < 10000000000)
           SELECT count(*) FROM cnt",
        )
        .await
    })
  };
  tokio::time::sleep(std::time::Duration::from_millis(200)).await;
  connection.cancel().await.unwrap();
  let err = runner.await.unwrap().unwrap_err();
  assert!(err.to_string().contains("interrupted"), "{err}");
}

#[tokio::test]
async fn sessions_keep_their_own_state() {
  let (_dir, connection) = fixture().await;
  let session = connection.open_session().await.unwrap();
  session
    .run_query("CREATE TEMP TABLE scratch (x); INSERT INTO scratch VALUES (1)")
    .await
    .unwrap();
  // Temp tables are per-handle: invisible from the shared connection.
  assert!(connection.run_query("SELECT * FROM scratch").await.is_err());
  let result = session.run_query("SELECT x FROM scratch").await.unwrap();
  assert_eq!(result.statements[0].rows.len(), 1);
  session.close().await.unwrap();
}

#[tokio::test]
async fn stream_rows_chunks_and_sends_columns_once() {
  let (_dir, connection) = fixture().await;
  connection
    .run_query(
      "CREATE TABLE big (n INTEGER);
       INSERT INTO big
       WITH RECURSIVE cnt(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM cnt WHERE x < 450)
       SELECT x FROM cnt;",
    )
    .await
    .unwrap();

  let chunks: Arc<std::sync::Mutex<Vec<RowsChunk>>> = Arc::default();
  let sink = chunks.clone();
  let mut req = request("big");
  req.limit = None;
  let summary = connection
    .stream_rows(
      &req,
      Box::new(move |chunk| {
        sink.lock().unwrap().push(chunk);
        true
      }),
    )
    .await
    .unwrap();

  assert_eq!(summary.rows, 450.0);
  let chunks = chunks.lock().unwrap();
  let sizes: Vec<usize> = chunks.iter().map(|chunk| chunk.rows.len()).collect();
  assert_eq!(sizes, [200, 200, 50]);
  // Column metadata rides the first chunk only.
  assert!(chunks[0].columns.is_some());
  assert!(chunks[1].columns.is_none());
  assert_eq!(chunks[2].rows[49][0].as_deref(), Some("450"));
}

#[tokio::test]
async fn stream_abort_leaves_the_connection_usable() {
  let (_dir, connection) = fixture().await;
  connection
    .run_query(
      "CREATE TABLE big (n INTEGER);
       INSERT INTO big
       WITH RECURSIVE cnt(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM cnt WHERE x < 450)
       SELECT x FROM cnt;",
    )
    .await
    .unwrap();

  let mut req = request("big");
  req.limit = None;
  let summary = connection
    .stream_rows(&req, Box::new(|_chunk| false))
    .await
    .unwrap();
  // The receiver bailed after the first full chunk: reading stops there.
  assert_eq!(summary.rows, 200.0);

  let result = connection
    .run_query("SELECT count(*) FROM big")
    .await
    .unwrap();
  assert_eq!(result.statements[0].rows[0][0].as_deref(), Some("450"));
}

#[tokio::test]
async fn wal_lets_the_shared_connection_read_past_an_open_write() {
  let (_dir, connection) = fixture().await;
  let session = connection.open_session().await.unwrap();
  session
    .run_query(
      "BEGIN IMMEDIATE;
       INSERT INTO customers (id, name) VALUES (50, 'uncommitted')",
    )
    .await
    .unwrap();

  // WAL snapshot: the write lock never blocks or leaks into readers.
  let count = connection
    .run_query("SELECT count(*) FROM customers")
    .await
    .unwrap();
  assert_eq!(count.statements[0].rows[0][0].as_deref(), Some("3"));

  session.run_query("COMMIT").await.unwrap();
  let count = connection
    .run_query("SELECT count(*) FROM customers")
    .await
    .unwrap();
  assert_eq!(count.statements[0].rows[0][0].as_deref(), Some("4"));
  session.close().await.unwrap();
}

#[tokio::test]
async fn table_rows_filters_sorts_and_paginates() {
  let (_dir, connection) = fixture().await;
  let mut req = request("customers");
  req.filters = vec![ColumnFilter {
    column: "name".to_string(),
    op: FilterOp::Contains,
    value: Some("50%_".to_string()),
  }];
  let result = connection.table_rows(&req).await.unwrap();
  let rows = &result.statements[0].rows;
  assert_eq!(rows.len(), 1);
  assert_eq!(rows[0][1].as_deref(), Some("Linus 50%_off"));

  let mut req = request("customers");
  req.sort = Some(SortSpec {
    column: "id".to_string(),
    direction: SortDirection::Desc,
  });
  req.limit = Some(2);
  req.offset = 1;
  let result = connection.table_rows(&req).await.unwrap();
  let ids: Vec<Option<String>> = result.statements[0]
    .rows
    .iter()
    .map(|row| row[0].clone())
    .collect();
  assert_eq!(ids, vec![Some("2".to_string()), Some("1".to_string())]);
}

#[tokio::test]
async fn apply_changes_updates_inserts_and_deletes() {
  let (_dir, connection) = fixture().await;
  let result = connection
    .apply_changes(&TableChanges {
      schema: "main".to_string(),
      table: "customers".to_string(),
      updates: vec![RowUpdate {
        key: vec![cell("id", Some("1"))],
        set: vec![cell("email", None)],
      }],
      inserts: vec![RowInsert {
        values: vec![cell("id", Some("9")), cell("name", Some("New"))],
      }],
      deletes: vec![RowDelete {
        key: vec![cell("id", Some("2"))],
      }],
    })
    .await
    .unwrap();
  assert_eq!((result.updated, result.inserted, result.deleted), (1, 1, 1));

  let rows = connection
    .run_query("SELECT id, name, email FROM customers ORDER BY id")
    .await
    .unwrap();
  let rows = &rows.statements[0].rows;
  assert_eq!(rows.len(), 3);
  assert_eq!(rows[0][2], None);
  assert_eq!(rows[2][1].as_deref(), Some("New"));
}

#[tokio::test]
async fn stale_key_rolls_the_batch_back() {
  let (_dir, connection) = fixture().await;
  let err = connection
    .apply_changes(&TableChanges {
      schema: "main".to_string(),
      table: "customers".to_string(),
      updates: vec![
        RowUpdate {
          key: vec![cell("id", Some("1"))],
          set: vec![cell("name", Some("changed"))],
        },
        RowUpdate {
          key: vec![cell("id", Some("404"))],
          set: vec![cell("name", Some("ghost"))],
        },
      ],
      inserts: vec![],
      deletes: vec![],
    })
    .await
    .unwrap_err();
  assert!(err.to_string().contains("matched 0 rows"), "{err}");

  // First update must not survive the failed batch.
  let rows = connection
    .run_query("SELECT name FROM customers WHERE id = 1")
    .await
    .unwrap();
  assert_eq!(rows.statements[0].rows[0][0].as_deref(), Some("Ada"));
}

#[tokio::test]
async fn rowid_rescue_keys_pk_less_tables() {
  let (_dir, connection) = fixture().await;
  let mut req = request("logs");
  req.include_ctid = true;
  let result = connection.table_rows(&req).await.unwrap();
  let statement = &result.statements[0];
  assert_eq!(statement.columns[0].name, "ctid");
  let rowid = statement.rows[1][0].clone();
  assert_eq!(rowid.as_deref(), Some("2"));

  connection
    .apply_changes(&TableChanges {
      schema: "main".to_string(),
      table: "logs".to_string(),
      updates: vec![RowUpdate {
        key: vec![CellValue {
          column: "ctid".to_string(),
          value: rowid,
        }],
        set: vec![cell("level", Some("warn"))],
      }],
      inserts: vec![],
      deletes: vec![],
    })
    .await
    .unwrap();
  let rows = connection
    .run_query("SELECT level FROM logs ORDER BY rowid")
    .await
    .unwrap();
  assert_eq!(rows.statements[0].rows[1][0].as_deref(), Some("warn"));
}

#[tokio::test]
async fn snapshot_covers_columns_keys_indexes_and_views() {
  let (_dir, connection) = fixture().await;
  let snapshot = connection.schema_snapshot().await.unwrap();
  assert_eq!(snapshot.schemas.len(), 1);
  let schema = &snapshot.schemas[0];
  assert_eq!(schema.name, "main");
  let names: Vec<&str> = schema.tables.iter().map(|t| t.name.as_str()).collect();
  assert_eq!(names, ["customers", "logs", "orders", "recent_orders"]);

  let customers = &schema.tables[0];
  assert_eq!(customers.kind, TableKind::Table);
  assert_eq!(customers.primary_key, ["id"]);
  assert_eq!(customers.estimated_rows, -1.0);
  let email = customers
    .columns
    .iter()
    .find(|c| c.name == "email")
    .unwrap();
  assert!(email.nullable);
  let name = customers.columns.iter().find(|c| c.name == "name").unwrap();
  assert!(!name.nullable);
  assert_eq!(name.data_type, "text");
  let balance = customers
    .columns
    .iter()
    .find(|c| c.name == "balance")
    .unwrap();
  assert_eq!(balance.default.as_deref(), Some("0"));
  assert!(customers
    .indexes
    .iter()
    .any(|i| i.name == "customers_name_idx" && !i.unique));

  let orders = &schema.tables[2];
  assert_eq!(orders.foreign_keys.len(), 1);
  let fk = &orders.foreign_keys[0];
  assert_eq!(fk.columns, ["customer_id"]);
  assert_eq!(fk.referenced_table, "customers");
  assert_eq!(fk.referenced_columns, ["id"]);
  // The UNIQUE constraint surfaces as an auto index with a synthesized definition.
  assert!(orders.indexes.iter().any(|i| i.unique));

  let view = &schema.tables[3];
  assert_eq!(view.kind, TableKind::View);
  assert_eq!(
    view
      .columns
      .iter()
      .map(|c| c.name.as_str())
      .collect::<Vec<_>>(),
    ["id", "label"]
  );
}

#[tokio::test]
async fn table_ddl_returns_create_statements_with_indexes() {
  let (_dir, connection) = fixture().await;
  let ddl = connection.table_ddl("main", "customers").await.unwrap();
  assert!(ddl.starts_with("CREATE TABLE customers"), "{ddl}");
  assert!(ddl.contains("CREATE INDEX customers_name_idx"), "{ddl}");

  let err = connection.table_ddl("main", "nope").await.unwrap_err();
  assert!(matches!(err, Error::NotFound { .. }));
}

#[test]
fn build_select_escapes_like_and_paginates() {
  let columns = vec!["id".to_string(), "name".to_string()];
  let plan = build_select(
    &columns,
    &TableRowsRequest {
      schema: "main".to_string(),
      table: "customers".to_string(),
      limit: Some(1_000_000),
      offset: 20,
      sort: Some(SortSpec {
        column: "id".to_string(),
        direction: SortDirection::Desc,
      }),
      filters: vec![ColumnFilter {
        column: "name".to_string(),
        op: FilterOp::Contains,
        value: Some("50%_\\off".to_string()),
      }],
      include_ctid: false,
      include_xmin: false,
    },
  )
  .unwrap();
  assert_eq!(
    plan.sql,
    "SELECT * FROM \"main\".\"customers\" WHERE \"name\" LIKE ? ESCAPE '\\' \
     ORDER BY \"id\" DESC LIMIT 5000 OFFSET 20"
  );
  assert_eq!(plan.params, vec!["%50\\%\\_\\\\off%"]);
}

#[test]
fn build_select_includes_the_rowid_rescue() {
  let columns = vec!["message".to_string()];
  let mut req = request("logs");
  req.include_ctid = true;
  let plan = build_select(&columns, &req).unwrap();
  assert!(
    plan.sql.starts_with("SELECT rowid AS \"ctid\", * FROM"),
    "{}",
    plan.sql
  );
}

#[test]
fn change_statements_use_is_keys_and_rowid_mapping() {
  let columns = vec!["id".to_string(), "name".to_string()];
  let statements = build_change_statements(
    &columns,
    &TableChanges {
      schema: "main".to_string(),
      table: "t".to_string(),
      updates: vec![RowUpdate {
        key: vec![cell("ctid", Some("7")), cell("name", None)],
        set: vec![cell("name", Some("x"))],
      }],
      deletes: vec![RowDelete {
        key: vec![cell("id", Some("2"))],
      }],
      inserts: vec![RowInsert { values: vec![] }],
    },
  )
  .unwrap();
  assert_eq!(
    statements[0].sql,
    "UPDATE \"main\".\"t\" SET \"name\" = ? WHERE rowid IS ? AND \"name\" IS ?"
  );
  assert_eq!(
    statements[1].sql,
    "DELETE FROM \"main\".\"t\" WHERE \"id\" IS ?"
  );
  assert_eq!(
    statements[2].sql,
    "INSERT INTO \"main\".\"t\" DEFAULT VALUES"
  );
}

#[tokio::test]
async fn read_only_query_blocks_writes() {
  let (_dir, connection) = fixture().await;

  let result = connection
    .run_read_only_query("SELECT name FROM customers ORDER BY id LIMIT 1")
    .await
    .unwrap();
  assert_eq!(result.statements[0].rows[0][0].as_deref(), Some("Ada"));

  let err = connection
    .run_read_only_query("INSERT INTO logs (message, level) VALUES ('x', 'info')")
    .await
    .unwrap_err();
  let Error::Database { message } = err else {
    panic!("expected a database error: {err:?}");
  };
  assert!(message.contains("readonly"), "{message}");

  // PRAGMA cannot undo a handle opened SQLITE_OPEN_READ_ONLY.
  let err = connection
    .run_read_only_query("PRAGMA query_only = 0; DELETE FROM logs")
    .await
    .unwrap_err();
  let Error::Database { message } = err else {
    panic!("expected a database error: {err:?}");
  };
  assert!(message.contains("readonly"), "{message}");

  // The interrupt timer must not fire on a query that finished in time.
  for _ in 0..3 {
    let ok = connection
      .run_read_only_query("SELECT count(*) FROM customers")
      .await
      .unwrap();
    assert_eq!(ok.statements[0].rows[0][0].as_deref(), Some("3"));
  }

  // The shared read-write handle is untouched.
  connection
    .run_query("INSERT INTO logs (message, level) VALUES ('still-writable', 'info')")
    .await
    .unwrap();
}

#[tokio::test]
async fn the_agent_timer_interrupts_a_runaway_query() {
  let (_dir, connection) = fixture().await;

  // An unbounded recursive CTE only ends when something stops it.
  let err = connection
    .read_only_within(
      "WITH RECURSIVE forever(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM forever) \
       SELECT count(*) FROM forever",
      std::time::Duration::from_millis(50),
    )
    .await
    .unwrap_err();
  let Error::Database { message } = err else {
    panic!("expected a database error: {err:?}");
  };
  assert!(message.contains("interrupted"), "{message}");

  // The timer belongs to that one handle: the shared connection still works.
  connection.run_query("SELECT 1").await.unwrap();
}
