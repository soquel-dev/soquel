use std::rc::Rc;
use std::sync::Arc;

use gpui::*;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::list::{List, ListState};
use gpui_component::{
  ActiveTheme, Icon, IconName, Root, Sizable, TitleBar, WindowExt, h_flex, v_flex,
};
use soquel_core::AppState;

use crate::actions::ToggleCommandPalette;
use crate::command_palette::{CommandPaletteDelegate, PaletteItem};
use crate::connections::{ConnectionsEvent, ConnectionsView, group_connections};
use crate::core;
use crate::kv::{KvWorkspace, KvWorkspaceEvent};
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
}

/// The window root: connections list first, the workspace once connected.
pub struct App {
  state: Arc<AppState>,
  screen: Screen,
  /// The cross-screen key home: the global cmd-k needs a focus target that
  /// survives the Connections screen, which does not focus itself.
  focus_handle: FocusHandle,
  _connections_subscription: Subscription,
}

impl App {
  pub fn new(state: Arc<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
    let connections = cx.new(|cx| ConnectionsView::new(state.clone(), window, cx));
    let subscription = cx.subscribe_in(
      &connections,
      window,
      |this, _, event: &ConnectionsEvent, window, cx| {
        let ConnectionsEvent::Connected { db, profile } = event;
        this.open_workspace(db.clone(), profile.clone(), window, cx);
      },
    );
    let focus_handle = cx.focus_handle();
    focus_handle.focus(window, cx);
    Self {
      state,
      screen: Screen::Connections(connections),
      focus_handle,
      _connections_subscription: subscription,
    }
  }

  fn open_workspace(
    &mut self,
    db: core::Db,
    profile: soquel_core::profiles::ConnectionProfile,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    let connection_id = profile.id.clone();
    // The first branch on kind: a key-value connection gets the redis browser,
    // everything else the SQL workspace.
    let browses_keys = soquel_core::connectors::connector_for(db.kind())
      .capabilities()
      .contains(&soquel_core::connectors::Capability::KvBrowse);
    self.screen = if browses_keys {
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
    } else {
      let view = cx.new(|cx| Workspace::new(db, profile, window, cx));
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
    let open_id = match &self.screen {
      Screen::Workspace { connection_id, .. } | Screen::KvWorkspace { connection_id, .. } => {
        Some(connection_id.clone())
      }
      Screen::Connections(_) => None,
    };
    if let Some(id) = open_id {
      core::disconnect_id(self.state.clone(), id);
    }
    let connections = cx.new(|cx| ConnectionsView::new(self.state.clone(), window, cx));
    self._connections_subscription = cx.subscribe_in(
      &connections,
      window,
      |this, _, event: &ConnectionsEvent, window, cx| {
        let ConnectionsEvent::Connected { db, profile } = event;
        this.open_workspace(db.clone(), profile.clone(), window, cx);
      },
    );
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
      run: Rc::new(theme::toggle),
    });

