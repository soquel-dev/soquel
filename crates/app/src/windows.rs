//! One hub window plus one window per open connection. The registry maps them;
//! closing a connection window is the disconnect gesture.

use std::sync::Arc;

use gpui::{
  AnyWindowHandle, App, AppContext, Bounds, Global, WindowBounds, WindowOptions, px, size,
};
use gpui_component::{Root, TitleBar};
use soquel_core::AppState;
use soquel_core::profiles::ConnectionProfile;

use crate::connection_window::ConnectionWindow;
use crate::core;

pub struct WindowRegistry {
  state: Arc<AppState>,
  hub: Option<AnyWindowHandle>,
  connections: Vec<(String, AnyWindowHandle)>,
}

impl Global for WindowRegistry {}

impl WindowRegistry {
  pub fn hub_handle(&self) -> Option<AnyWindowHandle> {
    self.hub
  }
}

pub fn init(state: Arc<AppState>, cx: &mut App) {
  cx.set_global(WindowRegistry {
    state,
    hub: None,
    connections: Vec::new(),
  });
  // The window is already gone when this fires: touch only the registry and
  // the core, never the closed window.
  cx.on_window_closed(|cx, window_id| {
    let registry = cx.global_mut::<WindowRegistry>();
    if registry
      .hub
      .is_some_and(|handle| handle.window_id() == window_id)
    {
      registry.hub = None;
      return;
    }
    let Some(index) = registry
      .connections
      .iter()
      .position(|(_, handle)| handle.window_id() == window_id)
    else {
      return;
    };
    let (id, _) = registry.connections.remove(index);
    let state = registry.state.clone();
    core::disconnect_connection(state, id, cx).detach();
  })
  .detach();
}

pub fn open_hub_window(cx: &mut App) -> Option<AnyWindowHandle> {
  let state = cx.global::<WindowRegistry>().state.clone();
  let handle = cx
    .open_window(
      WindowOptions {
        titlebar: Some(TitleBar::title_bar_options()),
        ..Default::default()
      },
      |window, cx| {
        window.activate_window();
        window.set_window_title("soquel");
        let view = cx.new(|cx| crate::app::App::new(state, window, cx));
        cx.new(|cx| Root::new(view, window, cx))
      },
    )
    .ok()?;
  let handle = *handle;
  cx.global_mut::<WindowRegistry>().hub = Some(handle);
  // Approvals parked while no window existed can land now.
  crate::mcp::poke_approvals(cx);
  Some(handle)
}

/// Focus the hub if it is open, else reopen it.
pub fn focus_or_open_hub(cx: &mut App) {
  let hub = cx
    .try_global::<WindowRegistry>()
    .and_then(|registry| registry.hub);
  if let Some(handle) = hub
    && handle
      .update(cx, |_, window, _| window.activate_window())
      .is_ok()
  {
    return;
  }
  // Deferred: reopening from inside another window's dispatch would re-enter.
  cx.defer(|cx| {
    let missing = cx
      .try_global::<WindowRegistry>()
      .is_some_and(|registry| registry.hub.is_none());
    if missing {
      open_hub_window(cx);
    }
  });
}

/// macOS dock reactivation with every window closed brings the hub back.
pub fn reopen_hub_if_none(cx: &mut App) {
  if cx.windows().is_empty() && cx.try_global::<WindowRegistry>().is_some() {
    open_hub_window(cx);
  }
}

