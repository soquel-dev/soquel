use std::rc::Rc;
use std::sync::Arc;

use gpui::*;
use gpui_component::{Icon, IconName, Root, TitleBar, h_flex, v_flex};
use soquel_core::AppState;

use crate::actions::ToggleCommandPalette;
use crate::command_palette::{PaletteItem, PaletteSection};
use crate::connections::{ConnectionsEvent, ConnectionsView, group_connections};
use crate::core;
use crate::icons::SoquelIcon;
use crate::theme;

/// The hub window root: the persistent connections list. Opening a connection
/// spawns its own window (`crate::windows`).
pub struct App {
  state: Arc<AppState>,
  connections: Entity<ConnectionsView>,
  /// The key home: the global cmd-k needs a focus target, and the Connections
  /// screen does not focus itself.
  focus_handle: FocusHandle,
  _connections_subscription: Subscription,
}

impl App {
  pub fn new(state: Arc<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
    let connections = cx.new(|cx| ConnectionsView::new(state.clone(), window, cx));
    let subscription = cx.subscribe_in(&connections, window, Self::on_connections_event);
    let focus_handle = cx.focus_handle();
    focus_handle.focus(window, cx);

    Self {
      state,
      connections,
      focus_handle,
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
        crate::windows::open_connection_window(db.clone(), profile.clone(), cx);
      }
      ConnectionsEvent::OpenMcpPanel => {
        crate::chrome::open_mcp_panel(this.state.clone(), window, cx)
      }
    }
  }

  fn palette_items(&self, cx: &mut Context<Self>) -> Vec<PaletteItem> {
    let mut items = crate::chrome::app_palette_items(self.state.clone(), cx);

    // Items live in the palette dialog: weak handles so an open palette never
    // keeps a closed hub alive.
    let view = &self.connections;
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
    items
  }

  fn open_command_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
    let items = self.palette_items(cx);
    crate::chrome::open_command_palette(items, window, cx);
  }

  #[cfg(test)]
  pub(crate) fn connections_view(&self) -> Entity<ConnectionsView> {
    self.connections.clone()
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
      .child(TitleBar::new().child(h_flex().child("soquel")))
      .child(div().flex_1().min_h_0().child(self.connections.clone()))
      .child(crate::chrome::footer(
        self.state.clone(),
        None,
        SharedString::default(),
        window,
        cx,
      ))
      .children(dialog_layer)
      .children(notification_layer)
  }
}

#[cfg(test)]
mod tests {
  use ::core::prelude::v1::test;
  use gpui::TestAppContext;
  use gpui_component::{ActiveTheme, WindowExt};

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
