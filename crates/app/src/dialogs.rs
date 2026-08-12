//! Global dialogs open from contexts with no Window in hand.

use gpui::{App, AppContext, Styled, Window};
use gpui_component::dialog::Dialog;

/// Shared dialog chrome: elevated surface in dark mode, capped to the viewport
/// so a tall body scrolls instead of spilling off-screen.
pub fn styled(dialog: Dialog, window: &Window, cx: &App) -> Dialog {
  dialog
    .bg(crate::theme::panel(cx))
    .max_h(window.viewport_size().height * 0.85)
}

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
