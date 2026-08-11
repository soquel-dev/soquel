use std::sync::Arc;

use serde::{Deserialize, Serialize};
use specta::Type;

use crate::credentials::Credentials;
use crate::error::Error;
use crate::mongo::MongoConnector;
use crate::mysql::MysqlConnector;
use crate::postgres::PostgresConnector;
use crate::profiles::{ConnectionProfile, ConnectorKind};
use crate::redis::RedisConnector;
use crate::sqlite::SqliteConnector;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
#[serde(rename_all = "kebab-case")]
pub enum Capability {
  SqlQuery,
  Introspection,
  KvBrowse,
  DocBrowse,
}

/// Coarse type family for UI decisions (alignment, editors, viewers);
/// `data_type` keeps the exact postgres name.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "kebab-case")]
pub enum ColumnKind {
  Bool,
  Number,
  Text,
  Json,
  Bytes,
  DateTime,
  Uuid,
  Array,
  #[default]
  Other,
}

// Deserialize: the export operations take columns back from the frontend.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct QueryColumn {
  pub name: String,
  /// None when type metadata is unavailable (multi-statement scripts).
  pub data_type: Option<String>,
  pub kind: ColumnKind,
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ServerNotice {
  pub severity: String,
  pub message: String,
}

#[derive(Debug, Clone, Default, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct StatementResult {
  pub columns: Vec<QueryColumn>,
  pub rows: Vec<Vec<Option<String>>>,
  pub rows_affected: f64,
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct QueryResult {
  pub statements: Vec<StatementResult>,
  pub notices: Vec<ServerNotice>,
  pub duration_ms: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Type)]
#[serde(rename_all = "kebab-case")]
pub enum SortDirection {
  Asc,
  Desc,
}

#[derive(Debug, Clone, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct SortSpec {
  pub column: String,
  pub direction: SortDirection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Type)]
#[serde(rename_all = "kebab-case")]
pub enum FilterOp {
  Eq,
  Neq,
  Lt,
  Lte,
  Gt,
  Gte,
  Contains,
  StartsWith,
  IsNull,
  IsNotNull,
}

#[derive(Debug, Clone, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ColumnFilter {
  pub column: String,
  pub op: FilterOp,
  /// Absent for the null operators.
  pub value: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct TableRowsRequest {
  pub schema: String,
  pub table: String,
  /// None streams the full result set (export).
  pub limit: Option<u32>,
  pub offset: u32,
  pub sort: Option<SortSpec>,
  #[serde(default)]
  pub filters: Vec<ColumnFilter>,
  /// ctid-keyed editing for tables without a primary key.
  #[serde(default)]
  pub include_ctid: bool,
  /// Optimistic-lock guard for editing: any concurrent write bumps xmin.
  #[serde(default)]
  pub include_xmin: bool,
}

#[derive(Debug, Clone, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct CellValue {
  pub column: String,
  /// None writes NULL.
  pub value: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct RowUpdate {
  pub key: Vec<CellValue>,
  pub set: Vec<CellValue>,
}

#[derive(Debug, Clone, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct RowInsert {
  /// Omitted columns take their DEFAULT.
  pub values: Vec<CellValue>,
}

#[derive(Debug, Clone, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct RowDelete {
  pub key: Vec<CellValue>,
}

