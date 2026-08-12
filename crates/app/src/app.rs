use std::rc::Rc;
use std::sync::Arc;

use gpui::*;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::list::{List, ListState};
use gpui_component::{
  ActiveTheme, Icon, IconName, Root, Sizable, TitleBar, WindowExt, h_flex, v_flex,
};
use soquel_core::AppState;

use futures::StreamExt;
use soquel_core::ApprovalAnswer;
use soquel_core::mcp::McpApprovalRequest;

use crate::actions::ToggleCommandPalette;
use crate::command_palette::{CommandPaletteDelegate, PaletteItem, PaletteSection, palette_footer};
use crate::connections::{ConnectionsEvent, ConnectionsView, group_connections};
use crate::core;
use crate::dialogs;
use crate::doc::{DocWorkspace, DocWorkspaceEvent};
use crate::icons::SoquelIcon;
use crate::kv::{KvWorkspace, KvWorkspaceEvent};
use crate::mcp::{McpAuditView, McpPanel};
use crate::mcp_approval::open_mcp_approval_dialog;
use crate::theme;
use crate::workspace::{Workspace, WorkspaceEvent};

enum Screen {
  Connections(Entity<ConnectionsView>),
  Workspace {
    view: Entity<Workspace>,
    connection_id: String,
    _subscription: Subscription,
  },
  KvWorkspace {
    view: Entity<KvWorkspace>,
    connection_id: String,
    _subscription: Subscription,
  },
  DocWorkspace {
    view: Entity<DocWorkspace>,
    connection_id: String,
    _subscription: Subscription,
  },
}

/// The window root: connections list first, the workspace once connected.
pub struct App {
  state: Arc<AppState>,
  screen: Screen,
  /// The cross-screen key home: the global cmd-k needs a focus target that
  /// survives the Connections screen, which does not focus itself.
  focus_handle: FocusHandle,
  /// Injected into every MCP server start so a blocked write reaches the
  /// approval dialog through this App's channel.
  make_approver: core::ApproverFactory,
  approval_queue: Vec<McpApprovalRequest>,
  approval_showing: bool,
  _approval_drain: Task<()>,
  _connections_subscription: Subscription,
}

