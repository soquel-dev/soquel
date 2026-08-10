use super::*;
use crate::connectors::{Connector, TableKind};
use crate::profiles::{ConnectorParams, Env};

#[test]
fn quote_ident_doubles_backticks() {
  assert_eq!(quote_ident("plain"), "`plain`");
  assert_eq!(quote_ident("weird`name"), "`weird``name`");
}

fn profile_with_ssl(ssl_mode: SslMode) -> Option<ConnectionProfile> {
  let addr = std::env::var("SOQUEL_TEST_MYSQL").ok()?;
  let (host, port) = addr
    .split_once(':')
    .expect("SOQUEL_TEST_MYSQL is host:port");
  Some(ConnectionProfile {
    id: String::new(),
    name: "test".to_string(),
    env: Env::Dev,
    group: None,
    agent_access: Default::default(),
    credential: Default::default(),
    params: ConnectorParams::Mysql(SqlServerParams {
      host: host.to_string(),
      port: port.parse().unwrap(),
      database: "soquel_test".to_string(),
      user: "soquel".to_string(),
      ssl_mode,
      ssl_root_cert: None,
      tunnel_id: None,
    }),
  })
}

fn profile_from_env() -> Option<ConnectionProfile> {
  profile_with_ssl(SslMode::Prefer)
}

/// Prints the password and records each run, so a test can tell how many times
/// the pool asked for it.
fn counting_password_script(dir: &tempfile::TempDir) -> (String, std::path::PathBuf) {
  let runs = dir.path().join("runs");
  let script = dir.path().join("password.sh");
  std::fs::write(
    &script,
    format!(
      "#!/bin/sh\necho run >> {}\nprintf %s soquel\n",
      runs.display()
    ),
  )
  .unwrap();
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
  }
  (script.to_string_lossy().into_owned(), runs)
}

#[tokio::test]
async fn integration_mysql_password_from_a_command() {
  let Some(profile) = profile_from_env() else {
    return;
  };
  let spec = crate::credentials::parse_command("printf %s soquel").unwrap();
  let connection = MysqlConnector
    .connect(
      &profile,
      Credentials::command(spec, std::time::Duration::from_secs(300)),
      None,
    )
    .await
    .unwrap();
  connection.health().await.unwrap();
  connection.close().await.unwrap();
}

/// mysql_async freezes the password inside the pool's Opts: an expired token
/// means the pool itself has to be rebuilt.
#[tokio::test]
async fn integration_mysql_expired_command_password_rebuilds_the_pool() {
  let Some(profile) = profile_from_env() else {
    return;
  };
  let dir = tempfile::tempdir().unwrap();
  let (script, runs) = counting_password_script(&dir);
  let spec = crate::credentials::parse_command(&script).unwrap();
  let connection = MysqlConnector
    .connect(
      &profile,
      Credentials::command(spec, std::time::Duration::from_millis(1)),
      None,
    )
    .await
    .unwrap();

  // Both a pooled checkout and a detached session must pick up a fresh token.
  connection.health().await.unwrap();
  let session = connection.sql().unwrap().open_session().await.unwrap();
  session.run_query("SELECT 1").await.unwrap();
  // The swap disconnects the stale pool: the live one still serves queries.
  connection
    .sql()
    .unwrap()
    .run_query("SELECT 1")
    .await
    .unwrap();

  let count = std::fs::read_to_string(&runs).unwrap().lines().count();
  assert!(count >= 3, "the command ran {count} time(s)");
  session.close().await.unwrap();
  connection.close().await.unwrap();
}

/// KILL QUERY on a SLEEP: mysql returns 1, mariadb raises ER_QUERY_INTERRUPTED.
fn assert_interrupted_sleep(outcome: Result<QueryResult, Error>) {
  match outcome {
    Ok(result) => assert_eq!(result.statements[0].rows[0][0].as_deref(), Some("1")),
    Err(Error::Database { message }) => {
      assert!(message.contains("interrupted"), "{message}")
    }
    Err(other) => panic!("unexpected error kind: {other:?}"),
  }
}

