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
    let view = cx.new(|cx| build(window, cx));
    inner = Some(view.clone());
    let shell = cx.new(|_| Shell { child: view.into() });
    Root::new(shell, window, cx)
  });
  // The test platform never activates windows, and the dialogs' deferred
  // open bails when `cx.active_window()` is None.
  cx.update(|window, _| window.activate_window());
  (inner.unwrap(), cx)
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