impl App {
  pub fn new(state: Arc<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
    let connections = cx.new(|cx| ConnectionsView::new(state.clone(), window, cx));
    let subscription = cx.subscribe_in(&connections, window, Self::on_connections_event);
    let focus_handle = cx.focus_handle();
    focus_handle.focus(window, cx);

    let (tx, mut rx) = crate::mcp::approval_channel();
    let make_approver = crate::mcp::approver_factory(tx);
    // One drain for the app's life: server restarts reuse the same channel.
    let _approval_drain = cx.spawn(async move |this, cx| {
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
      screen: Screen::Connections(connections),
      focus_handle,
      make_approver,
      approval_queue: Vec::new(),
      approval_showing: false,
      _approval_drain,
      _connections_subscription: subscription,
    }
  }

  fn on_connections_event(
    this: &mut Self,
    _: &Entity<ConnectionsView>,
    event: &ConnectionsEvent,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    match event {
      ConnectionsEvent::Connected { db, profile } => {
        this.open_workspace(db.clone(), profile.clone(), window, cx);
      }
      ConnectionsEvent::OpenMcpPanel => this.open_mcp_panel(cx),
    }
  }

  fn enqueue_approval(&mut self, request: McpApprovalRequest, cx: &mut Context<Self>) {
    self.approval_queue.push(request);
    self.show_next_approval(cx);
  }

  /// The open dialog is always for the front of the queue; new requests wait
  /// behind it and surface as it drains.
  fn show_next_approval(&mut self, cx: &mut Context<Self>) {
    if self.approval_showing || self.approval_queue.is_empty() {
      return;
    }
    self.approval_showing = true;
    let request = self.approval_queue[0].clone();
    let more = self.approval_queue.len() - 1;
    open_mcp_approval_dialog(cx.entity(), request, more, cx, |app, answer, cx| {
      app.resolve_front(answer, cx)
    });
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

  fn open_mcp_panel(&mut self, cx: &mut Context<Self>) {
    let state = self.state.clone();
    let make_approver = self.make_approver.clone();
    dialogs::defer_on_active_window(cx, move |window, cx| {
      let panel = cx.new(|cx| McpPanel::new(state, make_approver, window, cx));
      window.open_dialog(cx, move |dialog, window, cx| {
        dialogs::styled(dialog, window, cx)
          .w(px(560.))
          .child(panel.clone())
      });
    });
  }

  fn open_diagnostics(&mut self, cx: &mut Context<Self>) {
    let state = self.state.clone();
    dialogs::defer_on_active_window(cx, move |window, cx| {
      let view = cx.new(|cx| crate::diagnostics::DiagnosticsView::new(state, cx));
      window.open_dialog(cx, move |dialog, window, cx| {
        dialogs::styled(dialog, window, cx)
          .title(
            div()
              .font_family(crate::theme::mono(cx))
              .child("Diagnostics and logs"),
          )
          .w(px(520.))
          .child(view.clone())
      });
    });
  }

  fn open_licence(&mut self, cx: &mut Context<Self>) {
    let state = self.state.clone();
    dialogs::defer_on_active_window(cx, move |window, cx| {
      let view = cx.new(|cx| crate::licence::LicenceView::new(state, window, cx));
      window.open_dialog(cx, move |dialog, window, cx| {
        dialogs::styled(dialog, window, cx)
          .title(div().font_family(crate::theme::mono(cx)).child("Licence"))
          .w(px(440.))
          .child(view.clone())
      });
    });
  }

  fn open_audit(&mut self, cx: &mut Context<Self>) {
    let state = self.state.clone();
    dialogs::defer_on_active_window(cx, move |window, cx| {
      let audit = cx.new(|cx| McpAuditView::new(state, cx));
      window.open_dialog(cx, move |dialog, window, cx| {
        dialogs::styled(dialog, window, cx)
          .title(
            div()
              .font_family(crate::theme::mono(cx))
              .child("Agent activity"),
          )
          .w(px(640.))
          .child(audit.clone())
      });
    });
  }

  fn open_workspace(
    &mut self,
    db: core::Db,
    profile: soquel_core::profiles::ConnectionProfile,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    let connection_id = profile.id.clone();
    // The browsers branch on kind: keys -> redis, documents -> mongo,
    // everything else the SQL workspace.
    let caps = soquel_core::connectors::connector_for(db.kind()).capabilities();
    self.screen = if caps.contains(&soquel_core::connectors::Capability::KvBrowse) {
      let view = cx.new(|cx| KvWorkspace::new(self.state.clone(), db, profile, window, cx));
      let subscription = cx.subscribe_in(
        &view,
        window,
        |this, _, event: &KvWorkspaceEvent, window, cx| {
          let KvWorkspaceEvent::Close = event;
          this.close_workspace(window, cx);
        },
      );
      Screen::KvWorkspace {
        view,
        connection_id,
        _subscription: subscription,
      }
    } else if caps.contains(&soquel_core::connectors::Capability::DocBrowse) {
      let view = cx.new(|cx| DocWorkspace::new(self.state.clone(), db, profile, window, cx));
      let subscription = cx.subscribe_in(
        &view,
        window,
        |this, _, event: &DocWorkspaceEvent, window, cx| {
          let DocWorkspaceEvent::Close = event;
          this.close_workspace(window, cx);
        },
      );
      Screen::DocWorkspace {
        view,
        connection_id,
        _subscription: subscription,
      }
    } else {
      let data_dir = self.state.data_dir.clone();
      let view = cx.new(|cx| Workspace::new(db, profile, data_dir, window, cx));
      let subscription = cx.subscribe_in(
        &view,
        window,
        |this, _, event: &WorkspaceEvent, window, cx| {
          let WorkspaceEvent::Close = event;
          this.close_workspace(window, cx);
        },
      );
      Screen::Workspace {
        view,
        connection_id,
        _subscription: subscription,
      }
    };
    cx.notify();
  }

  fn close_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
    if let Screen::Workspace { view, .. } = &self.screen {
      view.update(cx, |workspace, _| workspace.close_sessions());
    }
    let open_id = match &self.screen {
      Screen::Workspace { connection_id, .. }
      | Screen::KvWorkspace { connection_id, .. }
      | Screen::DocWorkspace { connection_id, .. } => Some(connection_id.clone()),
      Screen::Connections(_) => None,
    };
    if let Some(id) = open_id {
      core::disconnect_id(self.state.clone(), id);
    }
    let connections = cx.new(|cx| ConnectionsView::new(self.state.clone(), window, cx));
    self._connections_subscription =
      cx.subscribe_in(&connections, window, Self::on_connections_event);
    self.screen = Screen::Connections(connections);
    // The connections screen has no focus of its own; keep the key home here.
    self.focus_handle.focus(window, cx);
    cx.notify();
  }

  /// The entries the palette lists depend on which screen is up: connections
  /// and their quick-connect, or the workspace's query/tab operations.
  fn palette_items(&self, cx: &mut Context<Self>) -> Vec<PaletteItem> {
    let mut items = Vec::new();
    let dark = cx.theme().mode.is_dark();
    items.push(PaletteItem {
      label: if dark {
        "Switch to light theme".into()
      } else {
        "Switch to dark theme".into()
      },
      hint: None,
      keywords: "toggle theme dark light".to_string(),
      icon: Icon::new(if dark { IconName::Sun } else { IconName::Moon }),
      section: PaletteSection::App,
      run: Rc::new(theme::toggle),
    });
    // Items live in the palette dialog: weak handles so an open palette never
    // keeps a swapped-out screen alive.
    let panel_app = cx.entity().downgrade();
    items.push(PaletteItem {
      label: "MCP server".into(),
      hint: None,
      keywords: "mcp server agent access port token".to_string(),
      icon: Icon::new(IconName::Bot),
      section: PaletteSection::App,
      run: Rc::new(move |_, cx| {
        panel_app.update(cx, |app, cx| app.open_mcp_panel(cx)).ok();
      }),
    });
    let audit_app = cx.entity().downgrade();
    items.push(PaletteItem {
      label: "Agent activity".into(),
      hint: None,
      keywords: "agent activity audit log mcp".to_string(),
      icon: Icon::new(IconName::BookOpen),
      section: PaletteSection::App,
      run: Rc::new(move |_, cx| {
        audit_app.update(cx, |app, cx| app.open_audit(cx)).ok();
      }),
    });
    let licence_app = cx.entity().downgrade();
    items.push(PaletteItem {
      label: "Licence".into(),
      hint: None,
      keywords: "licence license unlock buy activate tabs".to_string(),
      icon: Icon::new(SoquelIcon::Lock),
      section: PaletteSection::App,
      run: Rc::new(move |_, cx| {
        licence_app.update(cx, |app, cx| app.open_licence(cx)).ok();
      }),
    });
    let diagnostics_app = cx.entity().downgrade();
    items.push(PaletteItem {
      label: "Diagnostics and logs".into(),
      hint: None,
      keywords: "diagnostics logs support bug report".to_string(),
      icon: Icon::new(IconName::Info),
      section: PaletteSection::App,
      run: Rc::new(move |_, cx| {
        diagnostics_app
          .update(cx, |app, cx| app.open_diagnostics(cx))
          .ok();
      }),
    });

    match &self.screen {
      Screen::Connections(view) => {
        let profiles = core::list_connections(&self.state);
        let has_connections = !profiles.is_empty();
        for (group, profiles) in group_connections(&profiles) {
          for profile in profiles {
            let id = profile.id.clone();
            let view = view.downgrade();
            let target = soquel_core::transfer::target(&profile.params);
            items.push(PaletteItem {
              label: profile.name.clone().into(),
              hint: Some(target.clone().into()),
              keywords: format!(
                "connect {} {} {}",
                group.clone().unwrap_or_default(),
                profile.name,
                target
              )
              .to_lowercase(),
              icon: Icon::new(SoquelIcon::Database),
              section: PaletteSection::Connections,
              run: Rc::new(move |_, cx| {
                let id = id.clone();
                view.update(cx, |view, cx| view.connect(id, cx)).ok();
              }),
            });
          }
        }
        let new_view = view.downgrade();
        items.push(PaletteItem {
          label: "New connection".into(),
          hint: None,
          keywords: "new connection".to_string(),
          icon: Icon::new(IconName::Plus),
          section: PaletteSection::Actions,
          run: Rc::new(move |_, cx| {
            new_view
              .update(cx, |view, cx| view.open_form(None, cx))
              .ok();
          }),
        });
        let import_view = view.downgrade();
        items.push(PaletteItem {
          label: "Import connections…".into(),
          hint: None,
          keywords: "import connections file".to_string(),
          icon: Icon::new(IconName::FolderOpen),
          section: PaletteSection::Actions,
          run: Rc::new(move |_, cx| {
            import_view
              .update(cx, |view, cx| view.import_via_picker(cx))
              .ok();
          }),
        });
        if has_connections {
          let export_view = view.downgrade();
          items.push(PaletteItem {
            label: "Export connections…".into(),
            hint: None,
            keywords: "export connections file".to_string(),
            icon: Icon::new(IconName::ExternalLink),
            section: PaletteSection::Actions,
            run: Rc::new(move |_, cx| {
              export_view
                .update(cx, |view, cx| view.open_export_dialog(cx))
                .ok();
            }),
          });
        }
      }
      Screen::Workspace { view, .. } => {
        let run_view = view.downgrade();
        items.push(PaletteItem {
          label: "Run query".into(),
          hint: None,
          keywords: "run query execute".to_string(),
          icon: Icon::new(IconName::Play),
          section: PaletteSection::Actions,
          run: Rc::new(move |_, cx| {
            run_view.update(cx, |view, cx| view.run(cx)).ok();
          }),
        });
        let sql_view = view.downgrade();
        items.push(PaletteItem {
          label: "New SQL tab".into(),
          hint: None,
          keywords: "new sql tab".to_string(),
          icon: Icon::new(IconName::Plus),
          section: PaletteSection::Actions,
          run: Rc::new(move |window, cx| {
            sql_view
              .update(cx, |view, cx| view.open_sql(window, cx))
              .ok();
          }),
        });
        let refresh_view = view.downgrade();
        items.push(PaletteItem {
          label: "Refresh schema".into(),
          hint: None,
          keywords: "refresh schema reload".to_string(),
          icon: Icon::new(IconName::Replace),
          section: PaletteSection::Actions,
          run: Rc::new(move |_, cx| {
            refresh_view
              .update(cx, |view, cx| view.refresh_schema(cx))
              .ok();
          }),
        });
        let focus_view = view.downgrade();
        items.push(PaletteItem {
          label: "Focus editor".into(),
          hint: None,
          keywords: "focus editor sql".to_string(),
          icon: Icon::new(IconName::SquareTerminal),
          section: PaletteSection::Actions,
          run: Rc::new(move |window, cx| {
            focus_view
              .update(cx, |view, cx| view.focus_editor(window, cx))
              .ok();
          }),
        });
        let next_view = view.downgrade();
        items.push(PaletteItem {
          label: "Next tab".into(),
          hint: None,
          keywords: "next tab".to_string(),
          icon: Icon::new(IconName::ArrowRight),
          section: PaletteSection::Actions,
          run: Rc::new(move |_, cx| {
            next_view.update(cx, |view, cx| view.cycle(1, cx)).ok();
          }),
        });
        let prev_view = view.downgrade();
        items.push(PaletteItem {
          label: "Previous tab".into(),
          hint: None,
          keywords: "previous tab".to_string(),
          icon: Icon::new(IconName::ArrowLeft),
          section: PaletteSection::Actions,
          run: Rc::new(move |_, cx| {
            prev_view.update(cx, |view, cx| view.cycle(-1, cx)).ok();
          }),
        });
        let app = cx.entity().downgrade();
        items.push(PaletteItem {
          label: "Back to connections".into(),
          hint: None,
          keywords: "back connections close disconnect".to_string(),
          icon: Icon::new(IconName::ArrowLeft),
          section: PaletteSection::Actions,
          run: Rc::new(move |window, cx| {
            app
              .update(cx, |app, cx| app.close_workspace(window, cx))
              .ok();
          }),
        });
      }
      Screen::KvWorkspace { .. } | Screen::DocWorkspace { .. } => {
        let app = cx.entity().downgrade();
        items.push(PaletteItem {
          label: "Back to connections".into(),
          hint: None,
          keywords: "back connections close disconnect".to_string(),
          icon: Icon::new(IconName::ArrowLeft),
          section: PaletteSection::Actions,
          run: Rc::new(move |window, cx| {
            app
              .update(cx, |app, cx| app.close_workspace(window, cx))
              .ok();
          }),
        });
      }
    }
    items
  }

  fn open_command_palette(&mut self, _: &mut Window, cx: &mut Context<Self>) {
    let this = cx.entity().downgrade();
    dialogs::defer_on_active_window(cx, move |window, cx| {
      let Ok(items) = this.update(cx, |this, cx| this.palette_items(cx)) else {
        return;
      };
      let state = cx
        .new(|cx| ListState::new(CommandPaletteDelegate::new(items), window, cx).searchable(true));
      // No buttons in the footer: the List owns enter/escape under its own key context.
      let list = state.clone();
      window.open_dialog(cx, move |dialog, window, cx| {
        dialogs::styled(dialog, window, cx)
          .w(px(620.))
          .p_0()
          .gap_0()
          .close_button(false)
          .child(
            List::new(&list)
              .with_size(gpui_component::Size::Large)
              .search_placeholder("Search connections and actions…")
              .max_h(px(400.)),
          )
          .footer(palette_footer(cx))
      });
      // After the dialog took focus: hand it to the query input so typing
      // filters and Enter reaches the List's own confirm, not the dialog's.
      state.update(cx, |state, cx| state.focus(window, cx));
    });
  }
}

impl Render for App {
  fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    // Root does not render these itself: without them dialogs and toasts are silent no-ops.
    let dialog_layer = Root::render_dialog_layer(window, cx);
    let notification_layer = Root::render_notification_layer(window, cx);