fn is_mariadb(connection: &dyn Connection) -> bool {
  connection
    .server_version()
    .is_some_and(|version| version.contains("MariaDB"))
}

async fn test_connection_from_env() -> Option<Box<dyn Connection>> {
  let profile = profile_from_env()?;
  Some(
    MysqlConnector
      .connect(
        &profile,
        Credentials::fixed(Some("soquel".to_string())),
        None,
      )
      .await
      .unwrap(),
  )
}

#[tokio::test]
async fn integration_mysql_query_roundtrip_with_multi_statements() {
  let Some(connection) = test_connection_from_env().await else {
    return;
  };
  connection.health().await.unwrap();

  let sql = connection.sql().unwrap();
  let result = sql
    .run_query("SELECT 1 AS one; SELECT name FROM customers WHERE email IS NULL")
    .await
    .unwrap();
  assert_eq!(result.statements.len(), 2);
  assert_eq!(result.statements[0].columns[0].name, "one");
  assert_eq!(result.statements[0].rows[0][0].as_deref(), Some("1"));
  assert_eq!(
    result.statements[1].rows[0][0].as_deref(),
    Some("Grace Hopper")
  );

  let ddl = sql
    .run_query("CREATE TEMPORARY TABLE tmp_probe (id INT); DROP TEMPORARY TABLE tmp_probe")
    .await
    .unwrap();
  assert_eq!(ddl.statements.len(), 2);
  assert!(ddl.statements.iter().all(|s| s.columns.is_empty()));
}

#[tokio::test]
async fn integration_mysql_values_render_as_text() {
  let Some(connection) = test_connection_from_env().await else {
    return;
  };
  let result = connection
    .sql()
    .unwrap()
    .run_query(
      "SELECT c.id, c.name, c.email, c.meta, o.amount, o.receipt, o.placed_at \
       FROM customers c JOIN orders o ON o.customer_id = c.id \
       WHERE o.note = 'first order'",
    )
    .await
    .unwrap();
  let statement = &result.statements[0];
  let kinds: Vec<ColumnKind> = statement.columns.iter().map(|c| c.kind).collect();
  // mariadb has no real JSON type (LONGTEXT alias): the column reads as text.
  let json_kind = if is_mariadb(connection.as_ref()) {
    ColumnKind::Text
  } else {
    ColumnKind::Json
  };
  assert_eq!(
    kinds,
    vec![
      ColumnKind::Number,
      ColumnKind::Text,
      ColumnKind::Text,
      json_kind,
      ColumnKind::Number,
      ColumnKind::Bytes,
      ColumnKind::DateTime,
    ]
  );
  let row = &statement.rows[0];
  assert_eq!(row[0].as_deref(), Some("1"));
  assert_eq!(row[1].as_deref(), Some("Ada Lovelace"));
  assert!(row[3].as_deref().unwrap().contains("\"plan\""), "{row:?}");
  assert_eq!(row[4].as_deref(), Some("129.90"));
  assert_eq!(row[5].as_deref(), Some("0xdeadbeef"));

  let nulls = connection
    .sql()
    .unwrap()
    .run_query("SELECT email, meta FROM customers WHERE name = 'Grace Hopper'")
    .await
    .unwrap();
  assert_eq!(nulls.statements[0].rows[0], vec![None, None]);

  // Formatting branches: date-only, negative time, float.
  let literals = connection
    .sql()
    .unwrap()
    .run_query(
      "SELECT DATE '2026-01-02' AS d, CAST('-25:00:00' AS TIME) AS t, CAST(1.5 AS FLOAT) AS f",
    )
    .await
    .unwrap();
  let row = &literals.statements[0].rows[0];
  assert_eq!(row[0].as_deref(), Some("2026-01-02"));
  assert_eq!(row[1].as_deref(), Some("-25:00:00"));
  assert_eq!(row[2].as_deref(), Some("1.5"));
}

