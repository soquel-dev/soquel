use super::browse::build_where;
use super::*;
use crate::connectors::{CellValue, ColumnFilter, FilterOp, SortDirection};

// Convention: integration_* tests run via `pnpm test:integration` (needs `pnpm db:test`),
// each gated by its connector's env var, and skip silently otherwise.
#[test]
fn quote_ident_escapes_quotes() {
  assert_eq!(quote_ident("customers"), "\"customers\"");
  assert_eq!(quote_ident("MiXeD"), "\"MiXeD\"");
  assert_eq!(quote_ident("evil\"; DROP--"), "\"evil\"\"; DROP--\"");
}

fn text_columns(names: &[&str]) -> Vec<(String, PgType)> {
  names
    .iter()
    .map(|name| (name.to_string(), PgType::TEXT))
    .collect()
}

fn filter(column: &str, op: FilterOp, value: Option<&str>) -> ColumnFilter {
  ColumnFilter {
    column: column.to_string(),
    op,
    value: value.map(str::to_string),
  }
}

#[test]
fn build_where_covers_every_operator() {
  let columns = text_columns(&["name", "amount"]);
  let cases: [(FilterOp, Option<&str>, &str, Option<&str>); 10] = [
    (
      FilterOp::Eq,
      Some("x"),
      r#" WHERE "name" = $1::text"#,
      Some("x"),
    ),
    (
      FilterOp::Neq,
      Some("x"),
      r#" WHERE "name" <> $1::text"#,
      Some("x"),
    ),
    (
      FilterOp::Lt,
      Some("5"),
      r#" WHERE "name" < $1::text"#,
      Some("5"),
    ),
    (
      FilterOp::Lte,
      Some("5"),
      r#" WHERE "name" <= $1::text"#,
      Some("5"),
    ),
    (
      FilterOp::Gt,
      Some("5"),
      r#" WHERE "name" > $1::text"#,
      Some("5"),
    ),
    (
      FilterOp::Gte,
      Some("5"),
      r#" WHERE "name" >= $1::text"#,
      Some("5"),
    ),
    (
      FilterOp::Contains,
      Some("ada"),
      r#" WHERE "name"::text ILIKE $1"#,
      Some("%ada%"),
    ),
    (
      FilterOp::StartsWith,
      Some("ada"),
      r#" WHERE "name"::text ILIKE $1"#,
      Some("ada%"),
    ),
    (FilterOp::IsNull, None, r#" WHERE "name" IS NULL"#, None),
    (
      FilterOp::IsNotNull,
      None,
      r#" WHERE "name" IS NOT NULL"#,
      None,
    ),
  ];
  for (op, value, clause, param) in cases {
    let (built, params) = build_where(&columns, &[filter("name", op, value)]).unwrap();
    assert_eq!(built, clause, "{op:?}");
    assert_eq!(
      params,
      param.map(str::to_string).into_iter().collect::<Vec<_>>()
    );
  }
}

#[test]
fn build_where_numbers_params_and_ands_clauses() {
  let columns = text_columns(&["name", "email", "amount"]);
  let (clause, params) = build_where(
    &columns,
    &[
      filter("name", FilterOp::Contains, Some("a")),
      filter("email", FilterOp::IsNotNull, None),
      filter("amount", FilterOp::Gt, Some("10")),
    ],
  )
  .unwrap();
  assert_eq!(
    clause,
    r#" WHERE "name"::text ILIKE $1 AND "email" IS NOT NULL AND "amount" > $2::text"#
  );
  assert_eq!(params, vec!["%a%".to_string(), "10".to_string()]);
}

#[test]
fn build_where_rejects_unknown_columns_and_missing_values() {
  let columns = text_columns(&["name"]);
  assert!(matches!(
    build_where(&columns, &[filter("nope", FilterOp::Eq, Some("x"))]),
    Err(Error::Unsupported { .. })
  ));
  assert!(matches!(
    build_where(&columns, &[filter("name", FilterOp::Eq, None)]),
    Err(Error::Unsupported { .. })
  ));
}

#[test]
fn build_where_quotes_hostile_idents_and_escapes_like() {
  let columns = vec![("evil\"; DROP--".to_string(), PgType::TEXT)];
  let (clause, params) = build_where(
    &columns,
    &[filter("evil\"; DROP--", FilterOp::Contains, Some("50%_\\"))],
  )
  .unwrap();
  assert_eq!(clause, r#" WHERE "evil""; DROP--"::text ILIKE $1"#);
  assert_eq!(params, vec!["%50\\%\\_\\\\%".to_string()]);
}

fn cell(column: &str, value: Option<&str>) -> CellValue {
  CellValue {
    column: column.to_string(),
    value: value.map(str::to_string),
  }
}

fn no_changes() -> TableChanges {
  TableChanges {
    schema: "app".to_string(),
    table: "customers".to_string(),
    updates: vec![],
    inserts: vec![],
    deletes: vec![],
  }
}

#[test]
fn change_statements_cover_update_delete_insert() {
  let columns = vec![
    ("id".to_string(), PgType::INT4),
    ("name".to_string(), PgType::TEXT),
    ("meta".to_string(), PgType::JSONB),
  ];
  let changes = TableChanges {
    updates: vec![crate::connectors::RowUpdate {
      key: vec![cell("id", Some("1"))],
      set: vec![cell("name", Some("Ada")), cell("meta", None)],
    }],
    deletes: vec![crate::connectors::RowDelete {
      key: vec![cell("id", Some("2"))],
    }],
    inserts: vec![
      crate::connectors::RowInsert {
        values: vec![cell("name", Some("New"))],
      },
      crate::connectors::RowInsert { values: vec![] },
    ],
    ..no_changes()
  };
  let statements = build_change_statements("app", "customers", &columns, &changes).unwrap();
  assert_eq!(statements.len(), 4);

  assert_eq!(
    statements[0].sql,
    r#"UPDATE "app"."customers" SET "name" = $1::text, "meta" = $2::jsonb WHERE "id" IS NOT DISTINCT FROM $3::int4"#
  );
  assert_eq!(
    statements[0].params,
    vec![Some("Ada".to_string()), None, Some("1".to_string())]
  );
  assert_eq!(statements[0].kind, ChangeKind::Update);

  assert_eq!(
    statements[1].sql,
    r#"DELETE FROM "app"."customers" WHERE "id" IS NOT DISTINCT FROM $1::int4"#
  );
  assert_eq!(statements[1].kind, ChangeKind::Delete);

  assert_eq!(
    statements[2].sql,
    r#"INSERT INTO "app"."customers" ("name") VALUES ($1::text)"#
  );
  assert_eq!(
    statements[3].sql,
    r#"INSERT INTO "app"."customers" DEFAULT VALUES"#
  );
}

#[test]
fn change_statements_reject_unknown_columns_and_empty_shapes() {
  let columns = vec![("id".to_string(), PgType::INT4)];
  let unknown = TableChanges {
    updates: vec![crate::connectors::RowUpdate {
      key: vec![cell("id", Some("1"))],
      set: vec![cell("nope", Some("x"))],
    }],
    ..no_changes()
  };
  assert!(matches!(
    build_change_statements("app", "customers", &columns, &unknown),
    Err(Error::Unsupported { .. })
  ));

  let empty_key = TableChanges {
    deletes: vec![crate::connectors::RowDelete { key: vec![] }],
    ..no_changes()
  };
  assert!(matches!(
    build_change_statements("app", "customers", &columns, &empty_key),
    Err(Error::Unsupported { .. })
  ));
}

#[test]
fn change_statements_allow_ctid_keys_and_quote_hostile_idents() {
  let columns = vec![("evil\"; DROP--".to_string(), PgType::TEXT)];
  let changes = TableChanges {
    updates: vec![crate::connectors::RowUpdate {
      key: vec![cell("ctid", Some("(0,1)"))],
      set: vec![cell("evil\"; DROP--", Some("x"))],
    }],
    ..no_changes()
  };
  let statements = build_change_statements("app", "t", &columns, &changes).unwrap();
  assert_eq!(
    statements[0].sql,
    r#"UPDATE "app"."t" SET "evil""; DROP--" = $1::text WHERE "ctid" IS NOT DISTINCT FROM $2::tid"#
  );
}

#[test]
fn change_statements_key_on_xmin_as_xid() {
  let columns = vec![
    ("id".to_string(), PgType::INT4),
    ("name".to_string(), PgType::TEXT),
  ];
  let changes = TableChanges {
    updates: vec![crate::connectors::RowUpdate {
      key: vec![cell("id", Some("1")), cell("xmin", Some("12345"))],
      set: vec![cell("name", Some("Ada"))],
    }],
    ..no_changes()
  };
  let statements = build_change_statements("app", "customers", &columns, &changes).unwrap();
  assert_eq!(
    statements[0].sql,
    r#"UPDATE "app"."customers" SET "name" = $1::text WHERE "id" IS NOT DISTINCT FROM $2::int4 AND "xmin" IS NOT DISTINCT FROM $3::xid"#
  );
}

#[test]
fn column_kind_maps_common_types() {
  assert_eq!(column_kind(&PgType::INT8), ColumnKind::Number);
  assert_eq!(column_kind(&PgType::NUMERIC), ColumnKind::Number);
  assert_eq!(column_kind(&PgType::JSONB), ColumnKind::Json);
  assert_eq!(column_kind(&PgType::TIMESTAMPTZ), ColumnKind::DateTime);
  assert_eq!(column_kind(&PgType::TEXT_ARRAY), ColumnKind::Array);
  assert_eq!(column_kind(&PgType::POINT), ColumnKind::Other);
  assert_eq!(type_name(&PgType::TEXT_ARRAY), "text[]");
}

#[test]
fn build_config_keeps_logical_host_behind_a_forward() {
  use tokio_postgres::config::Host;

  let params = SqlServerParams {
    host: "db.internal".to_string(),
    port: 5432,
    database: "app".to_string(),
    user: "u".to_string(),
    ssl_mode: SslMode::VerifyFull,
    ssl_root_cert: None,
    tunnel_id: None,
  };

  // Tunneled: TCP goes to the forward, TLS still targets db.internal.
  let config = build_config(&params, Some(LocalForward { port: 6000 }));
  assert_eq!(config.get_hosts(), &[Host::Tcp("db.internal".to_string())]);
  assert_eq!(
    config.get_hostaddrs(),
    &[std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)]
  );
  assert_eq!(config.get_ports(), &[6000]);

  let direct = build_config(&params, None);
  assert!(direct.get_hostaddrs().is_empty());
  assert_eq!(direct.get_ports(), &[5432]);
}

fn connection_from_url(url: &str, ssl_mode: SslMode) -> PostgresConnection {
  let mut config: Config = url.parse().unwrap();
  config.ssl_mode(tls::config_ssl_mode(ssl_mode));
  PostgresConnection::new(config, Credentials::fixed(None), ssl_mode, None).unwrap()
}

pub async fn test_connection_from_env() -> Option<PostgresConnection> {
  let url = std::env::var("SOQUEL_TEST_PG").ok()?;
  Some(connection_from_url(&url, SslMode::Prefer))
}

#[tokio::test]
async fn integration_postgres_query_roundtrip() {
  let Some(pg) = test_connection_from_env().await else {
    return;
  };

  pg.health().await.unwrap();

  let result = pg
    .run_query("SELECT 1 AS one; SELECT 'a' AS a, NULL AS b")
    .await
    .unwrap();
  assert_eq!(result.statements.len(), 2);
  assert_eq!(result.statements[0].columns[0].name, "one");
  assert_eq!(result.statements[0].rows_affected, 1.0);
  assert_eq!(
    result.statements[1].rows[0],
    vec![Some("a".to_string()), None]
  );
  // Multi-statement scripts cannot be prepared: no type metadata.
  assert_eq!(result.statements[0].columns[0].data_type, None);
}

#[tokio::test]
async fn integration_postgres_single_statement_carries_types() {
  let Some(pg) = test_connection_from_env().await else {
    return;
  };
  let result = pg
    .run_query("SELECT 1 AS one, 'a'::text AS a, now() AS ts")
    .await
    .unwrap();
  let columns = &result.statements[0].columns;
  assert_eq!(columns[0].data_type.as_deref(), Some("int4"));
  assert_eq!(columns[0].kind, ColumnKind::Number);
  assert_eq!(columns[1].kind, ColumnKind::Text);
  assert_eq!(columns[2].data_type.as_deref(), Some("timestamptz"));
  assert_eq!(columns[2].kind, ColumnKind::DateTime);
}

#[tokio::test]
async fn integration_postgres_notices_surface_in_results() {
  let Some(pg) = test_connection_from_env().await else {
    return;
  };
  let result = pg
    .run_query("DO $$ BEGIN RAISE NOTICE 'soquel test notice'; END $$")
    .await
    .unwrap();
  assert!(result
    .notices
    .iter()
    .any(|n| n.message == "soquel test notice" && n.severity == "NOTICE"));

  // The buffer is per query: a follow-up query starts clean.
  let clean = pg.run_query("SELECT 1").await.unwrap();
  assert!(clean.notices.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn integration_postgres_pool_unblocks_concurrent_queries() {
  let Some(pg) = test_connection_from_env().await else {
    return;
  };
  let pg = Arc::new(pg);
  let slow = tokio::spawn({
    let pg = pg.clone();
    async move { pg.run_query("SELECT pg_sleep(2)").await }
  });
  tokio::time::sleep(Duration::from_millis(300)).await;

  let start = Instant::now();
  pg.run_query("SELECT 1").await.unwrap();
  assert!(
    start.elapsed() < Duration::from_secs(1),
    "quick query waited on the slow one"
  );
  slow.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn integration_postgres_cancel_kills_running_query() {
  let Some(pg) = test_connection_from_env().await else {
    return;
  };
  let pg = Arc::new(pg);
  let slow = tokio::spawn({
    let pg = pg.clone();
    async move { pg.run_query("SELECT pg_sleep(30)").await }
  });
  tokio::time::sleep(Duration::from_millis(300)).await;

  pg.cancel().await.unwrap();
  let Err(Error::Database { message }) = slow.await.unwrap() else {
    panic!("expected the canceled query to fail");
  };
  assert!(
    message.contains("canceling statement due to user request"),
    "unexpected message: {message}"
  );
}

#[tokio::test(flavor = "multi_thread")]
async fn integration_postgres_session_pins_state() {
  let Some(pg) = test_connection_from_env().await else {
    return;
  };
  let session = pg.open_session().await.unwrap();
  session
    .run_query("SET soquel.flag = 'pinned'")
    .await
    .unwrap();
  let shown = session.run_query("SHOW soquel.flag").await.unwrap();
  assert_eq!(
    shown.statements[0].rows[0][0].as_deref(),
    Some("pinned"),
    "session state must stick across its own runs"
  );
  // Custom GUCs are per-backend: the pool must not know this one.
  assert!(pg.run_query("SHOW soquel.flag").await.is_err());
  session.close().await.unwrap();
  pg.run_query("SELECT 1").await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn integration_postgres_session_cancel_kills_query() {
  let Some(pg) = test_connection_from_env().await else {
    return;
  };
  let session: Arc<dyn SqlSession> = pg.open_session().await.unwrap().into();
  let slow = tokio::spawn({
    let session = session.clone();
    async move { session.run_query("SELECT pg_sleep(30)").await }
  });
  tokio::time::sleep(Duration::from_millis(300)).await;

  session.cancel().await.unwrap();
  let Err(Error::Database { message }) = slow.await.unwrap() else {
    panic!("expected the canceled query to fail");
  };
  assert!(
    message.contains("canceling statement due to user request"),
    "unexpected message: {message}"
  );
  // The session survives a cancel.
  session.run_query("SELECT 1").await.unwrap();
}

#[tokio::test]
async fn integration_postgres_require_tls_fails_on_plaintext_server() {
  let Ok(url) = std::env::var("SOQUEL_TEST_PG") else {
    return;
  };
  // The compose postgres has no TLS: require must fail, prefer falls back.
  let pg = connection_from_url(&url, SslMode::Require);
  let Err(Error::Database { message }) = pg.health().await else {
    panic!("expected require to fail against a plaintext server");
  };
  assert!(!message.is_empty());
}

#[tokio::test]
async fn integration_postgres_tls_require_accepts_self_signed() {
  let Ok(url) = std::env::var("SOQUEL_TEST_PG_TLS") else {
    return;
  };
  // require encrypts without verifying: the throwaway cert must pass.
  let pg = connection_from_url(&url, SslMode::Require);
  let result = pg
    .run_query("SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()")
    .await
    .unwrap();
  assert_eq!(
    result.statements[0].rows[0][0].as_deref(),
    Some("t"),
    "session must actually be TLS"
  );
}

#[tokio::test]
async fn integration_postgres_tls_verify_full_rejects_self_signed() {
  let Ok(url) = std::env::var("SOQUEL_TEST_PG_TLS") else {
    return;
  };
  let pg = connection_from_url(&url, SslMode::VerifyFull);
  let Err(Error::Database { message }) = pg.health().await else {
    panic!("expected verify-full to reject an untrusted certificate");
  };
  assert!(!message.is_empty());
}

// Throwaway CA that signed the server cert (SAN localhost/127.0.0.1).
pub(crate) const TEST_ROOT_CERT: &str =
  concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/test-tls/ca.crt");

#[tokio::test]
async fn integration_postgres_tls_verify_full_passes_with_root_cert() {
  let Ok(url) = std::env::var("SOQUEL_TEST_PG_TLS") else {
    return;
  };
  let mut config: Config = url.parse().unwrap();
  config.ssl_mode(tls::config_ssl_mode(SslMode::VerifyFull));
  // The compose URL points at localhost, which the cert SAN carries.
  let pg = PostgresConnection::new(
    config,
    Credentials::fixed(None),
    SslMode::VerifyFull,
    Some(TEST_ROOT_CERT.to_string()),
  )
  .unwrap();
  pg.health().await.unwrap();
}

#[tokio::test]
async fn integration_postgres_server_version_is_captured() {
  use crate::connectors::Connection;

  let Some(pg) = test_connection_from_env().await else {
    return;
  };
  // Captured lazily with the first pooled connection.
  pg.health().await.unwrap();
  let version = pg.server_version().expect("version after first checkout");
  assert!(
    version.starts_with(|c: char| c.is_ascii_digit()),
    "{version}"
  );
}

#[tokio::test]
async fn integration_postgres_pool_recycles_terminated_connection() {
  let Some(pg) = test_connection_from_env().await else {
    return;
  };
  let result = pg.run_query("SELECT pg_backend_pid()").await.unwrap();
  let pid = result.statements[0].rows[0][0].clone().unwrap();

  // Kill the idle pooled backend from a second, out-of-pool connection.
  let killer = test_connection_from_env().await.unwrap();
  let killed = killer
    .run_query(&format!("SELECT pg_terminate_backend({pid})"))
    .await
    .unwrap();
  assert_eq!(killed.statements[0].rows[0][0].as_deref(), Some("t"));

  // The client task needs a moment to observe the EOF before is_closed
  // trips; recycle must then cull the dead object and hand out a fresh one.
  let mut fresh_pid = None;
  for _ in 0..50 {
    tokio::time::sleep(Duration::from_millis(100)).await;
    if let Ok(result) = pg.run_query("SELECT pg_backend_pid()").await {
      fresh_pid = Some(result.statements[0].rows[0][0].clone().unwrap());
      break;
    }
  }
  let fresh_pid = fresh_pid.expect("pool never recovered from a terminated backend");
  assert_ne!(fresh_pid, pid);
}

#[tokio::test]
async fn integration_postgres_table_rows_sorts_and_paginates() {
  let Some(pg) = test_connection_from_env().await else {
    return;
  };
  let result = pg
    .table_rows(&TableRowsRequest {
      schema: "app".to_string(),
      table: "customers".to_string(),
      limit: Some(2),
      offset: 0,
      sort: Some(crate::connectors::SortSpec {
        column: "name".to_string(),
        direction: SortDirection::Desc,
      }),
      filters: vec![],
      include_ctid: false,
      include_xmin: false,
    })
    .await
    .unwrap();
  let statement = &result.statements[0];
  assert_eq!(statement.rows.len(), 2);
  assert_eq!(statement.rows[0][1], Some("Grace Hopper".to_string()));
  let tags = statement.columns.iter().find(|c| c.name == "tags").unwrap();
  assert_eq!(tags.data_type.as_deref(), Some("text[]"));
  assert_eq!(tags.kind, ColumnKind::Array);

  let next = pg
    .table_rows(&TableRowsRequest {
      schema: "app".to_string(),
      table: "customers".to_string(),
      limit: Some(2),
      offset: 2,
      sort: Some(crate::connectors::SortSpec {
        column: "name".to_string(),
        direction: SortDirection::Desc,
      }),
      filters: vec![],
      include_ctid: false,
      include_xmin: false,
    })
    .await
    .unwrap();
  assert_eq!(next.statements[0].rows.len(), 1);
  assert_eq!(
    next.statements[0].rows[0][1],
    Some("Ada Lovelace".to_string())
  );
}

async fn filtered_rows(
  pg: &PostgresConnection,
  table: &str,
  filters: Vec<ColumnFilter>,
) -> StatementResult {
  pg.table_rows(&TableRowsRequest {
    schema: "app".to_string(),
    table: table.to_string(),
    limit: Some(100),
    offset: 0,
    sort: None,
    filters,
    include_ctid: false,
    include_xmin: false,
  })
  .await
  .unwrap()
  .statements
  .remove(0)
}

#[tokio::test]
async fn integration_postgres_filters_compare_typed_columns() {
  let Some(pg) = test_connection_from_env().await else {
    return;
  };

  // contains on text.
  let by_name = filtered_rows(
    &pg,
    "customers",
    vec![filter("name", FilterOp::Contains, Some("ada"))],
  )
  .await;
  assert_eq!(by_name.rows.len(), 1);
  assert_eq!(by_name.rows[0][1].as_deref(), Some("Ada Lovelace"));

  // gt on numeric and on timestamptz: UNKNOWN params coerce to the column type.
  let expensive = filtered_rows(
    &pg,
    "orders",
    vec![filter("amount", FilterOp::Gt, Some("100"))],
  )
  .await;
  assert_eq!(expensive.rows.len(), 2);
  let recent = filtered_rows(
    &pg,
    "orders",
    vec![filter("placed_at", FilterOp::Gt, Some("2000-01-01"))],
  )
  .await;
  assert_eq!(recent.rows.len(), 3);

  // is-null, and two filters AND-ed.
  let no_email = filtered_rows(
    &pg,
    "customers",
    vec![filter("email", FilterOp::IsNull, None)],
  )
  .await;
  assert_eq!(no_email.rows.len(), 1);
  assert_eq!(no_email.rows[0][1].as_deref(), Some("Grace Hopper"));
  let both = filtered_rows(
    &pg,
    "orders",
    vec![
      filter("amount", FilterOp::Gt, Some("100")),
      filter("note", FilterOp::IsNotNull, None),
    ],
  )
  .await;
  assert_eq!(both.rows.len(), 2);
}

#[tokio::test]
async fn integration_postgres_filters_keep_server_text_values() {
  let Some(pg) = test_connection_from_env().await else {
    return;
  };
  let rows = filtered_rows(
    &pg,
    "customers",
    vec![filter("name", FilterOp::Eq, Some("Ada Lovelace"))],
  )
  .await;
  let tags = rows.columns.iter().position(|c| c.name == "tags").unwrap();
  let meta = rows.columns.iter().position(|c| c.name == "meta").unwrap();
  assert_eq!(rows.rows[0][tags].as_deref(), Some("{vip,eu}"));
  assert_eq!(
    rows.rows[0][meta].as_deref(),
    Some(r#"{"plan": "pro", "seats": 3}"#)
  );
  assert_eq!(rows.columns[tags].kind, ColumnKind::Array);

  let receipts = filtered_rows(
    &pg,
    "orders",
    vec![filter("receipt", FilterOp::IsNotNull, None)],
  )
  .await;
  let receipt = receipts
    .columns
    .iter()
    .position(|c| c.name == "receipt")
    .unwrap();
  assert_eq!(receipts.rows[0][receipt].as_deref(), Some("\\xdeadbeef"));
}

fn collecting_chunks() -> (Arc<Mutex<Vec<RowsChunk>>>, crate::connectors::ChunkSink) {
  let chunks: Arc<Mutex<Vec<RowsChunk>>> = Arc::default();
  let sink = chunks.clone();
  (
    chunks,
    Box::new(move |chunk| {
      sink.lock().unwrap().push(chunk);
      true
    }),
  )
}

#[tokio::test]
async fn integration_postgres_stream_rows_chunks_in_order() {
  let Some(pg) = test_connection_from_env().await else {
    return;
  };
  let (chunks, on_chunk) = collecting_chunks();
  let summary = pg
    .stream_rows(
      &TableRowsRequest {
        schema: "app".to_string(),
        table: "events".to_string(),
        limit: Some(1000),
        offset: 0,
        sort: Some(crate::connectors::SortSpec {
          column: "id".to_string(),
          direction: SortDirection::Asc,
        }),
        filters: vec![],
        include_ctid: false,
        include_xmin: false,
      },
      on_chunk,
    )
    .await
    .unwrap();
  assert_eq!(summary.rows, 1000.0);

  let chunks = chunks.lock().unwrap();
  assert_eq!(chunks.len(), 5, "1000 rows in 200-row chunks");
  assert!(chunks[0].columns.is_some(), "first chunk carries columns");
  assert!(chunks[1..].iter().all(|c| c.columns.is_none()));
  assert_eq!(chunks[0].rows[0][0].as_deref(), Some("1"));
  let last = chunks.last().unwrap();
  assert_eq!(last.rows.last().unwrap()[0].as_deref(), Some("1000"));
}

#[tokio::test]
async fn integration_postgres_stream_rows_applies_filters() {
  let Some(pg) = test_connection_from_env().await else {
    return;
  };
  let (chunks, on_chunk) = collecting_chunks();
  let summary = pg
    .stream_rows(
      &TableRowsRequest {
        schema: "app".to_string(),
        table: "events".to_string(),
        limit: Some(5000),
        offset: 0,
        sort: None,
        filters: vec![filter("kind", FilterOp::Eq, Some("purchase"))],
        include_ctid: false,
        include_xmin: false,
      },
      on_chunk,
    )
    .await
    .unwrap();
  // n % 3 == 2 over 1..=10000.
  assert_eq!(summary.rows, 3333.0);
  let total: usize = chunks.lock().unwrap().iter().map(|c| c.rows.len()).sum();
  assert_eq!(total, 3333);
}

// Export path: no limit streams the full table, past MAX_FETCH_ROWS.
#[tokio::test]
async fn integration_postgres_export_writes_file_and_reports_progress() {
  use crate::export::{run_export, ExportFormat};

  let Some(pg) = test_connection_from_env().await else {
    return;
  };
  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("events.csv");
  let progress: Arc<Mutex<Vec<u64>>> = Arc::default();
  let sink = progress.clone();
  let summary = run_export(
    &pg,
    &TableRowsRequest {
      schema: "app".to_string(),
      table: "events".to_string(),
      limit: None,
      offset: 0,
      sort: None,
      filters: vec![],
      include_ctid: false,
      include_xmin: false,
    },
    ExportFormat::Csv,
    crate::profiles::ConnectorKind::Postgres,
    path.to_str().unwrap(),
    move |rows| sink.lock().unwrap().push(rows),
  )
  .await
  .unwrap();
  assert_eq!(summary.rows, 10000.0);

  let csv = std::fs::read_to_string(&path).unwrap();
  assert_eq!(csv.lines().count(), 10001, "header + every row");
  // 10k rows in 200-row chunks = 50 pushes: one report every 25.
  assert_eq!(*progress.lock().unwrap(), vec![5000, 10000]);
}

#[tokio::test]
async fn integration_postgres_export_failure_removes_the_partial_file() {
  use crate::export::{run_export, ExportFormat};

  let Some(pg) = test_connection_from_env().await else {
    return;
  };
  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("ghost.csv");
  let result = run_export(
    &pg,
    &TableRowsRequest {
      schema: "app".to_string(),
      table: "nope".to_string(),
      limit: None,
      offset: 0,
      sort: None,
      filters: vec![],
      include_ctid: false,
      include_xmin: false,
    },
    ExportFormat::Csv,
    crate::profiles::ConnectorKind::Postgres,
    path.to_str().unwrap(),
    |_| {},
  )
  .await;
  assert!(result.is_err());
  assert!(!path.exists(), "a failed export must not leave a file");
}

#[tokio::test]
async fn integration_postgres_stream_abort_leaves_connection_usable() {
  let Some(pg) = test_connection_from_env().await else {
    return;
  };
  let delivered: Arc<Mutex<usize>> = Arc::default();
  let sink = delivered.clone();
  let summary = pg
    .stream_rows(
      &TableRowsRequest {
        schema: "app".to_string(),
        table: "events".to_string(),
        limit: Some(5000),
        offset: 0,
        sort: None,
        filters: vec![],
        include_ctid: false,
        include_xmin: false,
      },
      Box::new(move |_chunk| {
        let mut count = sink.lock().unwrap();
        *count += 1;
        // Pretend the receiver disappeared after the first chunk.
        *count < 1
      }),
    )
    .await
    .unwrap();
  assert!(summary.rows < 5000.0, "the stream must stop early");
  assert_eq!(*delivered.lock().unwrap(), 1);
  // The pooled client survives an aborted stream.
  pg.run_query("SELECT 1").await.unwrap();
}

#[tokio::test]
async fn integration_postgres_apply_changes_roundtrip_in_one_transaction() {
  let Some(pg) = test_connection_from_env().await else {
    return;
  };
  // Work on rows this test owns so parallel tests stay unaffected.
  let applied = pg
    .apply_changes(&TableChanges {
      inserts: vec![crate::connectors::RowInsert {
        values: vec![
          cell("name", Some("Temp Row")),
          cell("email", Some("temp@example.com")),
        ],
      }],
      ..no_changes()
    })
    .await
    .unwrap();
  assert_eq!(applied.inserted, 1);

  let update_null = pg
    .apply_changes(&TableChanges {
      updates: vec![crate::connectors::RowUpdate {
        key: vec![cell("email", Some("temp@example.com"))],
        set: vec![cell("name", Some("Temp Renamed")), cell("meta", None)],
      }],
      ..no_changes()
    })
    .await
    .unwrap();
  assert_eq!(update_null.updated, 1);

  let rows = filtered_rows(
    &pg,
    "customers",
    vec![filter("email", FilterOp::Eq, Some("temp@example.com"))],
  )
  .await;
  assert_eq!(rows.rows[0][1].as_deref(), Some("Temp Renamed"));

  let deleted = pg
    .apply_changes(&TableChanges {
      deletes: vec![crate::connectors::RowDelete {
        key: vec![cell("email", Some("temp@example.com"))],
      }],
      ..no_changes()
    })
    .await
    .unwrap();
  assert_eq!(deleted.deleted, 1);
}

#[tokio::test]
async fn integration_postgres_apply_changes_rolls_back_entirely() {
  let Some(pg) = test_connection_from_env().await else {
    return;
  };
  // First update is valid, second matches nothing: NOTHING must stick.
  let result = pg
    .apply_changes(&TableChanges {
      updates: vec![
        crate::connectors::RowUpdate {
          key: vec![cell("id", Some("1"))],
          set: vec![cell("name", Some("Should Not Stick"))],
        },
        crate::connectors::RowUpdate {
          key: vec![cell("id", Some("999999"))],
          set: vec![cell("name", Some("x"))],
        },
      ],
      ..no_changes()
    })
    .await;
  let Err(Error::Database { message }) = result else {
    panic!("expected the batch to fail");
  };
  assert!(message.contains("matched 0 rows"), "{message}");

  let rows = filtered_rows(
    &pg,
    "customers",
    vec![filter("id", FilterOp::Eq, Some("1"))],
  )
  .await;
  assert_eq!(rows.rows[0][1].as_deref(), Some("Ada Lovelace"));
  // The pooled client came back clean (no open transaction).
  pg.run_query("SELECT 1").await.unwrap();
}

#[tokio::test]
async fn integration_postgres_xmin_guard_detects_concurrent_writes() {
  let Some(pg) = test_connection_from_env().await else {
    return;
  };
  pg.apply_changes(&TableChanges {
    inserts: vec![crate::connectors::RowInsert {
      values: vec![
        cell("name", Some("Xmin Guard")),
        cell("email", Some("xmin@example.com")),
      ],
    }],
    ..no_changes()
  })
  .await
  .unwrap();

  let fetched = pg
    .table_rows(&TableRowsRequest {
      schema: "app".to_string(),
      table: "customers".to_string(),
      limit: Some(1),
      offset: 0,
      sort: None,
      filters: vec![filter("email", FilterOp::Eq, Some("xmin@example.com"))],
      include_ctid: false,
      include_xmin: true,
    })
    .await
    .unwrap();
  let statement = &fetched.statements[0];
  assert_eq!(statement.columns[0].name, "xmin");
  let stale_xmin = statement.rows[0][0].clone();

  // A concurrent write bumps xmin: the stale guard must match nothing.
  pg.run_query(
    "UPDATE app.customers SET name = 'Moved Underneath' WHERE email = 'xmin@example.com'",
  )
  .await
  .unwrap();
  let result = pg
    .apply_changes(&TableChanges {
      updates: vec![crate::connectors::RowUpdate {
        key: vec![
          cell("email", Some("xmin@example.com")),
          crate::connectors::CellValue {
            column: "xmin".to_string(),
            value: stale_xmin,
          },
        ],
        set: vec![cell("name", Some("Should Conflict"))],
      }],
      ..no_changes()
    })
    .await;
  let Err(Error::Database { message }) = result else {
    panic!("expected the stale xmin to conflict");
  };
  assert!(message.contains("changed or deleted"), "{message}");

  // With the fresh xmin the same update goes through.
  let fresh = pg
    .table_rows(&TableRowsRequest {
      schema: "app".to_string(),
      table: "customers".to_string(),
      limit: Some(1),
      offset: 0,
      sort: None,
      filters: vec![filter("email", FilterOp::Eq, Some("xmin@example.com"))],
      include_ctid: false,
      include_xmin: true,
    })
    .await
    .unwrap();
  let fresh_xmin = fresh.statements[0].rows[0][0].clone();
  let applied = pg
    .apply_changes(&TableChanges {
      updates: vec![crate::connectors::RowUpdate {
        key: vec![
          cell("email", Some("xmin@example.com")),
          crate::connectors::CellValue {
            column: "xmin".to_string(),
            value: fresh_xmin,
          },
        ],
        set: vec![cell("name", Some("Guard Passed"))],
      }],
      ..no_changes()
    })
    .await
    .unwrap();
  assert_eq!(applied.updated, 1);

  pg.apply_changes(&TableChanges {
    deletes: vec![crate::connectors::RowDelete {
      key: vec![cell("email", Some("xmin@example.com"))],
    }],
    ..no_changes()
  })
  .await
  .unwrap();
}

#[tokio::test]
async fn integration_postgres_update_matching_several_rows_rolls_back() {
  let Some(pg) = test_connection_from_env().await else {
    return;
  };
  // Both audit_log seed rows share the same `at` (inserted in one statement):
  // keying on it matches 2 rows and must trip the exactly-one guard.
  let shared_at = pg
    .run_query("SELECT at::text FROM public.audit_log GROUP BY at HAVING count(*) > 1 LIMIT 1")
    .await
    .unwrap()
    .statements[0]
    .rows[0][0]
    .clone()
    .unwrap();
  let result = pg
    .apply_changes(&TableChanges {
      schema: "public".to_string(),
      table: "audit_log".to_string(),
      updates: vec![crate::connectors::RowUpdate {
        key: vec![cell("at", Some(&shared_at))],
        set: vec![cell("message", Some("clobbered"))],
      }],
      ..no_changes()
    })
    .await;
  let Err(Error::Database { message }) = result else {
    panic!("expected the multi-match update to fail");
  };
  assert!(message.contains("matched 2 rows"), "{message}");

  let clobbered = pg
    .run_query("SELECT count(*) FROM public.audit_log WHERE message = 'clobbered'")
    .await
    .unwrap();
  assert_eq!(clobbered.statements[0].rows[0][0].as_deref(), Some("0"));
}

#[tokio::test]
async fn integration_postgres_write_values_cannot_inject_sql() {
  let Some(pg) = test_connection_from_env().await else {
    return;
  };
  let hostile = "'; DROP TABLE app.customers; --";
  let email = "hostile@example.com";
  pg.apply_changes(&TableChanges {
    inserts: vec![crate::connectors::RowInsert {
      values: vec![cell("name", Some(hostile)), cell("email", Some(email))],
    }],
    ..no_changes()
  })
  .await
  .unwrap();

  // Stored literally, executed never.
  let stored = filtered_rows(
    &pg,
    "customers",
    vec![filter("email", FilterOp::Eq, Some(email))],
  )
  .await;
  assert_eq!(stored.rows[0][1].as_deref(), Some(hostile));

  pg.apply_changes(&TableChanges {
    deletes: vec![crate::connectors::RowDelete {
      key: vec![cell("email", Some(email))],
    }],
    ..no_changes()
  })
  .await
  .unwrap();
}

#[tokio::test]
async fn integration_postgres_invalid_cast_applies_nothing() {
  let Some(pg) = test_connection_from_env().await else {
    return;
  };
  let result = pg
    .apply_changes(&TableChanges {
      schema: "app".to_string(),
      table: "orders".to_string(),
      updates: vec![crate::connectors::RowUpdate {
        key: vec![cell("id", Some("1"))],
        set: vec![cell("amount", Some("not-a-number"))],
      }],
      ..no_changes()
    })
    .await;
  let Err(Error::Database { message }) = result else {
    panic!("expected the cast to fail");
  };
  assert!(message.contains("invalid input syntax"), "{message}");

  let intact = filtered_rows(&pg, "orders", vec![filter("id", FilterOp::Eq, Some("1"))]).await;
  assert_eq!(intact.rows[0][2].as_deref(), Some("129.90"));
}

#[tokio::test]
async fn integration_postgres_ctid_editing_on_pkless_table() {
  let Some(pg) = test_connection_from_env().await else {
    return;
  };
  let inserted = pg
    .apply_changes(&TableChanges {
      schema: "public".to_string(),
      table: "audit_log".to_string(),
      inserts: vec![crate::connectors::RowInsert {
        values: vec![cell("message", Some("ctid test row"))],
      }],
      ..no_changes()
    })
    .await
    .unwrap();
  assert_eq!(inserted.inserted, 1);

  let fetched = pg
    .table_rows(&TableRowsRequest {
      schema: "public".to_string(),
      table: "audit_log".to_string(),
      limit: Some(100),
      offset: 0,
      sort: None,
      filters: vec![filter("message", FilterOp::Eq, Some("ctid test row"))],
      include_ctid: true,
      include_xmin: false,
    })
    .await
    .unwrap();
  let statement = &fetched.statements[0];
  assert_eq!(statement.columns[0].name, "ctid");
  let ctid = statement.rows[0][0].clone().unwrap();

  let deleted = pg
    .apply_changes(&TableChanges {
      schema: "public".to_string(),
      table: "audit_log".to_string(),
      deletes: vec![crate::connectors::RowDelete {
        key: vec![cell("ctid", Some(&ctid))],
      }],
      ..no_changes()
    })
    .await
    .unwrap();
  assert_eq!(deleted.deleted, 1);
}

#[tokio::test]
async fn integration_postgres_filter_values_cannot_inject_sql() {
  let Some(pg) = test_connection_from_env().await else {
    return;
  };
  // Values are bound parameters: hostile input is compared, never executed.
  for hostile in [
    "'; DROP TABLE app.customers; --",
    "1 OR 1=1",
    "Ada' OR name <> '",
  ] {
    let rows = filtered_rows(
      &pg,
      "customers",
      vec![filter("name", FilterOp::Eq, Some(hostile))],
    )
    .await;
    assert!(rows.rows.is_empty(), "{hostile:?} must match nothing");
    let contains = filtered_rows(
      &pg,
      "customers",
      vec![filter("name", FilterOp::Contains, Some(hostile))],
    )
    .await;
    assert!(contains.rows.is_empty(), "{hostile:?} must match nothing");
  }
  // The table survived every attempt. (No global count: the write tests
  // running in parallel insert their own temporary rows.)
  let intact = filtered_rows(
    &pg,
    "customers",
    vec![filter("name", FilterOp::Eq, Some("Ada Lovelace"))],
  )
  .await;
  assert_eq!(intact.rows.len(), 1);
}

#[tokio::test]
async fn integration_postgres_filters_combine_with_sort_and_offset() {
  let Some(pg) = test_connection_from_env().await else {
    return;
  };
  let result = pg
    .table_rows(&TableRowsRequest {
      schema: "app".to_string(),
      table: "orders".to_string(),
      limit: Some(1),
      offset: 1,
      sort: Some(crate::connectors::SortSpec {
        column: "amount".to_string(),
        direction: SortDirection::Desc,
      }),
      filters: vec![filter("amount", FilterOp::Gt, Some("40"))],
      include_ctid: false,
      include_xmin: false,
    })
    .await
    .unwrap();
  // amounts > 40 sorted desc: 999.99, 129.90, 49.00 -> offset 1 = 129.90.
  assert_eq!(result.statements[0].rows[0][2].as_deref(), Some("129.90"));
}

fn profile_from_env_url(url: &str) -> ConnectionProfile {
  let config: Config = url.parse().unwrap();
  // Host::Unix is cfg(unix), so on Windows this match is exhaustive with the one
  // arm: a wildcard would be unreachable there and -D warnings would refuse it.
  let host = match &config.get_hosts()[0] {
    tokio_postgres::config::Host::Tcp(host) => host.clone(),
    #[cfg(unix)]
    tokio_postgres::config::Host::Unix(socket) => {
      panic!("expected a tcp host, got the socket {}", socket.display())
    }
  };
  ConnectionProfile {
    id: String::new(),
    name: "test".to_string(),
    env: crate::profiles::Env::Dev,
    group: None,
    agent_access: Default::default(),
    credential: Default::default(),
    params: crate::profiles::ConnectorParams::Postgres(SqlServerParams {
      host,
      port: config.get_ports()[0],
      database: config.get_dbname().unwrap().to_string(),
      user: config.get_user().unwrap().to_string(),
      ssl_mode: SslMode::Prefer,
      ssl_root_cert: None,
      tunnel_id: None,
    }),
  }
}

#[tokio::test]
async fn integration_postgres_auth_failure_maps_to_database_error() {
  let Ok(url) = std::env::var("SOQUEL_TEST_PG") else {
    return;
  };
  let profile = profile_from_env_url(&url);
  let result = PostgresConnector
    .connect(
      &profile,
      Credentials::fixed(Some("definitely-wrong".to_string())),
      None,
    )
    .await;
  let Err(Error::Database { message }) = result.map(|_| ()) else {
    panic!("expected a database error");
  };
  assert!(
    message.contains("password authentication failed"),
    "unhelpful message: {message}"
  );
}

#[tokio::test]
async fn integration_postgres_unreachable_maps_to_database_error() {
  let Ok(url) = std::env::var("SOQUEL_TEST_PG") else {
    return;
  };
  let mut profile = profile_from_env_url(&url);
  match &mut profile.params {
    crate::profiles::ConnectorParams::Postgres(params) => params.port = 59999,
    _ => unreachable!(),
  }
  let result = PostgresConnector
    .connect(&profile, Credentials::fixed(None), None)
    .await;
  let Err(Error::Database { message }) = result.map(|_| ()) else {
    panic!("expected a database error");
  };
  assert!(!message.is_empty());
}

/// A script that prints the password and records each run, so a test can tell
/// how many times the core asked for it.
fn counting_password_script(
  dir: &tempfile::TempDir,
  password: &str,
) -> (String, std::path::PathBuf) {
  let runs = dir.path().join("runs");
  let script = dir.path().join("password.sh");
  std::fs::write(
    &script,
    format!(
      "#!/bin/sh\necho run >> {runs}\nprintf %s '{password}'\n",
      runs = runs.display()
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

fn env_password(url: &str) -> String {
  let config: Config = url.parse().unwrap();
  String::from_utf8(config.get_password().unwrap().to_vec()).unwrap()
}

#[tokio::test]
async fn integration_postgres_password_from_a_command() {
  let Ok(url) = std::env::var("SOQUEL_TEST_PG") else {
    return;
  };
  let spec =
    crate::credentials::parse_command(&format!("printf %s {}", env_password(&url))).unwrap();
  let connection = PostgresConnector
    .connect(
      &profile_from_env_url(&url),
      Credentials::command(spec, Duration::from_secs(300)),
      None,
    )
    .await
    .unwrap();
  connection.health().await.unwrap();
  connection.close().await.unwrap();
}

#[tokio::test]
async fn integration_postgres_expired_command_password_is_resolved_again() {
  let Ok(url) = std::env::var("SOQUEL_TEST_PG") else {
    return;
  };
  let dir = tempfile::tempdir().unwrap();
  let (script, runs) = counting_password_script(&dir, &env_password(&url));
  let spec = crate::credentials::parse_command(&script).unwrap();
  let connection = PostgresConnector
    .connect(
      &profile_from_env_url(&url),
      // Expired by the time the next connection is built.
      Credentials::command(spec, Duration::from_millis(1)),
      None,
    )
    .await
    .unwrap();
  // A session is a fresh connection: the pool must ask the command again.
  let session = connection.sql().unwrap().open_session().await.unwrap();
  session.run_query("SELECT 1").await.unwrap();

  let count = std::fs::read_to_string(&runs).unwrap().lines().count();
  assert!(count >= 2, "the command ran {count} time(s)");
  session.close().await.unwrap();
  connection.close().await.unwrap();
}

#[tokio::test]
async fn integration_postgres_read_only_query() {
  let Some(pg) = test_connection_from_env().await else {
    return;
  };

  let result = pg.run_read_only_query("SELECT 1 AS one").await.unwrap();
  assert_eq!(result.statements[0].rows[0][0].as_deref(), Some("1"));

  let err = pg
    .run_read_only_query("UPDATE app.customers SET name = name")
    .await
    .unwrap_err();
  let Error::Database { message } = err else {
    panic!("expected a database error: {err:?}");
  };
  assert!(message.contains("read-only"), "{message}");

  let err = pg
    .run_read_only_query("CREATE TABLE app.agent_leak (id int)")
    .await
    .unwrap_err();
  let Error::Database { message } = err else {
    panic!("expected a database error: {err:?}");
  };
  assert!(message.contains("read-only"), "{message}");

  // Multiple statements cannot escape the transaction: prepare rejects them.
  let err = pg
    .run_read_only_query("SELECT 1; DROP TABLE app.customers")
    .await
    .unwrap_err();
  let Error::Database { message } = err else {
    panic!("expected a database error: {err:?}");
  };
  assert!(message.contains("multiple commands"), "{message}");

  // The agent cap is armed inside the transaction, and gone once it ends.
  let armed = pg
    .run_read_only_query("SHOW statement_timeout")
    .await
    .unwrap();
  assert_eq!(armed.statements[0].rows[0][0].as_deref(), Some("30s"));
  let after = pg.run_query("SHOW statement_timeout").await.unwrap();
  assert_ne!(after.statements[0].rows[0][0].as_deref(), Some("30s"));

  // The pooled connection comes back clean and writable for the app itself.
  pg.run_query("SELECT 1").await.unwrap();
  pg.run_query("UPDATE app.customers SET name = name WHERE id = 1")
    .await
    .unwrap();
}
