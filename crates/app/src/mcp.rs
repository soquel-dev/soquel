//! The MCP panel (server toggle, port, setup command, trust windows), the audit
//! log dialog, and the gpui `Approver`: a blocked write reaches the coordinator
//! through a channel, the human's answer fires the oneshot the server thread
//! waits on.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use futures::channel::mpsc;
use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::switch::Switch;
use gpui_component::{ActiveTheme, Sizable, StyledExt, WindowExt, h_flex, v_flex};
use soquel_core::mcp::{Approver, AuditEntry, McpApprovalRequest, McpStatus, TrustWindowInfo};
use soquel_core::profiles::AgentAccess;
use soquel_core::{AppState, ApprovalAnswer};

use crate::core;

/// The access options, ordered as the form's select lists them.
pub const AGENT_ACCESSES: [AgentAccess; 3] = [
  AgentAccess::None,
  AgentAccess::ReadOnly,
  AgentAccess::WriteWithApproval,
];

pub fn agent_access_label(access: AgentAccess) -> &'static str {
  match access {
    AgentAccess::None => "Off",
    AgentAccess::ReadOnly => "Read-only",
    AgentAccess::WriteWithApproval => "Writes need approval",
  }
}

/// Mirrors the core floor; a text field never binds below 1024 (needs root
/// on unix) or above the u16 ceiling.
pub fn parse_port(input: &str) -> Result<u16, String> {
  let value: i64 = input
    .trim()
    .parse()
    .map_err(|_| "Port must be a whole number".to_string())?;
  if value < 1024 {
    return Err("Port must be 1024 or above".to_string());
  }
  if value > 65535 {
    return Err("Port must be below 65536".to_string());
  }
  Ok(value as u16)
}

/// "M:SS" of whole seconds left; a window that has already ended reads "0:00".
pub fn remaining_label(ms: f64) -> String {
  let total = (ms / 1000.0).ceil().max(0.0) as i64;
  format!("{}:{:02}", total / 60, total % 60)
}

/// The one-liner under a running server: how many connections agents can see
/// and how many of those they can write to.
pub fn exposed_summary(exposed: usize, writable: usize) -> String {
  if exposed == 0 {
    return "No connections are exposed yet. Opt one in with the Agent access field in its \
            settings."
      .to_string();
  }
  let noun = if exposed == 1 {
    "connection"
  } else {
    "connections"
  };
  let reach = if writable == 0 {
    "read-only".to_string()
  } else {
    format!("{writable} of them writable with your approval")
  };
  format!("{exposed} {noun} exposed, {reach}. Every agent call lands in the audit log.")
}

pub fn setup_command(server_name: &str, endpoint: &str, token: &str) -> String {
  format!(
    "claude mcp add --transport http {server_name} {endpoint} --header \"Authorization: Bearer \
     {token}\""
  )
}

fn now_ms() -> f64 {
  SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map_or(0.0, |d| d.as_millis() as f64)
}

// The gpui Approver seam -------------------------------------------------------

/// Blocks the server thread on the oneshot the App will fire, after handing the
/// request to the App's drain through the channel. Silence still denies: the
/// 60s timeout and the deny-on-drop both live in core.
struct GpuiApprover {
  tx: mpsc::UnboundedSender<McpApprovalRequest>,
}

#[async_trait::async_trait]
impl Approver for GpuiApprover {
  async fn request(&self, state: &AppState, request: McpApprovalRequest) -> ApprovalAnswer {
    let id = request.id.clone();
    let receiver = soquel_core::mcp::register_approval(state, &id).await;
    if self.tx.unbounded_send(request).is_err() {
      // No UI draining the channel (App gone): nothing can allow it.
      let _ = soquel_core::mcp::resolve_approval(state, &id, ApprovalAnswer::Deny).await;
      return ApprovalAnswer::Deny;
    }
    soquel_core::mcp::wait_for_approval(state, &id, receiver).await
  }
}

pub fn approver_factory(tx: mpsc::UnboundedSender<McpApprovalRequest>) -> core::ApproverFactory {
  Arc::new(move || Arc::new(GpuiApprover { tx: tx.clone() }) as Arc<dyn Approver>)
}

/// The channel the coordinator owns; `rx` is drained into the approval dialog,
/// `tx` builds the approver factory the server injects.
pub fn approval_channel() -> (
  mpsc::UnboundedSender<McpApprovalRequest>,
  mpsc::UnboundedReceiver<McpApprovalRequest>,
) {
  mpsc::unbounded()
}