#[tokio::test]
async fn integration_mysql_server_errors_carry_the_message() {
  let Some(connection) = test_connection_from_env().await else {
    return;
  };
  let result = connection
    .sql()
    .unwrap()
    .run_query("SELECT * FROM nope_table")
    .await;
  let Err(Error::Database { message }) = result else {
    panic!("expected a database error");
  };
  assert!(message.contains("doesn't exist"), "{message}");
}

#[tokio::test]
async fn integration_mysql_connection_cancel_kills_pooled_query() {
  let Some(connection) = test_connection_from_env().await else {
    return;
  };
  let connection: std::sync::Arc<Box<dyn Connection>> = connection.into();
  let runner = connection.clone();
  let query =
    tokio::spawn(async move { runner.sql().unwrap().run_query("SELECT SLEEP(30)").await });
  tokio::time::sleep(std::time::Duration::from_millis(300)).await;

  let started = Instant::now();
  connection.sql().unwrap().cancel().await.unwrap();
  let outcome = query.await.unwrap();
  assert!(
    started.elapsed() < std::time::Duration::from_secs(5),
    "cancel took {:?}",
    started.elapsed()
  );
  assert_interrupted_sleep(outcome);
  // The pool hands out a healthy connection afterwards.
  connection.health().await.unwrap();
}

#[tokio::test]
async fn integration_mysql_ssl_mode_controls_encryption() {
  let Some(require) = profile_with_ssl(SslMode::Require) else {
    return;
  };
  let cipher = |result: QueryResult| result.statements[0].rows[0][1].clone();

  // mysql 8 auto-generates certs: require must yield an encrypted session.
  let encrypted = MysqlConnector
    .connect(
      &require,
      Credentials::fixed(Some("soquel".to_string())),
      None,
    )
    .await
    .unwrap();
  let status = encrypted
    .sql()
    .unwrap()
    .run_query("SHOW STATUS LIKE 'Ssl_cipher'")
    .await
    .unwrap();
  assert!(
    !cipher(status).unwrap_or_default().is_empty(),
    "require must encrypt"
  );

  let disable = profile_with_ssl(SslMode::Disable).unwrap();
  let plaintext = MysqlConnector
    .connect(
      &disable,
      Credentials::fixed(Some("soquel".to_string())),
      None,
    )
    .await
    .unwrap();
  let status = plaintext
    .sql()
    .unwrap()
    .run_query("SHOW STATUS LIKE 'Ssl_cipher'")
    .await
    .unwrap();
  assert_eq!(cipher(status).unwrap_or_default(), "");
}

#[tokio::test]
async fn integration_mysql_server_version_is_captured() {
  let Some(connection) = test_connection_from_env().await else {
    return;
  };
  let version = connection.server_version().expect("captured at connect");
  assert!(
    version.starts_with(|c: char| c.is_ascii_digit()),
    "{version}"
  );
}

#[tokio::test]
async fn integration_mysql_session_pins_state_and_cancel_kills_query() {
  let Some(connection) = test_connection_from_env().await else {
    return;
  };
  let session: std::sync::Arc<dyn SqlSession> = connection
    .sql()
    .unwrap()
    .open_session()
    .await
    .unwrap()
    .into();

  // Session variables stick across statements on the pinned conn.
  session.run_query("SET @probe = 41").await.unwrap();
  let result = session
    .run_query("SELECT @probe + 1 AS answer")
    .await
    .unwrap();
  assert_eq!(result.statements[0].rows[0][0].as_deref(), Some("42"));

  // Cancel from the outside kills only this session's running query.
  let runner = session.clone();
  let query = tokio::spawn(async move { runner.run_query("SELECT SLEEP(30)").await });
  tokio::time::sleep(std::time::Duration::from_millis(300)).await;
  let started = Instant::now();
  session.cancel().await.unwrap();
  let outcome = query.await.unwrap();
  assert!(
    started.elapsed() < std::time::Duration::from_secs(5),
    "cancel took {:?}",
    started.elapsed()
  );
  assert_interrupted_sleep(outcome);

  // The session survives the cancel.
  session.run_query("SELECT 1").await.unwrap();
  session.close().await.unwrap();
}