    match &self.screen {
      Screen::Connections(view) => {
        let profiles = core::list_connections(&self.state);
        for (group, profiles) in group_connections(&profiles) {
          for profile in profiles {
            let id = profile.id.clone();
            let view = view.clone();
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
              run: Rc::new(move |_, cx| {
                let id = id.clone();
                view.update(cx, |view, cx| view.connect(id, cx));
              }),
            });
          }
        }
        let has_connections = !core::list_connections(&self.state).is_empty();
        let new_view = view.clone();
        items.push(PaletteItem {
          label: "New connection".into(),
          hint: None,
          keywords: "new connection".to_string(),
          run: Rc::new(move |_, cx| new_view.update(cx, |view, cx| view.open_form(None, cx))),
        });
        let import_view = view.clone();
        items.push(PaletteItem {
          label: "Import connections…".into(),
          hint: None,
          keywords: "import connections file".to_string(),
          run: Rc::new(move |_, cx| import_view.update(cx, |view, cx| view.import_via_picker(cx))),
        });
        if has_connections {
          let export_view = view.clone();
          items.push(PaletteItem {
            label: "Export connections…".into(),
            hint: None,
            keywords: "export connections file".to_string(),
            run: Rc::new(move |_, cx| {
              export_view.update(cx, |view, cx| view.open_export_dialog(cx))
            }),
          });
        }
      }
      Screen::Workspace { view, .. } => {
        let run_view = view.clone();
        items.push(PaletteItem {
          label: "Run query".into(),
          hint: None,
          keywords: "run query execute".to_string(),
          run: Rc::new(move |_, cx| run_view.update(cx, |view, cx| view.run(cx))),
        });
        let sql_view = view.clone();
        items.push(PaletteItem {
          label: "New SQL tab".into(),
          hint: None,
          keywords: "new sql tab".to_string(),
          run: Rc::new(move |window, cx| sql_view.update(cx, |view, cx| view.open_sql(window, cx))),
        });
        let refresh_view = view.clone();
        items.push(PaletteItem {
          label: "Refresh schema".into(),
          hint: None,
          keywords: "refresh schema reload".to_string(),
          run: Rc::new(move |_, cx| refresh_view.update(cx, |view, cx| view.refresh_schema(cx))),
        });
        let focus_view = view.clone();
        items.push(PaletteItem {
          label: "Focus editor".into(),
          hint: None,
          keywords: "focus editor sql".to_string(),
          run: Rc::new(move |window, cx| {
            focus_view.update(cx, |view, cx| view.focus_editor(window, cx))
          }),
        });
        let next_view = view.clone();
        items.push(PaletteItem {
          label: "Next tab".into(),
          hint: None,
          keywords: "next tab".to_string(),
          run: Rc::new(move |_, cx| next_view.update(cx, |view, cx| view.cycle(1, cx))),
        });
        let prev_view = view.clone();
        items.push(PaletteItem {
          label: "Previous tab".into(),
          hint: None,
          keywords: "previous tab".to_string(),
          run: Rc::new(move |_, cx| prev_view.update(cx, |view, cx| view.cycle(-1, cx))),
        });
        let app = cx.entity();
        items.push(PaletteItem {
          label: "Back to connections".into(),
          hint: None,
          keywords: "back connections close disconnect".to_string(),
          run: Rc::new(move |window, cx| app.update(cx, |app, cx| app.close_workspace(window, cx))),
        });
      }
      Screen::KvWorkspace { .. } => {
        let app = cx.entity();
        items.push(PaletteItem {
          label: "Back to connections".into(),
          hint: None,
          keywords: "back connections close disconnect".to_string(),
          run: Rc::new(move |window, cx| app.update(cx, |app, cx| app.close_workspace(window, cx))),
        });
      }
    }
    items
  }

  fn open_command_palette(&mut self, _: &mut Window, cx: &mut Context<Self>) {
    let this = cx.entity();
    cx.defer(move |cx| {
      let Some(window_handle) = cx.active_window() else {
        return;
      };
      let _ = cx.update_window(window_handle, |_, window, cx| {
        let items = this.update(cx, |this, cx| this.palette_items(cx));
        let state = cx.new(|cx| {
          ListState::new(CommandPaletteDelegate::new(items), window, cx).searchable(true)
        });
        // No footer: the List owns enter/escape under its own key context.
        let list = state.clone();
        window.open_dialog(cx, move |dialog, _, _| {
          dialog.w(px(560.)).child(
            List::new(&list)
              .search_placeholder("Search connections and actions…")
              .max_h(px(360.)),
          )
        });
        // After the dialog took focus: hand it to the query input so typing
        // filters and Enter reaches the List's own confirm, not the dialog's.
        state.update(cx, |state, cx| state.focus(window, cx));
      });
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
      .bg(cx.theme().background)
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
      })
      .children(dialog_layer)
      .children(notification_layer)
  }
}

#[cfg(test)]
mod tests {
  use ::core::prelude::v1::test;
  use gpui::TestAppContext;
  use gpui_component::WindowExt;

  use super::*;

  fn test_state() -> (tempfile::TempDir, Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let state = Arc::new(soquel_core::AppState::for_tests(
      dir.path(),
      Box::new(soquel_core::secrets::InMemoryStore::default()),
    ));
    (dir, state)
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
