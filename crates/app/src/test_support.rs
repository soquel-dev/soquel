//! Dialog-capable test windows: `add_empty_window` roots the window on gpui's
//! `Empty`, and `Root::update` panics on anything that is not a `Root`.

use gpui::*;
use gpui_component::Root;

/// Mirrors `App::render`: hosts the view under test plus the dialog and
/// notification layers, which `Root` itself does not render.
pub struct Shell {
  child: AnyView,
}

impl Render for Shell {
  fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    div()
      .size_full()
      .child(self.child.clone())
      .children(Root::render_dialog_layer(window, cx))
      .children(Root::render_notification_layer(window, cx))
  }
}

pub fn shell_window<V: Render + 'static>(
  cx: &mut TestAppContext,
  build: impl FnOnce(&mut Window, &mut Context<V>) -> V,
) -> (Entity<V>, &mut VisualTestContext) {
  cx.update(gpui_component::init);
  // The dialog slide/fade animations read the real clock and keep the window
  // dirty forever; reduce_motion renders them settled.
  cx.update(|cx| cx.set_reduce_motion(true));
  let mut inner = None;
  let (_root, cx) = cx.add_window_view(|window, cx| {
    // The test platform never activates windows, and both the dialogs'
    // deferred open and view tasks routing through `cx.active_window()` bail
    // when it is None. Activate before the view spawns its first task.
    window.activate_window();
    let view = cx.new(|cx| build(window, cx));
    inner = Some(view.clone());
    let shell = cx.new(|_| Shell { child: view.into() });
    Root::new(shell, window, cx)
  });
  (inner.unwrap(), cx)
}

pub fn test_state() -> (tempfile::TempDir, std::sync::Arc<soquel_core::AppState>) {
  let dir = tempfile::tempdir().unwrap();
  let state = std::sync::Arc::new(soquel_core::AppState::for_tests(
    dir.path(),
    Box::new(soquel_core::secrets::InMemoryStore::default()),
  ));
  (dir, state)
}

/// A connectable profile: sqlite over a file that must already exist, since
/// the connector refuses to mint one.
pub fn seed_sqlite(state: &soquel_core::AppState, dir: &std::path::Path, name: &str) -> String {
  let path = dir.join(format!("{name}.sqlite"));
  std::fs::File::create(&path).unwrap();
  soquel_core::ops::create_connection(
    state,
    &soquel_core::profiles::ConnectionInput {
      name: name.to_string(),
      env: soquel_core::profiles::Env::Dev,
      group: None,
      agent_access: soquel_core::profiles::AgentAccess::None,
      credential: Default::default(),
      params: soquel_core::profiles::ConnectorParams::Sqlite {
        path: path.to_string_lossy().into_owned(),
      },
      password: None,
    },
  )
  .unwrap()
  .id
}

/// Boots the multi-window app: coordinator + registry globals, the hub `App`
/// in a shell window registered as the hub.
pub fn boot_app(
  cx: &mut TestAppContext,
  state: std::sync::Arc<soquel_core::AppState>,
) -> (Entity<crate::app::App>, &mut VisualTestContext) {
  cx.update(|cx| {
    crate::mcp::init_coordinator(state.clone(), cx);
    crate::windows::init(state.clone(), cx);
  });
  let (app, cx) = shell_window(cx, move |window, cx| {
    crate::app::App::new(state, window, cx)
  });
  let handle = cx.update(|window, _| window.window_handle());
  cx.update(|_, cx| crate::windows::register_hub_for_tests(handle, cx));
  (app, cx)
}

/// Settle-and-check. Deterministic tests satisfy the predicate on the first
/// pass; the real-time polling only runs for integration tests, whose database
/// wakes `run_until_parked` cannot see.
#[track_caller]
pub fn wait_until(
  cx: &mut VisualTestContext,
  what: &str,
  mut pred: impl FnMut(&mut VisualTestContext) -> bool,
) {
  let started = std::time::Instant::now();
  loop {
    cx.run_until_parked();
    if pred(cx) {
      return;
    }
    assert!(
      started.elapsed() < std::time::Duration::from_secs(5),
      "timed out waiting for {what}"
    );
    std::thread::sleep(std::time::Duration::from_millis(10));
  }
}

/// `debug_bounds` wants `&'static str`; test-lifetime leaks are fine.
pub fn selector(name: String) -> &'static str {
  Box::leak(name.into_boxed_str())
}