    v_flex()
      .size_full()
      .key_context("App")
      .track_focus(&self.focus_handle)
      .on_action(cx.listener(|this, _: &ToggleCommandPalette, window, cx| {
        this.open_command_palette(window, cx)
      }))
      .bg(theme::canvas(cx))
      .child(
        TitleBar::new().child(h_flex().child("soquel")).child(
          h_flex().flex_1().justify_end().pr_2().child(
            Button::new("toggle-theme")
              .ghost()
              .xsmall()
              .icon(Icon::new(if cx.theme().mode.is_dark() {
                IconName::Sun
              } else {
                IconName::Moon
              }))
              .on_click(cx.listener(|_, _, window, cx| theme::toggle(window, cx))),
          ),
        ),
      )
      .child(match &self.screen {
        Screen::Connections(view) => view.clone().into_any_element(),
        Screen::Workspace { view, .. } => view.clone().into_any_element(),
        Screen::KvWorkspace { view, .. } => view.clone().into_any_element(),
        Screen::DocWorkspace { view, .. } => view.clone().into_any_element(),
      })
      .children(dialog_layer)
      .children(notification_layer)
  }
}

#[cfg(test)]
mod tests {
  use ::core::prelude::v1::test;
  use gpui::{Modifiers, TestAppContext};
  use gpui_component::WindowExt;