#[derive(Debug, Clone, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct TableChanges {
  pub schema: String,
  pub table: String,
  pub updates: Vec<RowUpdate>,
  pub inserts: Vec<RowInsert>,
  pub deletes: Vec<RowDelete>,
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct RowsChunk {
  /// Present on the first chunk only.
  pub columns: Option<Vec<QueryColumn>>,
  pub rows: Vec<Vec<Option<String>>>,
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct StreamSummary {
  pub rows: f64,
  pub duration_ms: f64,
  pub notices: Vec<ServerNotice>,
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ApplyResult {
  pub updated: u32,
  pub inserted: u32,
  pub deleted: u32,
  pub duration_ms: f64,
}

/// Chunk consumer for `stream_rows`; returning false aborts (receiver gone).
pub type ChunkSink = Box<dyn Fn(RowsChunk) -> bool + Send>;

/// SQL capability surface; only connections whose connector declares
/// `Capability::SqlQuery` expose it.
#[async_trait::async_trait]
pub trait SqlQuery: Send + Sync {
  async fn run_query(&self, sql: &str) -> Result<QueryResult, Error>;
  /// Single statement for the agent surface, read-only enforced by the engine
  /// (never by SQL parsing).
  async fn run_read_only_query(&self, sql: &str) -> Result<QueryResult, Error>;
  async fn cancel(&self) -> Result<(), Error>;
  async fn table_rows(&self, request: &TableRowsRequest) -> Result<QueryResult, Error>;
  async fn stream_rows(
    &self,
    request: &TableRowsRequest,
    on_chunk: ChunkSink,
  ) -> Result<StreamSummary, Error>;
  async fn apply_changes(&self, changes: &TableChanges) -> Result<ApplyResult, Error>;
  async fn open_session(&self) -> Result<Box<dyn SqlSession>, Error>;
}

/// A dedicated client outside the pool: session state (SET, transactions)
/// sticks, and cancel targets only this session.
#[async_trait::async_trait]
pub trait SqlSession: Send + Sync {
  async fn run_query(&self, sql: &str) -> Result<QueryResult, Error>;
  async fn cancel(&self) -> Result<(), Error>;
  async fn close(&self) -> Result<(), Error>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
#[serde(rename_all = "kebab-case")]
pub enum TableKind {
  Table,
  View,
  MaterializedView,
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ColumnInfo {
  pub name: String,
  pub data_type: String,
  pub nullable: bool,
  pub default: Option<String>,
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct IndexInfo {
  pub name: String,
  pub definition: String,
  pub unique: bool,
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ForeignKeyInfo {
  pub name: String,
  pub columns: Vec<String>,
  pub referenced_schema: String,
  pub referenced_table: String,
  pub referenced_columns: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct TableInfo {
  pub name: String,
  pub kind: TableKind,
  /// Planner estimate (pg reltuples): -1 when never analyzed.
  pub estimated_rows: f64,
  pub columns: Vec<ColumnInfo>,
  pub primary_key: Vec<String>,
  pub indexes: Vec<IndexInfo>,
  pub foreign_keys: Vec<ForeignKeyInfo>,
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct SchemaInfo {
  pub name: String,
  pub tables: Vec<TableInfo>,
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct SchemaSnapshot {
  pub schemas: Vec<SchemaInfo>,
}

/// Introspection capability surface, mirroring `Capability::Introspection`.
#[async_trait::async_trait]
pub trait Introspect: Send + Sync {
  async fn schema_snapshot(&self) -> Result<SchemaSnapshot, Error>;
  async fn table_ddl(&self, schema: &str, table: &str) -> Result<String, Error>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
#[serde(rename_all = "kebab-case")]
pub enum KeyKind {
  String,
  List,
  Set,
  Zset,
  Hash,
  Stream,
  Other,
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct KeyEntry {
  pub key: String,
  pub kind: KeyKind,
  /// Milliseconds to expiry; None = no expiry.
  pub ttl_ms: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct KeyScanPage {
  pub keys: Vec<KeyEntry>,
  /// Opaque continuation cursor; None when the iteration completed.
  pub cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ZsetMember {
  pub member: String,
  pub score: f64,
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct HashField {
  pub field: String,
  pub value: String,
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct StreamEntry {
  pub id: String,
  pub fields: Vec<HashField>,
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(
  tag = "kind",
  rename_all = "kebab-case",
  rename_all_fields = "camelCase"
)]
pub enum KeyValue {
  String { value: String },
  List { entries: Vec<String> },
  Set { entries: Vec<String> },
  Zset { entries: Vec<ZsetMember> },
  Hash { entries: Vec<HashField> },
  Stream { entries: Vec<StreamEntry> },
  Other { type_name: String },
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct KeyDetail {
  pub key: String,
  /// Milliseconds to expiry; None = no expiry.
  pub ttl_ms: Option<f64>,
  /// Full collection length (bytes for strings); entries hold a bounded sample.
  pub size: f64,
  pub value: KeyValue,
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct KvDatabaseKeys {
  pub db: u32,
  pub keys: f64,
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct KvDatabases {
  pub current: u32,
  /// Configured database count; selection targets 0..total.
  pub total: u32,
  /// Non-empty databases with their key counts.
  pub used: Vec<KvDatabaseKeys>,
}

/// Key-value capability surface, mirroring `Capability::KvBrowse`.
#[async_trait::async_trait]
pub trait KvBrowse: Send + Sync {
  async fn databases(&self) -> Result<KvDatabases, Error>;
  async fn scan_keys(
    &self,
    pattern: &str,
    cursor: Option<&str>,
    count: u32,
  ) -> Result<KeyScanPage, Error>;
  async fn key_detail(&self, key: &str) -> Result<KeyDetail, Error>;
  async fn set_string(&self, key: &str, value: &str) -> Result<(), Error>;
  async fn delete_key(&self, key: &str) -> Result<(), Error>;
  /// None clears the expiry (PERSIST).
  async fn set_ttl(&self, key: &str, ttl_ms: Option<f64>) -> Result<(), Error>;
  /// One console command; the reply rendered as display lines.
  async fn run_command(&self, command: &str) -> Result<Vec<String>, Error>;
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct DocDatabase {
  pub name: String,
  /// listDatabases sizeOnDisk; None when a restricted user forced the fallback path.
  pub size_bytes: Option<f64>,
  pub empty: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
#[serde(rename_all = "kebab-case")]
pub enum DocCollectionKind {
  Collection,
  View,
  Timeseries,
  Other,
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct DocCollection {
  pub name: String,
  pub kind: DocCollectionKind,
  /// estimatedDocumentCount (collection metadata); None for views.
  pub estimated_docs: Option<f64>,
  pub capped: bool,
}

/// One document on the wire. `doc` is relaxed extended JSON (display); `id` is
/// canonical extended JSON of the `_id` value alone - the lossless address for
/// get/replace/delete (the display form must never double as the key).
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct DocEntry {
  /// None for documents without `_id`; edit/delete are unavailable then.
  pub id: Option<String>,
  pub doc: String,
}

#[derive(Debug, Clone, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct DocFindRequest {
  pub db: String,
  pub collection: String,
  /// Extended JSON object (canonical or relaxed); None/empty = {}.
  #[serde(default)]
  pub filter: Option<String>,
  /// Extended JSON object, e.g. {"age": -1}.
  #[serde(default)]
  pub sort: Option<String>,
  /// Page size; clamped server-side.
  pub limit: u32,
  /// Opaque continuation from the previous page; None starts over.
  #[serde(default)]
  pub cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct DocPage {
  pub docs: Vec<DocEntry>,
  /// Opaque continuation cursor; None when the iteration completed.
  pub cursor: Option<String>,
}

/// Both renderings of one document: relaxed for reading, canonical for a
/// lossless edit round-trip (relaxed collapses Int32/Int64/Double).
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct DocDetail {
  pub id: Option<String>,
  pub relaxed: String,
  pub canonical: String,
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct DocCount {
  pub count: f64,
  /// false when this is a metadata estimate or the exact count hit the cap.
  pub exact: bool,
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct DocQueryResult {
  /// Relaxed extended JSON strings; console results are read-only.
  pub docs: Vec<String>,
  /// true when results were cut at the sample cap.
  pub truncated: bool,
  pub duration_ms: f64,
}

/// Document capability surface, mirroring `Capability::DocBrowse`.
#[async_trait::async_trait]
pub trait DocBrowse: Send + Sync {
  async fn databases(&self) -> Result<Vec<DocDatabase>, Error>;
  async fn collections(&self, db: &str) -> Result<Vec<DocCollection>, Error>;
  async fn find_docs(&self, request: &DocFindRequest) -> Result<DocPage, Error>;
  /// `id` is the canonical extended JSON produced by `DocEntry.id`.
  async fn doc_detail(&self, db: &str, collection: &str, id: &str) -> Result<DocDetail, Error>;
  /// Edit = full replacement; a doc carrying a different `_id` is rejected server-side.
  async fn replace_doc(&self, db: &str, collection: &str, id: &str, doc: &str)
    -> Result<(), Error>;
  async fn delete_doc(&self, db: &str, collection: &str, id: &str) -> Result<(), Error>;
  async fn indexes(&self, db: &str, collection: &str) -> Result<Vec<IndexInfo>, Error>;
  /// No filter -> estimatedDocumentCount; filter -> countDocuments, capped.
  async fn count_docs(
    &self,
    db: &str,
    collection: &str,
    filter: Option<&str>,
  ) -> Result<DocCount, Error>;
  /// Console entry point: an object is a find filter, an array is an aggregate
  /// pipeline ($out/$merge rejected). Results capped at the sample size.
  async fn run_query(
    &self,
    db: &str,
    collection: &str,
    source: &str,
  ) -> Result<DocQueryResult, Error>;
}

/// A live connection to a database, produced by a `Connector`.
#[async_trait::async_trait]
pub trait Connection: Send + Sync {
  async fn health(&self) -> Result<(), Error>;
  async fn close(&self) -> Result<(), Error>;
  /// Human-readable server version, captured at connect when available.
  fn server_version(&self) -> Option<String> {
    None
  }
  fn sql(&self) -> Option<&dyn SqlQuery> {
    None
  }
  fn introspect(&self) -> Option<&dyn Introspect> {
    None
  }
  fn kv(&self) -> Option<&dyn KvBrowse> {
    None
  }
  fn doc(&self) -> Option<&dyn DocBrowse> {
    None
  }
}

/// Shared across SQL connectors.
pub const POOL_MAX_SIZE: usize = 4;
pub const CHUNK_ROWS: usize = 200;
/// Ceiling on agent queries, enforced by the engine: a runaway agent must not
/// hold a pooled connection forever. The UI stays uncapped (users can cancel).
pub const AGENT_STATEMENT_TIMEOUT_MS: u32 = 30_000;

/// First keyword of a statement, past whitespace, comments and opening parens.
pub fn statement_head(sql: &str) -> String {
  let mut rest = sql;
  loop {
    rest = rest.trim_start();
    if let Some(stripped) = rest.strip_prefix("--").or_else(|| rest.strip_prefix('#')) {
      rest = stripped.split_once('\n').map_or("", |(_, tail)| tail);
    } else if let Some(stripped) = rest.strip_prefix("/*") {
      rest = stripped.split_once("*/").map_or("", |(_, tail)| tail);
    } else if let Some(stripped) = rest.strip_prefix('(') {
      rest = stripped;
    } else {
      break;
    }
  }
  rest
    .chars()
    .take_while(|c| c.is_ascii_alphabetic())
    .collect()
}

/// Whether a statement only reads, across engines. Deliberately generous: a
/// false "read" still hits the engine-enforced read-only path, while a false
/// "write" only costs a needless approval prompt.
pub fn is_read_statement(sql: &str) -> bool {
  const READ_HEADS: [&str; 8] = [
    "SELECT", "WITH", "TABLE", "VALUES", "SHOW", "EXPLAIN", "DESCRIBE", "DESC",
  ];
  let head = statement_head(sql);
  READ_HEADS
    .iter()
    .any(|candidate| head.eq_ignore_ascii_case(candidate))
}

/// Update/delete guard shared by SQL connectors: exactly one row or the whole
/// batch rolls back.
pub fn verify_exactly_one(kind: &str, affected: u64) -> Result<(), Error> {
  if affected == 1 {
    return Ok(());
  }
  let hint = if affected == 0 {
    "; the row may have been changed or deleted since it was loaded - refresh and retry"
  } else {
    ""
  };
  Err(Error::Database {
    message: format!(
      "a {kind} matched {affected} rows instead of exactly 1; nothing was applied{hint}"
    ),
  })
}

/// Tracks the cancel handles of in-flight queries; the guard unregisters on drop.
pub struct CancelRegistry<T> {
  entries: std::sync::Mutex<std::collections::HashMap<u64, T>>,
  next_id: std::sync::atomic::AtomicU64,
}

impl<T: Clone> Default for CancelRegistry<T> {
  fn default() -> Self {
    Self {
      entries: std::sync::Mutex::new(std::collections::HashMap::new()),
      next_id: std::sync::atomic::AtomicU64::new(0),
    }
  }
}

impl<T: Clone> CancelRegistry<T> {
  pub fn register(&self, token: T) -> CancelGuard<'_, T> {
    let id = self
      .next_id
      .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    self.entries.lock().unwrap().insert(id, token);
    CancelGuard { registry: self, id }
  }

  pub fn tokens(&self) -> Vec<T> {
    self.entries.lock().unwrap().values().cloned().collect()
  }
}

pub struct CancelGuard<'a, T> {
  registry: &'a CancelRegistry<T>,
  id: u64,
}

impl<T> Drop for CancelGuard<'_, T> {
  fn drop(&mut self) {
    self.registry.entries.lock().unwrap().remove(&self.id);
  }
}

/// Local TCP endpoint of an SSH forward. TCP dials 127.0.0.1:{port} while the
/// profile keeps the logical host, so TLS verification still targets it.
#[derive(Debug, Clone, Copy)]
pub struct LocalForward {
  pub port: u16,
}

/// A database kind the app knows how to talk to. Capabilities drive the UI:
/// no capability may assume SQL (Redis browses keys, not tables).
#[async_trait::async_trait]
pub trait Connector: Send + Sync {
  fn capabilities(&self) -> &'static [Capability];
  /// `secret` is resolved lazily: a pooling connector keeps it and asks again
  /// when it builds a connection, so a short-lived token can be refreshed.
  async fn connect(
    &self,
    profile: &ConnectionProfile,
    secret: Arc<Credentials>,
    forward: Option<LocalForward>,
  ) -> Result<Box<dyn Connection>, Error>;
}

// Exhaustive match: adding a ConnectorKind refuses to compile until it gets a connector.
pub fn connector_for(kind: ConnectorKind) -> &'static dyn Connector {
  match kind {
    ConnectorKind::Postgres => &PostgresConnector,
    ConnectorKind::Mysql => &MysqlConnector,
    ConnectorKind::Sqlite => &SqliteConnector,
    ConnectorKind::Redis => &RedisConnector,
    ConnectorKind::Mongo => &MongoConnector,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn postgres_declares_sql_capabilities() {
    let caps = connector_for(ConnectorKind::Postgres).capabilities();
    assert!(caps.contains(&Capability::SqlQuery));
    assert!(caps.contains(&Capability::Introspection));
    assert!(!caps.contains(&Capability::KvBrowse));
  }

  #[test]
  fn sqlite_declares_sql_capabilities() {
    let caps = connector_for(ConnectorKind::Sqlite).capabilities();
    assert!(caps.contains(&Capability::SqlQuery));
    assert!(caps.contains(&Capability::Introspection));
    assert!(!caps.contains(&Capability::KvBrowse));
  }

  #[test]
  fn redis_declares_kv_only() {
    let caps = connector_for(ConnectorKind::Redis).capabilities();
    assert!(caps.contains(&Capability::KvBrowse));
    assert!(!caps.contains(&Capability::SqlQuery));
    assert!(!caps.contains(&Capability::Introspection));
  }

  #[test]
  fn mongo_declares_doc_only() {
    let caps = connector_for(ConnectorKind::Mongo).capabilities();
    assert!(caps.contains(&Capability::DocBrowse));
    assert!(!caps.contains(&Capability::SqlQuery));
    assert!(!caps.contains(&Capability::Introspection));
    assert!(!caps.contains(&Capability::KvBrowse));
  }
}