// The approval coordinator ------------------------------------------------------

/// Process-global owner of the approval queue, created before any window so
/// approvals outlive the hub.
pub struct McpCoordinator {
  state: Arc<AppState>,
  make_approver: core::ApproverFactory,
  pub(crate) approval_queue: Vec<McpApprovalRequest>,
  approval_showing: bool,
  _drain: Task<()>,
}

struct GlobalMcpCoordinator(Entity<McpCoordinator>);

impl Global for GlobalMcpCoordinator {}

pub fn init_coordinator(state: Arc<AppState>, cx: &mut App) -> Entity<McpCoordinator> {
  let coordinator = cx.new(|cx| McpCoordinator::new(state, cx));
  cx.set_global(GlobalMcpCoordinator(coordinator.clone()));
  coordinator
}

pub fn coordinator(cx: &App) -> Entity<McpCoordinator> {
  cx.global::<GlobalMcpCoordinator>().0.clone()
}

pub fn make_approver(cx: &App) -> core::ApproverFactory {
  coordinator(cx).read(cx).make_approver.clone()
}

/// Re-show the front of the queue: approvals park while no window exists.
pub fn poke_approvals(cx: &mut App) {
  if !cx.has_global::<GlobalMcpCoordinator>() {
    return;
  }
  coordinator(cx).update(cx, |coordinator, cx| coordinator.show_next_approval(cx));
}

impl McpCoordinator {
  fn new(state: Arc<AppState>, cx: &mut Context<Self>) -> Self {
    let (tx, mut rx) = approval_channel();
    let make_approver = approver_factory(tx);
    // One drain for the process's life: server restarts reuse the same channel.
    let _drain = cx.spawn(async move |this, cx| {
      while let Some(request) = rx.next().await {
        if this
          .update(cx, |this, cx| this.enqueue_approval(request, cx))
          .is_err()
        {
          break;
        }
      }
    });
    // An enabled server comes back on launch, off the UI thread.
    core::mcp_autostart(state.clone(), make_approver.clone());
    Self {
      state,
      make_approver,
      approval_queue: Vec::new(),
      approval_showing: false,
      _drain,
    }
  }

  pub(crate) fn enqueue_approval(&mut self, request: McpApprovalRequest, cx: &mut Context<Self>) {
    self.approval_queue.push(request);
    self.show_next_approval(cx);
  }

  /// The open dialog is always for the front of the queue; new requests wait
  /// behind it and surface as it drains.
  pub(crate) fn show_next_approval(&mut self, cx: &mut Context<Self>) {
    if self.approval_showing || self.approval_queue.is_empty() {
      return;
    }
    // No window to land on: park; opening the hub pokes the queue again and
    // core's 60s timeout is the backstop.
    if cx.windows().is_empty() {
      return;
    }
    self.approval_showing = true;
    let request = self.approval_queue[0].clone();
    let more = self.approval_queue.len() - 1;
    crate::mcp_approval::open_mcp_approval_dialog(
      cx.entity(),
      request,
      more,
      cx,
      |this, answer, cx| this.resolve_front(answer, cx),
    );
  }

  fn resolve_front(&mut self, answer: ApprovalAnswer, cx: &mut Context<Self>) {
    if self.approval_queue.is_empty() {
      return;
    }
    let request = self.approval_queue.remove(0);
    core::mcp_resolve_approval(self.state.clone(), request.id, answer);
    self.approval_showing = false;
    self.show_next_approval(cx);
  }
}

// The panel --------------------------------------------------------------------

pub struct McpPanel {
  state: Arc<AppState>,
  make_approver: core::ApproverFactory,
  status: Option<McpStatus>,
  windows: Vec<TrustWindowInfo>,
  /// (exposed, write-with-approval) profile counts, recounted on refresh.
  agent_counts: (usize, usize),
  port_input: Entity<InputState>,
  busy: bool,
  problem: Option<SharedString>,
  _task: Task<()>,
  _tick: Task<()>,
  _port_subscription: Subscription,
}