  use super::*;

  fn approval_request(id: &str) -> McpApprovalRequest {
    McpApprovalRequest {
      id: id.to_string(),
      connection_id: "c1".to_string(),
      connection_name: "warehouse".to_string(),
      operation: format!("DELETE FROM t WHERE id = {id}"),
      payload: None,
    }
  }

  fn test_state() -> (tempfile::TempDir, Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let state = Arc::new(soquel_core::AppState::for_tests(
      dir.path(),
      Box::new(soquel_core::secrets::InMemoryStore::default()),
    ));
    (dir, state)
  }

  // Iterations shuffle the task order per seed: the queue must drain in
  // request order whatever the scheduler does.
  #[gpui::test(iterations = 10)]
  fn approvals_show_one_dialog_at_a_time_and_surface_the_next(cx: &mut TestAppContext) {
    let (_dir, state) = test_state();
    let (app, cx) = crate::test_support::shell_window(cx, {
      let state = state.clone();
      move |window, cx| App::new(state, window, cx)
    });

    // Two writes block at the same time.
    cx.update(|_, cx| {
      app.update(cx, |app, cx| {
        app.enqueue_approval(approval_request("1"), cx);
        app.enqueue_approval(approval_request("2"), cx);
      });
    });
    crate::test_support::wait_until(cx, "the first approval dialog", |cx| {
      cx.update(|window, cx| window.has_active_dialog(cx))
    });
    // Both are queued, but only one dialog is up.
    cx.update(|_, cx| assert_eq!(app.read(cx).approval_queue.len(), 2));

    // Answering the front drains it and the second surfaces on its own.
    let bounds = cx
      .debug_bounds("approval-allow")
      .expect("run button painted");
    cx.simulate_click(bounds.center(), Modifiers::none());
    crate::test_support::wait_until(cx, "the second approval dialog", |cx| {
      cx.update(|window, cx| window.has_active_dialog(cx))
        && cx.update(|_, cx| app.read(cx).approval_queue.len() == 1)
    });

    // Answering the second empties the queue and closes the dialog.
    let bounds = cx
      .debug_bounds("approval-allow")
      .expect("run button painted");
    cx.simulate_click(bounds.center(), Modifiers::none());
    cx.run_until_parked();
    cx.update(|_, cx| assert!(app.read(cx).approval_queue.is_empty()));
    assert!(!cx.update(|window, cx| window.has_active_dialog(cx)));
  }