fn rows_request(
  table: &str,
  limit: Option<u32>,
  filters: Vec<crate::connectors::ColumnFilter>,
  sort: Option<crate::connectors::SortSpec>,
) -> TableRowsRequest {
  TableRowsRequest {
    schema: "soquel_test".to_string(),
    table: table.to_string(),
    limit,
    offset: 0,
    sort,
    filters,
    // pg-only hints, ignored by this connector.
    include_ctid: true,
    include_xmin: true,
  }
}

fn text_filter(
  column: &str,
  op: crate::connectors::FilterOp,
  value: &str,
) -> crate::connectors::ColumnFilter {
  crate::connectors::ColumnFilter {
    column: column.to_string(),
    op,
    value: Some(value.to_string()),
  }
}

#[tokio::test]
async fn integration_mysql_table_rows_sorts_filters_paginates() {
  use crate::connectors::{FilterOp, SortDirection, SortSpec};

  let Some(connection) = test_connection_from_env().await else {
    return;
  };
  let sql = connection.sql().unwrap();
  // events: immutable across the suite, unlike customers (the apply test
  // inserts rows there concurrently).
  let result = sql
    .table_rows(&rows_request(
      "events",
      Some(2),
      vec![text_filter("kind", FilterOp::Eq, "view")],
      Some(SortSpec {
        column: "n".to_string(),
        direction: SortDirection::Desc,
      }),
    ))
    .await
    .unwrap();
  let statement = &result.statements[0];
  // Odd n seeds as 'view': 999 then 997 in desc order.
  assert_eq!(statement.rows.len(), 2);
  assert_eq!(statement.rows[0][2].as_deref(), Some("999"));
  assert_eq!(statement.rows[1][2].as_deref(), Some("997"));
  let kind = statement.columns.iter().find(|c| c.name == "kind").unwrap();
  assert_eq!(kind.kind, ColumnKind::Text);

  // Numeric coercion of text params: no explicit casts here, the server
  // must compare `n > '990'` numerically, not lexicographically.
  let numeric = sql
    .table_rows(&rows_request(
      "events",
      Some(100),
      vec![text_filter("n", FilterOp::Gt, "990")],
      None,
    ))
    .await
    .unwrap();
  assert_eq!(numeric.statements[0].rows.len(), 10);

  let unknown = sql
    .table_rows(&rows_request(
      "customers",
      Some(2),
      vec![text_filter("nope", FilterOp::Eq, "x")],
      None,
    ))
    .await;
  assert!(matches!(unknown, Err(Error::Unsupported { .. })));
}

#[tokio::test]
async fn integration_mysql_stream_abort_leaves_connection_usable() {
  let Some(connection) = test_connection_from_env().await else {
    return;
  };
  let sql = connection.sql().unwrap();

  let delivered: std::sync::Arc<std::sync::Mutex<usize>> = std::sync::Arc::default();
  let sink = delivered.clone();
  let summary = sql
    .stream_rows(
      &rows_request("events", None, vec![], None),
      Box::new(move |chunk| {
        *sink.lock().unwrap() += chunk.rows.len();
        // Receiver gone after the first chunk.
        false
      }),
    )
    .await
    .unwrap();
  assert!(summary.rows < 1000.0, "abort must stop the stream early");

  // The pooled conn went back with a half-read result set: the driver must
  // drain it before the next query.
  let after = sql.run_query("SELECT COUNT(*) FROM events").await.unwrap();
  assert_eq!(after.statements[0].rows[0][0].as_deref(), Some("1000"));
}

#[tokio::test]
async fn integration_mysql_default_only_insert() {
  let Some(connection) = test_connection_from_env().await else {
    return;
  };
  let applied = connection
    .sql()
    .unwrap()
    .apply_changes(&TableChanges {
      schema: "soquel_test".to_string(),
      table: "defaults_probe".to_string(),
      updates: vec![],
      deletes: vec![],
      inserts: vec![crate::connectors::RowInsert { values: vec![] }],
    })
    .await
    .unwrap();
  assert_eq!(applied.inserted, 1);
  let rows = connection
    .sql()
    .unwrap()
    .run_query("SELECT label FROM defaults_probe ORDER BY id DESC LIMIT 1")
    .await
    .unwrap();
  assert_eq!(rows.statements[0].rows[0][0].as_deref(), Some("fresh"));
}