pub fn open_connection_window(db: core::Db, profile: ConnectionProfile, cx: &mut App) {
  if focus_connection_window(&profile.id, cx) {
    return;
  }
  // Deferred so a window never opens inside another window's dispatch; the
  // duplicate check reruns since a first open may have landed meanwhile.
  cx.defer(move |cx| {
    if focus_connection_window(&profile.id, cx) {
      return;
    }
    let Some(registry) = cx.try_global::<WindowRegistry>() else {
      return;
    };
    let state = registry.state.clone();
    let connection_id = profile.id.clone();
    let title = profile.name.clone();
    let bounds = Bounds::centered(None, size(px(1200.), px(800.)), cx);
    let opened = cx.open_window(
      WindowOptions {
        titlebar: Some(TitleBar::title_bar_options()),
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        window_min_size: Some(size(px(760.), px(480.))),
        ..Default::default()
      },
      |window, cx| {
        window.activate_window();
        window.set_window_title(&title);
        let view = cx.new(|cx| ConnectionWindow::new(state, db, profile, window, cx));
        cx.new(|cx| Root::new(view, window, cx))
      },
    );
    match opened {
      Ok(handle) => {
        cx.global_mut::<WindowRegistry>()
          .connections
          .push((connection_id, *handle));
      }
      // The connection stays open in core; the hub still lists it.
      Err(error) => log::warn!("failed to open a connection window: {error}"),
    }
  });
}

/// True when a window for this connection exists and took focus.
pub fn focus_connection_window(connection_id: &str, cx: &mut App) -> bool {
  let Some(registry) = cx.try_global::<WindowRegistry>() else {
    return false;
  };
  let Some((_, handle)) = registry
    .connections
    .iter()
    .find(|(id, _)| id == connection_id)
  else {
    return false;
  };
  (*handle)
    .update(cx, |_, window, _| window.activate_window())
    .is_ok()
}

/// `shell_window` opens the test window itself; the registry learns about it here.
#[cfg(test)]
pub(crate) fn register_hub_for_tests(handle: AnyWindowHandle, cx: &mut App) {
  cx.global_mut::<WindowRegistry>().hub = Some(handle);
}

#[cfg(test)]
mod tests {
  // The parent globs gpui: shadow `test` back or #[gpui::test] recurses.
  use ::core::prelude::v1::test;
  use gpui::{Entity, Modifiers, TestAppContext, VisualTestContext};
  use gpui_component::WindowExt;

  use super::*;
  use crate::test_support::{self, wait_until};

  fn approval_request(id: &str) -> soquel_core::mcp::McpApprovalRequest {
    soquel_core::mcp::McpApprovalRequest {
      id: id.to_string(),
      connection_id: "c1".to_string(),
      connection_name: "warehouse".to_string(),
      operation: format!("DELETE FROM t WHERE id = {id}"),
      payload: None,
    }
  }

  struct Booted {
    _dir: tempfile::TempDir,
    state: std::sync::Arc<AppState>,
    id: String,
    conn_handle: AnyWindowHandle,
  }

  /// Boot, seed a sqlite profile and open its window. sqlite IO wakes
  /// cross-thread, hence `allow_parking` + `wait_until`.
  fn open_one_connection(
    cx: &mut TestAppContext,
  ) -> (Booted, Entity<crate::app::App>, &mut VisualTestContext) {
    cx.executor().allow_parking();
    let (dir, state) = test_support::test_state();
    let id = test_support::seed_sqlite(&state, dir.path(), "local db");
    let (app, cx) = test_support::boot_app(cx, state.clone());

    let connections = cx.update(|_, cx| app.read(cx).connections_view());
    cx.update(|_, cx| connections.update(cx, |view, cx| view.connect(id.clone(), cx)));
    wait_until(cx, "the connection window", |cx| {
      cx.update(|_, cx| cx.windows().len() == 2)
    });
    let conn_handle = cx
      .update(|_, cx| {
        cx.global::<WindowRegistry>()
          .connections
          .iter()
          .find(|(open_id, _)| *open_id == id)
          .map(|(_, handle)| *handle)
      })
      .expect("the registry maps the connection window");
    (
      Booted {
        _dir: dir,
        state,
        id,
        conn_handle,
      },
      app,
      cx,
    )
  }

  #[gpui::test]
  fn connecting_from_the_hub_spawns_a_connection_window(cx: &mut TestAppContext) {
    let (booted, _app, cx) = open_one_connection(cx);
    assert!(
      booted
        .state
        .connections
        .try_lock()
        .unwrap()
        .contains_key(&booted.id)
    );
    let mut wcx = VisualTestContext::from_window(booted.conn_handle, cx);
    assert_eq!(wcx.window_title().as_deref(), Some("local db"));
  }