  fn seed(state: &AppState, name: &str) -> String {
    soquel_core::ops::create_connection(
      state,
      &soquel_core::profiles::ConnectionInput {
        name: name.to_string(),
        env: soquel_core::profiles::Env::Dev,
        group: None,
        agent_access: soquel_core::profiles::AgentAccess::None,
        credential: soquel_core::profiles::CredentialSource::Keychain,
        params: soquel_core::profiles::ConnectorParams::Postgres(
          soquel_core::profiles::SqlServerParams {
            host: "db.internal".to_string(),
            port: 5432,
            database: "app".to_string(),
            user: "u".to_string(),
            ssl_mode: soquel_core::profiles::SslMode::Prefer,
            ssl_root_cert: None,
            tunnel_id: None,
          },
        ),
        password: None,
      },
    )
    .unwrap()
    .id
  }

  #[gpui::test]
  fn the_palette_lists_quick_connect_and_actions_on_the_connections_screen(
    cx: &mut TestAppContext,
  ) {
    let (_dir, state) = test_state();
    seed(&state, "warehouse");
    let (app, cx) = crate::test_support::shell_window(cx, {
      let state = state.clone();
      move |window, cx| App::new(state, window, cx)
    });
    let labels: Vec<String> = cx.update(|_, cx| {
      app
        .update(cx, |app, cx| app.palette_items(cx))
        .iter()
        .map(|item| item.label.to_string())
        .collect()
    });
    assert!(
      labels
        .iter()
        .any(|l| l == "Switch to dark theme" || l == "Switch to light theme")
    );
    assert!(
      labels.iter().any(|l| l == "warehouse"),
      "quick-connect entry"
    );
    assert!(labels.iter().any(|l| l == "New connection"));
    assert!(labels.iter().any(|l| l == "Export connections…"));
  }