#[tokio::test]
async fn integration_mysql_stream_rows_chunks_and_streams_unlimited() {
  use std::sync::{Arc as StdArc, Mutex as StdMutex};

  let Some(connection) = test_connection_from_env().await else {
    return;
  };
  let sql = connection.sql().unwrap();

  let chunks: StdArc<StdMutex<Vec<RowsChunk>>> = StdArc::default();
  let sink = chunks.clone();
  let summary = sql
    .stream_rows(
      &rows_request("events", Some(500), vec![], None),
      Box::new(move |chunk| {
        sink.lock().unwrap().push(chunk);
        true
      }),
    )
    .await
    .unwrap();
  assert_eq!(summary.rows, 500.0);
  {
    let chunks = chunks.lock().unwrap();
    assert_eq!(chunks.len(), 3, "500 rows in 200-row chunks");
    assert!(chunks[0].columns.is_some());
    assert!(chunks[1..].iter().all(|c| c.columns.is_none()));
  }

  let total: StdArc<StdMutex<usize>> = StdArc::default();
  let sink = total.clone();
  let summary = sql
    .stream_rows(
      &rows_request("events", None, vec![], None),
      Box::new(move |chunk| {
        *sink.lock().unwrap() += chunk.rows.len();
        true
      }),
    )
    .await
    .unwrap();
  assert_eq!(summary.rows, 1000.0, "no limit streams the whole table");
  assert_eq!(*total.lock().unwrap(), 1000);
}

#[tokio::test]
async fn integration_mysql_apply_changes_roundtrip_and_rollback() {
  let Some(connection) = test_connection_from_env().await else {
    return;
  };
  let sql = connection.sql().unwrap();
  let changes = |updates, deletes, inserts| TableChanges {
    schema: "soquel_test".to_string(),
    table: "customers".to_string(),
    updates,
    deletes,
    inserts,
  };
  let cell = |column: &str, value: Option<&str>| crate::connectors::CellValue {
    column: column.to_string(),
    value: value.map(str::to_string),
  };

  let applied = sql
    .apply_changes(&changes(
      vec![],
      vec![],
      vec![crate::connectors::RowInsert {
        values: vec![
          cell("name", Some("Temp Row")),
          cell("email", Some("temp@example.com")),
        ],
      }],
    ))
    .await
    .unwrap();
  assert_eq!(applied.inserted, 1);

  let updated = sql
    .apply_changes(&changes(
      vec![crate::connectors::RowUpdate {
        key: vec![cell("email", Some("temp@example.com"))],
        set: vec![cell("name", Some("Temp Renamed")), cell("meta", None)],
      }],
      vec![],
      vec![],
    ))
    .await
    .unwrap();
  assert_eq!(updated.updated, 1);

  // Second update matches nothing: the whole batch must roll back.
  let result = sql
    .apply_changes(&changes(
      vec![
        crate::connectors::RowUpdate {
          key: vec![cell("email", Some("temp@example.com"))],
          set: vec![cell("name", Some("Should Not Stick"))],
        },
        crate::connectors::RowUpdate {
          key: vec![cell("email", Some("ghost@example.com"))],
          set: vec![cell("name", Some("x"))],
        },
      ],
      vec![],
      vec![],
    ))
    .await;
  let Err(Error::Database { message }) = result else {
    panic!("expected the batch to fail");
  };
  assert!(message.contains("changed or deleted"), "{message}");
  let check = sql
    .table_rows(&rows_request(
      "customers",
      Some(1),
      vec![text_filter(
        "email",
        crate::connectors::FilterOp::Eq,
        "temp@example.com",
      )],
      None,
    ))
    .await
    .unwrap();
  assert_eq!(
    check.statements[0].rows[0][1].as_deref(),
    Some("Temp Renamed")
  );

  // A key matching several rows trips the exactly-one guard too.
  let multi = sql
    .apply_changes(&TableChanges {
      schema: "soquel_test".to_string(),
      table: "events".to_string(),
      updates: vec![crate::connectors::RowUpdate {
        key: vec![cell("kind", Some("click"))],
        set: vec![cell("n", Some("0"))],
      }],
      deletes: vec![],
      inserts: vec![],
    })
    .await;
  let Err(Error::Database { message }) = multi else {
    panic!("expected the multi-row update to fail");
  };
  assert!(message.contains("instead of exactly 1"), "{message}");

  let deleted = sql
    .apply_changes(&changes(
      vec![],
      vec![crate::connectors::RowDelete {
        key: vec![cell("email", Some("temp@example.com"))],
      }],
      vec![],
    ))
    .await
    .unwrap();
  assert_eq!(deleted.deleted, 1);
}

