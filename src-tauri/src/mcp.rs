//! Tauri glue over the core MCP server: the approval dialog and its event.

use std::sync::Arc;

use serde::Serialize;
use specta::Type;
use tauri::{AppHandle, Manager};
use tauri_specta::Event as _;

use soquel_core::mcp::{register_approval, wait_for_approval, Approver};
use soquel_core::{AppState, ApprovalAnswer};

/// The MCP call stays blocked until this is answered.
#[derive(Debug, Clone, Serialize, serde::Deserialize, Type, tauri_specta::Event)]
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

/// Emits to the webview and waits for the dialog; a silent UI denies by timeout.
pub struct DialogApprover {
  pub app: AppHandle,
}

#[async_trait::async_trait]
impl Approver for DialogApprover {
  async fn request(
    &self,
    state: &AppState,
    request: soquel_core::mcp::McpApprovalRequest,
  ) -> ApprovalAnswer {
    let id = request.id.clone();
    let receiver = register_approval(state, &id).await;
    let event = McpApprovalRequest {
      id: request.id,
      connection_id: request.connection_id,
      connection_name: request.connection_name,
      operation: request.operation,
      payload: request.payload,
    };
    if event.emit(&self.app).is_err() {
      state.approvals.lock().await.remove(&id);
      return ApprovalAnswer::Deny;
    }
    wait_for_approval(state, &id, receiver).await
  }
}

/// A factory the core calls per MCP session to raise the approval dialog.
pub fn make_approver(app: &AppHandle) -> Arc<dyn Fn() -> Arc<dyn Approver> + Send + Sync> {
  let app = app.clone();
  Arc::new(move || Arc::new(DialogApprover { app: app.clone() }) as Arc<dyn Approver>)
}

/// Spawns the core autostart on the tauri runtime with a dialog-backed approver.
pub fn autostart(app: &AppHandle) {
  let state: Arc<AppState> = app.state::<Arc<AppState>>().inner().clone();
  let make = make_approver(app);
  tauri::async_runtime::spawn(async move {
    soquel_core::mcp::autostart(state, make).await;
  });
}
