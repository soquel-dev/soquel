use std::sync::Arc;

use gpui::*;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::{ActiveTheme, Icon, IconName, Root, Sizable, TitleBar, h_flex, v_flex};
use soquel_core::AppState;

use crate::connections::{ConnectionsEvent, ConnectionsView};
use crate::core;
use crate::theme;
use crate::workspace::{Workspace, WorkspaceEvent};

enum Screen {
  Connections(Entity<ConnectionsView>),
  Workspace {
    view: Entity<Workspace>,
    connection_id: String,
    _subscription: Subscription,
  },
}

/// The window root: connections list first, the workspace once connected.
pub struct App {
  state: Arc<AppState>,
  screen: Screen,
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
    Self {
      state,
      screen: Screen::Connections(connections),
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
    let view = cx.new(|cx| Workspace::new(db, profile, window, cx));
    let subscription = cx.subscribe_in(
      &view,
      window,
      |this, _, event: &WorkspaceEvent, window, cx| {
        let WorkspaceEvent::Close = event;
        this.close_workspace(window, cx);
      },
    );
    self.screen = Screen::Workspace {
      view,
      connection_id,
      _subscription: subscription,
    };
    cx.notify();
  }

  fn close_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
    if let Screen::Workspace { connection_id, .. } = &self.screen {
      core::disconnect_id(self.state.clone(), connection_id.clone());
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
    cx.notify();
  }
}

impl Render for App {
  fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    // Root does not render these itself: without them dialogs and toasts are silent no-ops.
    let dialog_layer = Root::render_dialog_layer(window, cx);
    let notification_layer = Root::render_notification_layer(window, cx);

    v_flex()
      .size_full()
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
      })
      .children(dialog_layer)
      .children(notification_layer)
  }
}