#[tokio::test]
async fn integration_mysql_schema_snapshot() {
  let Some(connection) = test_connection_from_env().await else {
    return;
  };
  let snapshot = connection
    .introspect()
    .unwrap()
    .schema_snapshot()
    .await
    .unwrap();
  let schema = snapshot
    .schemas
    .iter()
    .find(|s| s.name == "soquel_test")
    .expect("seeded database present");

  let customers = schema
    .tables
    .iter()
    .find(|t| t.name == "customers")
    .unwrap();
  assert_eq!(customers.kind, TableKind::Table);
  assert_eq!(customers.primary_key, vec!["id"]);
  let id = customers.columns.iter().find(|c| c.name == "id").unwrap();
  // mariadb keeps the display width mysql 8 dropped.
  assert!(id.data_type.starts_with("int"), "{}", id.data_type);
  assert_eq!(id.default.as_deref(), Some("auto_increment"));
  let email = customers
    .columns
    .iter()
    .find(|c| c.name == "email")
    .unwrap();
  assert!(email.nullable);
  assert_eq!(email.data_type, "varchar(255)");
  assert!(
    customers
      .indexes
      .iter()
      .any(|i| i.unique && i.definition.contains("email")),
    "{:?}",
    customers.indexes
  );

  let orders = schema.tables.iter().find(|t| t.name == "orders").unwrap();
  let fk = &orders.foreign_keys[0];
  assert_eq!(fk.columns, vec!["customer_id"]);
  assert_eq!(fk.referenced_schema, "soquel_test");
  assert_eq!(fk.referenced_table, "customers");
  assert_eq!(fk.referenced_columns, vec!["id"]);
  // The FK's auto-created index shows up as non-unique.
  assert!(
    orders
      .indexes
      .iter()
      .any(|i| !i.unique && i.definition.contains("customer_id")),
    "{:?}",
    orders.indexes
  );

  // Composite FK: both column pairs, in key order.
  let subscriptions = schema
    .tables
    .iter()
    .find(|t| t.name == "subscriptions")
    .unwrap();
  let composite = &subscriptions.foreign_keys[0];
  assert_eq!(composite.columns, vec!["org_id", "plan_code"]);
  assert_eq!(composite.referenced_table, "plans");
  assert_eq!(composite.referenced_columns, vec!["org_id", "code"]);

  // Databases group as schemas, alphabetical input order preserved.
  let other = snapshot
    .schemas
    .iter()
    .find(|s| s.name == "soquel_other")
    .expect("granted second database visible");
  assert!(other.tables.iter().any(|t| t.name == "notes"));

  let view = schema
    .tables
    .iter()
    .find(|t| t.name == "recent_orders")
    .unwrap();
  assert_eq!(view.kind, TableKind::View);
  assert!(!view.columns.is_empty());
}

#[tokio::test]
async fn integration_mysql_table_ddl_via_show_create() {
  let Some(connection) = test_connection_from_env().await else {
    return;
  };
  let introspect = connection.introspect().unwrap();

  let ddl = introspect.table_ddl("soquel_test", "orders").await.unwrap();
  assert!(ddl.contains("CREATE TABLE `orders`"), "{ddl}");
  assert!(ddl.contains("PRIMARY KEY (`id`)"), "{ddl}");
  assert!(
    ddl.contains("FOREIGN KEY (`customer_id`) REFERENCES `customers` (`id`)"),
    "{ddl}"
  );

  let view = introspect
    .table_ddl("soquel_test", "recent_orders")
    .await
    .unwrap();
  assert!(view.contains("VIEW `recent_orders`"), "{view}");

  assert!(matches!(
    introspect.table_ddl("soquel_test", "nope").await,
    Err(Error::NotFound { .. })
  ));
}