  #[gpui::test]
  fn connecting_an_already_open_profile_focuses_its_window(cx: &mut TestAppContext) {
    let (booted, app, cx) = open_one_connection(cx);

    // The hub takes focus back, then the same profile connects again.
    cx.update(|window, _| window.activate_window());
    let connections = cx.update(|_, cx| app.read(cx).connections_view());
    cx.update(|_, cx| connections.update(cx, |view, cx| view.connect(booted.id.clone(), cx)));
    cx.run_until_parked();

    assert_eq!(cx.update(|_, cx| cx.windows().len()), 2, "no second window");
    assert_eq!(
      cx.update(|_, cx| cx.active_window()),
      Some(booted.conn_handle),
      "the existing window took focus"
    );
    assert_eq!(
      booted.state.connections.try_lock().unwrap().len(),
      1,
      "no double connect"
    );
  }

  #[gpui::test]
  fn closing_a_connection_window_disconnects(cx: &mut TestAppContext) {
    let (booted, _app, cx) = open_one_connection(cx);

    let mut wcx = VisualTestContext::from_window(booted.conn_handle, cx);
    wcx.update(|window, _| window.remove_window());

    let state = booted.state.clone();
    wait_until(cx, "the disconnect", move |_| {
      state.connections.try_lock().is_ok_and(|map| map.is_empty())
    });
    assert_eq!(cx.update(|_, cx| cx.windows().len()), 1);
    cx.update(|_, cx| {
      assert!(cx.global::<WindowRegistry>().connections.is_empty());
    });
  }

  #[gpui::test]
  fn closing_the_hub_keeps_workspace_windows(cx: &mut TestAppContext) {
    let (booted, _app, cx) = open_one_connection(cx);
    let hub_handle = cx.update(|window, _| window.window_handle());

    // The rest of the test drives the surviving connection window.
    let mut wcx = VisualTestContext::from_window(booted.conn_handle, cx);
    hub_handle
      .update(&mut wcx, |_, window, _| window.remove_window())
      .unwrap();
    wcx.run_until_parked();

    assert_eq!(wcx.update(|_, cx| cx.windows().len()), 1);
    wcx.update(|_, cx| assert!(cx.global::<WindowRegistry>().hub.is_none()));
    assert!(
      booted
        .state
        .connections
        .try_lock()
        .unwrap()
        .contains_key(&booted.id),
      "the connection survived the hub"
    );

    wcx.update(|_, cx| focus_or_open_hub(cx));
    wcx.run_until_parked();
    assert_eq!(wcx.update(|_, cx| cx.windows().len()), 2);
    wcx.update(|_, cx| assert!(cx.global::<WindowRegistry>().hub.is_some()));
  }

  #[gpui::test]
  fn approvals_land_on_a_remaining_window_when_the_hub_closes(cx: &mut TestAppContext) {
    let (booted, _app, cx) = open_one_connection(cx);
    let hub_handle = cx.update(|window, _| window.window_handle());

    let mut wcx = VisualTestContext::from_window(booted.conn_handle, cx);
    hub_handle
      .update(&mut wcx, |_, window, _| window.remove_window())
      .unwrap();
    wcx.run_until_parked();

    let coordinator = wcx.update(|_, cx| crate::mcp::coordinator(cx));
    wcx.update(|_, cx| {
      coordinator.update(cx, |coordinator, cx| {
        coordinator.enqueue_approval(approval_request("1"), cx);
      });
    });
    wait_until(&mut wcx, "the approval dialog", |cx| {
      cx.update(|window, cx| window.has_active_dialog(cx))
    });

    let bounds = wcx
      .debug_bounds("approval-allow")
      .expect("run button painted");
    wcx.simulate_click(bounds.center(), Modifiers::none());
    wcx.run_until_parked();
    wcx.update(|_, cx| assert!(coordinator.read(cx).approval_queue.is_empty()));
  }
}