impl McpPanel {
  pub fn new(
    state: Arc<AppState>,
    make_approver: core::ApproverFactory,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) -> Self {
    let port_input = cx.new(|cx| {
      InputState::new(window, cx).default_value(core::mcp_configured_port(&state).to_string())
    });
    // Commit on blur and Enter, never per keystroke: a change restarts the server.
    let _port_subscription = cx.subscribe(&port_input, |this, _, event: &InputEvent, cx| {
      if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur) {
        this.apply_port(cx);
      }
    });
    // The countdown ticks locally so a window's row leaves without a round trip.
    let _tick = cx.spawn(async move |this, cx| {
      loop {
        cx.background_executor().timer(Duration::from_secs(1)).await;
        if this.update(cx, |_, cx| cx.notify()).is_err() {
          break;
        }
      }
    });
    let mut panel = Self {
      state,
      make_approver,
      status: None,
      windows: Vec::new(),
      agent_counts: (0, 0),
      port_input,
      busy: false,
      problem: None,
      _task: Task::ready(()),
      _tick,
      _port_subscription,
    };
    panel.refresh(cx);
    panel
  }

  fn refresh(&mut self, cx: &mut Context<Self>) {
    // Counted here, not per frame in render: refresh runs on every panel event.
    let profiles = core::list_connections(&self.state);
    self.agent_counts = (
      profiles
        .iter()
        .filter(|p| p.agent_access != AgentAccess::None)
        .count(),
      profiles
        .iter()
        .filter(|p| p.agent_access == AgentAccess::WriteWithApproval)
        .count(),
    );
    let status_task = core::mcp_status(self.state.clone(), cx);
    let windows_task = core::mcp_trust_windows(self.state.clone(), cx);
    self._task = cx.spawn(async move |this, cx| {
      let status = status_task.await;
      let windows = windows_task.await;
      let _ = this.update(cx, |this, cx| {
        if let Some(status) = crate::status::ok_or_log(status) {
          this.status = Some(status);
        }
        this.windows = windows;
        cx.notify();
      });
    });
  }

  fn toggle(&mut self, on: bool, cx: &mut Context<Self>) {
    if self.busy {
      return;
    }
    self.busy = true;
    self.problem = None;
    cx.notify();
    let task = if on {
      core::mcp_start(
        self.state.clone(),
        core::mcp_configured_port(&self.state),
        self.make_approver.clone(),
        cx,
      )
    } else {
      core::mcp_stop(self.state.clone(), cx)
    };
    self._task = cx.spawn(async move |this, cx| {
      let result = task.await;
      let _ = this.update(cx, |this, cx| {
        this.busy = false;
        if let Err(error) = result {
          this.problem = Some(crate::status::message(&error));
        }
        this.refresh(cx);
      });
    });
  }

  fn apply_port(&mut self, cx: &mut Context<Self>) {
    let raw = self.port_input.read(cx).value().to_string();
    let port = match parse_port(&raw) {
      Ok(port) => port,
      Err(message) => {
        self.problem = Some(message.into());
        cx.notify();
        return;
      }
    };
    self.problem = None;
    let current = self.status.as_ref().map(|s| s.port);
    if current == Some(port) {
      cx.notify();
      return;
    }
    let was_running = self.status.as_ref().is_some_and(|s| s.running);
    self.busy = true;
    cx.notify();
    let state = self.state.clone();
    let make_approver = self.make_approver.clone();
    self._task = cx.spawn(async move |this, cx| {
      // Persist before restarting: the choice survives even if the new port
      // fails to bind.
      if was_running {
        crate::status::ok_or_log(core::mcp_stop(state.clone(), cx).await);
      }
      let set = core::mcp_set_port(state.clone(), port, cx).await;
      let restart = if was_running && matches!(set, Ok(())) {
        Some(core::mcp_start(state.clone(), port, make_approver, cx).await)
      } else {
        None
      };
      let _ = this.update(cx, |this, cx| {
        this.busy = false;
        this.problem = match (set, restart) {
          (Err(error), _) | (_, Some(Err(error))) => Some(crate::status::message(&error)),
          _ => None,
        };
        this.refresh(cx);
      });
    });
  }

  fn copy_setup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
    let Some(status) = &self.status else {
      return;
    };
    let command = setup_command(&status.server_name, &status.endpoint, &status.token);
    cx.write_to_clipboard(ClipboardItem::new_string(command));
    window.push_notification("Setup command copied", cx);
  }

  fn confirm_regenerate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
    let this = cx.entity().downgrade();
    crate::dialogs::confirm_danger(
      window,
      cx,
      "Regenerate the token?",
      |cx| {
        div()
          .text_sm()
          .text_color(cx.theme().muted_foreground)
          .child(
            "The current token stops working immediately; every agent using it must be \
             reconfigured with the new one.",
          )
          .into_any_element()
      },
      "Regenerate",
      "regenerate-confirm",
      move |_, cx| {
        this.update(cx, |this, cx| this.regenerate(cx)).ok();
      },
    );
  }

  fn regenerate(&mut self, cx: &mut Context<Self>) {
    let task = core::mcp_regenerate_token(self.state.clone(), cx);
    self._task = cx.spawn(async move |this, cx| {
      let result = task.await;
      let _ = this.update(cx, |this, cx| {
        match result {
          Ok(_) => this.refresh(cx),
          Err(error) => this.problem = Some(crate::status::message(&error)),
        }
        cx.notify();
      });
    });
  }

  fn revoke(&mut self, session: String, connection_id: String, cx: &mut Context<Self>) {
    let task = core::mcp_revoke_trust(self.state.clone(), session, connection_id, cx);
    self._task = cx.spawn(async move |this, cx| {
      task.await;
      let _ = this.update(cx, |this, cx| this.refresh(cx));
    });
  }

  fn open_audit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
    let audit = cx.new(|cx| McpAuditView::new(self.state.clone(), cx));
    window.open_dialog(cx, move |dialog, window, cx| {
      crate::dialogs::styled(dialog, window, cx)
        .title(
          div()
            .font_family(crate::theme::mono(cx))
            .child("Agent activity"),
        )
        .w(px(640.))
        .child(audit.clone())
    });
  }

  fn live_windows(&self) -> Vec<(&TrustWindowInfo, String)> {
    let now = now_ms();
    self
      .windows
      .iter()
      .filter(|w| w.expires_at_ms > now)
      .map(|w| (w, remaining_label(w.expires_at_ms - now)))
      .collect()
  }
}