  #[gpui::test]
  fn dispatching_the_toggle_opens_the_palette_globally(cx: &mut TestAppContext) {
    let (_dir, state) = test_state();
    let (_app, cx) = crate::test_support::shell_window(cx, {
      let state = state.clone();
      move |window, cx| App::new(state, window, cx)
    });
    // App holds focus on the connections screen, so the global action reaches it.
    cx.update(|window, cx| window.dispatch_action(Box::new(ToggleCommandPalette), cx));
    crate::test_support::wait_until(cx, "the palette dialog", |cx| {
      cx.update(|window, cx| window.has_active_dialog(cx))
    });
  }

  #[gpui::test]
  fn filtering_then_enter_runs_the_entry(cx: &mut TestAppContext) {
    let (_dir, state) = test_state();
    let (app, cx) = crate::test_support::shell_window(cx, {
      let state = state.clone();
      move |window, cx| App::new(state, window, cx)
    });
    let dark_before = cx.update(|_, cx| cx.theme().mode.is_dark());

    cx.update(|window, cx| app.update(cx, |app, cx| app.open_command_palette(window, cx)));
    crate::test_support::wait_until(cx, "the palette dialog", |cx| {
      cx.update(|window, cx| window.has_active_dialog(cx))
    });

    // The query input is focused: typing filters to the theme toggle.
    cx.simulate_input("theme");
    cx.run_until_parked();
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();

    assert!(!cx.update(|window, cx| window.has_active_dialog(cx)));
    assert_ne!(
      cx.update(|_, cx| cx.theme().mode.is_dark()),
      dark_before,
      "the theme entry ran"
    );
  }
}
