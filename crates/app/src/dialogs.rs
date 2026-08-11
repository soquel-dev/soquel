//! Global dialogs open from contexts with no Window in hand.

use gpui::{App, AppContext, Window};

/// Defers, then runs on whichever window is active: the dialog stack needs a
/// Window, and palette or background callers do not have one.
pub fn defer_on_active_window(cx: &mut App, f: impl FnOnce(&mut Window, &mut App) + 'static) {
  cx.defer(move |cx| {
    let Some(window_handle) = cx.active_window() else {
      return;
    };
    let _ = cx.update_window(window_handle, |_, window, cx| f(window, cx));
  });
}