impl Render for McpPanel {
  fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    let running = self.status.as_ref().is_some_and(|s| s.running);
    let has_status = self.status.is_some();
    let endpoint = match &self.status {
      Some(status) if status.running => status.endpoint.clone(),
      _ => "Give coding agents scoped access to your connections.".to_string(),
    };
    let dot = if running {
      cx.theme().primary
    } else {
      cx.theme().muted_foreground.opacity(0.3)
    };

    v_flex()
      .w_full()
      .gap_3()
      .child(
        h_flex()
          .justify_between()
          .items_center()
          // Clear the dialog's absolute close button.
          .pr_8()
          .child(
            div()
              .font_family(crate::theme::mono(cx))
              .text_sm()
              .text_color(cx.theme().muted_foreground)
              .child("agent access (mcp)"),
          )
          .child(
            Button::new("open-audit")
              .outline()
              .small()
              .label("Activity")
              .debug_selector(|| "open-audit".into())
              .on_click(cx.listener(|this, _, window, cx| this.open_audit(window, cx))),
          ),
      )
      .child(
        v_flex()
          .rounded(cx.theme().radius)
          .border_1()
          .border_color(cx.theme().border)
          .child(
            h_flex()
              .items_center()
              .gap_3()
              .px_4()
              .py_3()
              .child(div().size_2().flex_shrink_0().rounded_full().bg(dot))
              .child(
                v_flex()
                  .flex_1()
                  .min_w_0()
                  .child(div().text_sm().child("MCP server"))
                  .child(
                    div()
                      .font_family(crate::theme::mono(cx))
                      .text_xs()
                      .text_color(cx.theme().muted_foreground)
                      .truncate()
                      .child(endpoint),
                  ),
              )
              .child(div().w(px(88.)).child(Input::new(&self.port_input).small()))
              .child(
                Switch::new("mcp-toggle")
                  .checked(running)
                  .on_click(cx.listener(|this, checked: &bool, _, cx| {
                    let on = *checked;
                    this.toggle(on, cx);
                  })),
              ),
          )
          .when_some(self.problem.clone(), |this, problem| {
            this.child(
              div()
                .px_4()
                .pb_3()
                .text_xs()
                .text_color(cx.theme().danger)
                .child(problem),
            )
          })
          .when(running, |this| this.child(self.running_block(cx)))
          .when(has_status && !running, |this| {
            this.child(self.stopped_block(cx))
          }),
      )
  }
}