#[test]
fn statement_head_skips_noise() {
  assert_eq!(statement_head("SELECT 1"), "SELECT");
  assert_eq!(
    statement_head("  -- lead\n/* block */ (select 1)"),
    "select"
  );
  assert_eq!(statement_head("# hash comment\nSHOW TABLES"), "SHOW");
  assert_eq!(statement_head("/* unterminated"), "");
  assert_eq!(statement_head(""), "");
}

#[test]
fn read_statement_guard_allows_reads_only() {
  for sql in [
    "SELECT 1",
    "with recent as (select 1) select * from recent",
    "EXPLAIN SELECT 1",
    "SHOW TABLES",
    "DESCRIBE customers",
  ] {
    assert!(read_statement_guard(sql).is_ok(), "{sql}");
  }
  for sql in [
    "INSERT INTO t VALUES (1)",
    "UPDATE t SET a = 1",
    "CREATE TABLE t (id int)",
    "DROP TABLE t",
    "SET SESSION sql_mode = ''",
    "CALL cleanup()",
    "/* sneaky */ TRUNCATE t",
    "",
  ] {
    let err = read_statement_guard(sql).unwrap_err();
    let Error::Unsupported { message } = err else {
      panic!("expected unsupported for {sql}: {err:?}");
    };
    assert!(message.contains("only read statements"), "{message}");
  }
}

#[tokio::test]
async fn integration_mysql_read_only_query() {
  let Some(profile) = profile_from_env() else {
    return;
  };
  let connection = MysqlConnector
    .connect(
      &profile,
      Credentials::fixed(Some("soquel".to_string())),
      None,
    )
    .await
    .unwrap();
  let sql = connection.sql().unwrap();

  let result = sql.run_read_only_query("SELECT 1 AS one").await.unwrap();
  assert_eq!(result.statements[0].rows[0][0].as_deref(), Some("1"));

  let err = sql
    .run_read_only_query("INSERT INTO customers (name) VALUES ('x')")
    .await
    .unwrap_err();
  let Error::Unsupported { message } = err else {
    panic!("expected unsupported: {err:?}");
  };
  assert!(message.contains("only read statements"), "{message}");

  // A read head that locks still dies on the READ ONLY transaction
  // (WITH...UPDATE would too, but MariaDB does not parse CTE writes).
  let err = sql
    .run_read_only_query("SELECT id FROM customers LIMIT 1 FOR UPDATE")
    .await
    .unwrap_err();
  let Error::Database { message } = err else {
    panic!("expected a database error: {err:?}");
  };
  assert!(message.to_lowercase().contains("read only"), "{message}");

  // The agent cap is armed for the query and reset before the connection goes
  // back to the pool. MariaDB caps in seconds under a different name.
  let mariadb = is_mariadb(connection.as_ref());
  let knob = if mariadb {
    "@@max_statement_time"
  } else {
    "@@MAX_EXECUTION_TIME"
  };
  let expected = if mariadb { "30" } else { "30000" };
  let armed = sql
    .run_read_only_query(&format!("SELECT {knob} AS cap"))
    .await
    .unwrap();
  let reported = armed.statements[0].rows[0][0].clone().unwrap();
  assert!(reported.starts_with(expected), "{reported}");
  let after = sql
    .run_query(&format!("SELECT {knob} AS cap"))
    .await
    .unwrap();
  assert!(
    !after.statements[0].rows[0][0]
      .clone()
      .unwrap()
      .starts_with(expected),
    "the cap leaked to the pool"
  );

  // The pool stays writable for the app itself.
  sql
    .run_query("UPDATE customers SET name = name LIMIT 1")
    .await
    .unwrap();
}