impl McpPanel {
  fn running_block(&self, cx: &mut Context<Self>) -> impl IntoElement {
    let status = self.status.clone().expect("running block needs a status");
    let (exposed, writable) = self.agent_counts;
    let masked = setup_command(&status.server_name, &status.endpoint, "••••••••");
    let windows = self.live_windows();

    v_flex()
      .gap_3()
      .border_t_1()
      .border_color(cx.theme().border)
      .px_4()
      .py_3()
      .child(
        div()
          .text_xs()
          .text_color(cx.theme().muted_foreground)
          .child(exposed_summary(exposed, writable)),
      )
      .child(
        h_flex()
          .items_center()
          .gap_2()
          .child(
            div()
              .flex_1()
              .min_w_0()
              .px_2()
              .py_1p5()
              .rounded(cx.theme().radius)
              .bg(cx.theme().muted.opacity(0.4))
              .font_family(crate::theme::mono(cx))
              .text_xs()
              .truncate()
              .child(masked),
          )
          .child(
            Button::new("mcp-copy-setup")
              .outline()
              .small()
              .label("Copy")
              .debug_selector(|| "mcp-copy-setup".into())
              .on_click(cx.listener(|this, _, window, cx| this.copy_setup(window, cx))),
          ),
      )
      .child(
        div()
          .text_xs()
          .text_color(cx.theme().muted_foreground)
          .child(
            "The command is for Claude Code; any MCP client over streamable HTTP works with the \
             same URL and token.",
          ),
      )
      .when(!windows.is_empty(), |this| {
        this.child(
          v_flex()
            .gap_2()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(cx.theme().yellow.opacity(0.3))
            .p_3()
            .child(div().text_xs().text_color(cx.theme().yellow).child(
              "Writes are running without asking. Revoke to go back to one dialog per write.",
            ))
            .children(windows.into_iter().map(|(window, remaining)| {
              let session = window.session.clone();
              let connection_id = window.connection_id.clone();
              h_flex()
                .items_center()
                .gap_2()
                .child(
                  div()
                    .flex_1()
                    .min_w_0()
                    .text_xs()
                    .truncate()
                    .child(format!("{}  {} left", window.connection_name, remaining)),
                )
                .child(
                  Button::new(SharedString::from(format!(
                    "revoke-{}-{}",
                    window.session, window.connection_id
                  )))
                  .outline()
                  .small()
                  .label("Revoke")
                  .on_click(cx.listener(move |this, _, _, cx| {
                    this.revoke(session.clone(), connection_id.clone(), cx)
                  })),
                )
            })),
        )
      })
  }

  fn stopped_block(&self, cx: &mut Context<Self>) -> impl IntoElement {
    h_flex()
      .items_center()
      .justify_between()
      .border_t_1()
      .border_color(cx.theme().border)
      .px_4()
      .py_3()
      .child(
        div()
          .text_xs()
          .text_color(cx.theme().muted_foreground)
          .child("Off. Connections stay invisible to agents until the server runs."),
      )
      .child(
        Button::new("mcp-regenerate")
          .ghost()
          .small()
          .label("Regenerate token")
          .debug_selector(|| "mcp-regenerate".into())
          .on_click(cx.listener(|this, _, window, cx| this.confirm_regenerate(window, cx))),
      )
  }
}

// The audit log dialog ---------------------------------------------------------

const AUDIT_LIMIT: usize = 200;

pub struct McpAuditView {
  /// None while the log file loads; Some(empty) is a genuinely empty log.
  entries: Option<Vec<AuditEntry>>,
  names: HashMap<String, String>,
  _task: Task<()>,
}

impl McpAuditView {
  pub fn new(state: Arc<AppState>, cx: &mut Context<Self>) -> Self {
    let names = core::list_connections(&state)
      .into_iter()
      .map(|p| (p.id, p.name))
      .collect();
    let task = core::mcp_audit_log(state, AUDIT_LIMIT, cx);
    let _task = cx.spawn(async move |this, cx| {
      let entries = task.await.unwrap_or_default();
      this
        .update(cx, |this, cx| {
          this.entries = Some(entries);
          cx.notify();
        })
        .ok();
    });
    Self {
      entries: None,
      names,
      _task,
    }
  }

  fn target(&self, entry: &AuditEntry) -> String {
    match &entry.connection {
      Some(id) => self.names.get(id).cloned().unwrap_or_else(|| id.clone()),
      None => String::new(),
    }
  }
}

/// UTC HH:MM:SS of an epoch-ms timestamp; enough to order a session's calls.
fn clock(ts_ms: f64) -> String {
  let secs = (ts_ms / 1000.0) as i64;
  let day = secs.rem_euclid(86_400);
  format!("{:02}:{:02}:{:02}", day / 3600, (day % 3600) / 60, day % 60)
}

impl Render for McpAuditView {
  fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    let quiet = |text: &'static str| {
      v_flex().w_full().child(
        div()
          .py_8()
          .text_center()
          .text_sm()
          .text_color(cx.theme().muted_foreground)
          .child(text),
      )
    };
    let Some(entries) = &self.entries else {
      return quiet("Loading…");
    };
    if entries.is_empty() {
      return quiet("Nothing yet. Calls appear here as agents use your connections.");
    }
    let rows: Vec<AnyElement> = entries
      .iter()
      .map(|entry| {
        let dot = if entry.ok {
          cx.theme().primary
        } else {
          cx.theme().danger
        };
        v_flex()
          .w_full()
          .gap_0p5()
          .px_3()
          .py_2()
          .border_b_1()
          .border_color(cx.theme().border)
          .child(
            h_flex()
              .items_center()
              .gap_2()
              .font_family(crate::theme::mono(cx))
              .text_xs()
              .child(div().size_1p5().flex_shrink_0().rounded_full().bg(dot))
              .child(div().font_medium().child(entry.tool.clone()))
              .child(
                div()
                  .flex_1()
                  .min_w_0()
                  .truncate()
                  .text_color(cx.theme().muted_foreground)
                  .child(self.target(entry)),
              )
              .child(
                div()
                  .flex_shrink_0()
                  .text_color(cx.theme().muted_foreground)
                  .child(clock(entry.ts)),
              ),
          )
          .when_some(entry.detail.clone(), |this, detail| {
            this.child(
              div()
                .font_family(crate::theme::mono(cx))
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .truncate()
                .child(detail),
            )
          })
          .when_some(entry.error.clone(), |this, error| {
            this.child(
              div()
                .font_family(crate::theme::mono(cx))
                .text_xs()
                .text_color(cx.theme().danger)
                .child(error),
            )
          })
          .into_any_element()
      })
      .collect();

    v_flex().w_full().child(
      v_flex()
        .id("mcp-audit-list")
        .max_h(px(420.))
        .overflow_y_scroll()
        .rounded(cx.theme().radius)
        .border_1()
        .border_color(cx.theme().border)
        .children(rows),
    )
  }
}

#[cfg(test)]
mod tests {
  // The parent globs gpui: shadow `test` back or #[gpui::test] recurses.
  use ::core::prelude::v1::test;
  use gpui::{Modifiers, TestAppContext};
  use gpui_component::WindowExt;

  use super::*;
  use crate::mcp_approval::open_mcp_approval_dialog;
  use crate::test_support;

  fn approval_request(id: &str) -> McpApprovalRequest {
    McpApprovalRequest {
      id: id.to_string(),
      connection_id: "c1".to_string(),
      connection_name: "warehouse".to_string(),
      operation: format!("DELETE FROM t WHERE id = {id}"),
      payload: None,
    }
  }

  fn coordinator_state() -> (tempfile::TempDir, Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let state = Arc::new(AppState::for_tests(
      dir.path(),
      Box::new(soquel_core::secrets::InMemoryStore::default()),
    ));
    (dir, state)
  }

  // Iterations shuffle the task order per seed: the queue must drain in
  // request order whatever the scheduler does.
  #[gpui::test(iterations = 10)]
  fn approvals_show_one_dialog_at_a_time_and_surface_the_next(cx: &mut TestAppContext) {
    let (_dir, state) = coordinator_state();
    let coordinator = cx.update(|cx| init_coordinator(state.clone(), cx));
    let (_app, cx) = test_support::shell_window(cx, {
      let state = state.clone();
      move |window, cx| crate::app::App::new(state, window, cx)
    });

    // Two writes block at the same time.
    cx.update(|_, cx| {
      coordinator.update(cx, |coordinator, cx| {
        coordinator.enqueue_approval(approval_request("1"), cx);
        coordinator.enqueue_approval(approval_request("2"), cx);
      });
    });
    test_support::wait_until(cx, "the first approval dialog", |cx| {
      cx.update(|window, cx| window.has_active_dialog(cx))
    });
    // Both are queued, but only one dialog is up.
    cx.update(|_, cx| assert_eq!(coordinator.read(cx).approval_queue.len(), 2));

    // Answering the front drains it and the second surfaces on its own.
    let bounds = cx
      .debug_bounds("approval-allow")
      .expect("run button painted");
    cx.simulate_click(bounds.center(), Modifiers::none());
    test_support::wait_until(cx, "the second approval dialog", |cx| {
      cx.update(|window, cx| window.has_active_dialog(cx))
        && cx.update(|_, cx| coordinator.read(cx).approval_queue.len() == 1)
    });

    // Answering the second empties the queue and closes the dialog.
    let bounds = cx
      .debug_bounds("approval-allow")
      .expect("run button painted");
    cx.simulate_click(bounds.center(), Modifiers::none());
    cx.run_until_parked();
    cx.update(|_, cx| assert!(coordinator.read(cx).approval_queue.is_empty()));
    assert!(!cx.update(|window, cx| window.has_active_dialog(cx)));
  }

  #[test]
  fn parse_port_refuses_out_of_range_values() {
    assert_eq!(parse_port("5432"), Ok(5432));
    assert_eq!(parse_port("  8080 "), Ok(8080));
    assert_eq!(
      parse_port("nope").unwrap_err(),
      "Port must be a whole number"
    );
    assert_eq!(
      parse_port("80.5").unwrap_err(),
      "Port must be a whole number"
    );
    assert_eq!(parse_port("").unwrap_err(), "Port must be a whole number");
    assert_eq!(parse_port("80").unwrap_err(), "Port must be 1024 or above");
    assert_eq!(
      parse_port("1023").unwrap_err(),
      "Port must be 1024 or above"
    );
    assert_eq!(parse_port("70000").unwrap_err(), "Port must be below 65536");
  }

  #[test]
  fn remaining_label_counts_whole_seconds_down_to_zero() {
    assert_eq!(remaining_label(0.0), "0:00");
    assert_eq!(remaining_label(59_000.0), "0:59");
    assert_eq!(remaining_label(60_000.0), "1:00");
    assert_eq!(remaining_label(90_400.0), "1:31");
    assert_eq!(remaining_label(-5_000.0), "0:00");
  }

  #[test]
  fn exposed_summary_reads_by_count_and_reach() {
    assert_eq!(
      exposed_summary(0, 0),
      "No connections are exposed yet. Opt one in with the Agent access field in its settings."
    );
    assert_eq!(
      exposed_summary(1, 0),
      "1 connection exposed, read-only. Every agent call lands in the audit log."
    );
    assert_eq!(
      exposed_summary(3, 2),
      "3 connections exposed, 2 of them writable with your approval. Every agent call lands in \
       the audit log."
    );
  }

  #[test]
  fn setup_command_masks_and_shapes_the_line() {
    let command = setup_command("soquel", "http://127.0.0.1:52700/mcp", "••••••••");
    assert_eq!(
      command,
      "claude mcp add --transport http soquel http://127.0.0.1:52700/mcp --header \
       \"Authorization: Bearer ••••••••\""
    );
  }

  #[test]
  fn agent_access_labels_cover_every_variant() {
    assert_eq!(agent_access_label(AgentAccess::None), "Off");
    assert_eq!(agent_access_label(AgentAccess::ReadOnly), "Read-only");
    assert_eq!(
      agent_access_label(AgentAccess::WriteWithApproval),
      "Writes need approval"
    );
  }

  struct Probe {
    answer: Option<ApprovalAnswer>,
  }

  impl Render for Probe {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
      gpui::div()
    }
  }

  fn sample_request() -> McpApprovalRequest {
    McpApprovalRequest {
      id: "req-1".to_string(),
      connection_id: "c1".to_string(),
      connection_name: "warehouse".to_string(),
      operation: "DELETE FROM users WHERE id = 4".to_string(),
      payload: None,
    }
  }

  fn open_probe_dialog(cx: &mut TestAppContext) -> (Entity<Probe>, &mut VisualTestContext) {
    let (probe, cx) = test_support::shell_window(cx, |_, _| Probe { answer: None });
    cx.update(|_, cx| {
      probe.update(cx, |_, cx| {
        open_mcp_approval_dialog(
          cx.entity(),
          sample_request(),
          0,
          cx,
          |probe: &mut Probe, answer, _| probe.answer = Some(answer),
        );
      });
    });
    test_support::wait_until(cx, "the approval dialog", |cx| {
      cx.update(|window, cx| window.has_active_dialog(cx))
    });
    (probe, cx)
  }

  #[gpui::test]
  fn running_the_write_answers_once_and_enter_does_nothing(cx: &mut TestAppContext) {
    let (probe, cx) = open_probe_dialog(cx);

    // Enter allows nothing: the write waits for an explicit button.
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert!(cx.update(|window, cx| window.has_active_dialog(cx)));
    cx.update(|_, cx| assert!(probe.read(cx).answer.is_none()));

    let bounds = cx
      .debug_bounds("approval-allow")
      .expect("run button painted");
    cx.simulate_click(bounds.center(), Modifiers::none());
    cx.run_until_parked();

    assert!(!cx.update(|window, cx| window.has_active_dialog(cx)));
    cx.update(|_, cx| assert!(matches!(probe.read(cx).answer, Some(ApprovalAnswer::Once))));
  }

  #[gpui::test]
  fn allowing_for_the_window_answers_for_window(cx: &mut TestAppContext) {
    let (probe, cx) = open_probe_dialog(cx);
    let bounds = cx
      .debug_bounds("approval-allow-window")
      .expect("window button painted");
    cx.simulate_click(bounds.center(), Modifiers::none());
    cx.run_until_parked();
    cx.update(|_, cx| {
      assert!(matches!(
        probe.read(cx).answer,
        Some(ApprovalAnswer::ForWindow)
      ))
    });
  }

  #[gpui::test]
  fn denying_answers_deny(cx: &mut TestAppContext) {
    let (probe, cx) = open_probe_dialog(cx);
    let bounds = cx
      .debug_bounds("approval-deny")
      .expect("deny button painted");
    cx.simulate_click(bounds.center(), Modifiers::none());
    cx.run_until_parked();
    cx.update(|_, cx| assert!(matches!(probe.read(cx).answer, Some(ApprovalAnswer::Deny))));
  }

  #[gpui::test]
  fn dismissing_the_dialog_denies(cx: &mut TestAppContext) {
    let (probe, cx) = open_probe_dialog(cx);
    // Silence must never mean yes: escape is a refusal.
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(!cx.update(|window, cx| window.has_active_dialog(cx)));
    cx.update(|_, cx| assert!(matches!(probe.read(cx).answer, Some(ApprovalAnswer::Deny))));
  }

  #[gpui::test]
  fn a_bad_port_is_refused_with_a_message(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let cx = cx.add_empty_window();
    let dir = tempfile::tempdir().unwrap();
    let state = Arc::new(soquel_core::AppState::for_tests(
      dir.path(),
      Box::new(soquel_core::secrets::InMemoryStore::default()),
    ));
    let (tx, _rx) = approval_channel();
    let make_approver = approver_factory(tx);
    let panel =
      cx.update(|window, cx| cx.new(|cx| McpPanel::new(state, make_approver, window, cx)));
    cx.update(|window, cx| {
      panel.update(cx, |panel, cx| {
        panel
          .port_input
          .update(cx, |i, cx| i.set_value("nope", window, cx));
        panel.apply_port(cx);
        assert_eq!(
          panel.problem.as_deref(),
          Some("Port must be a whole number")
        );
      });
    });
  }

  #[tokio::test]
  async fn the_approver_relays_the_answer_through_the_channel() {
    use futures::StreamExt;

    let dir = tempfile::tempdir().unwrap();
    let state = Arc::new(soquel_core::AppState::for_tests(
      dir.path(),
      Box::new(soquel_core::secrets::InMemoryStore::default()),
    ));
    let (tx, mut rx) = approval_channel();
    let approver = GpuiApprover { tx };
    let request = sample_request();
    let id = request.id.clone();

    let blocked = state.clone();
    let waiting = tokio::spawn(async move { approver.request(&blocked, request).await });

    // The blocked write surfaces on the channel the App drains...
    let received = rx.next().await.expect("the request reaches the channel");
    assert_eq!(received.id, id);

    // ...and the human's yes fires the oneshot the server thread waits on.
    soquel_core::mcp::resolve_approval(&state, &id, ApprovalAnswer::Once)
      .await
      .unwrap();
    assert!(matches!(waiting.await.unwrap(), ApprovalAnswer::Once));
  }
}
