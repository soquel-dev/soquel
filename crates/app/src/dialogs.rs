//! Global dialogs open from contexts with no Window in hand, plus the form
//! chrome shared by the connection and tunnel dialogs.

use gpui::{
  AnyElement, App, AppContext, Hsla, IntoElement, ParentElement, SharedString, Styled, Window, div,
  rems,
};
use gpui_component::dialog::Dialog;
use gpui_component::form::Field;
use gpui_component::{ActiveTheme, Icon, IconName, Sizable, h_flex};

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

/// Shared dialog chrome: elevated surface in dark mode, capped to the viewport
/// so a tall body scrolls instead of spilling off-screen.
pub fn styled(dialog: Dialog, window: &Window, cx: &App) -> Dialog {
  dialog
    .bg(crate::theme::panel(cx))
    .max_h(window.viewport_size().height * 0.85)
}

/// The description slot under an input: a validation error wins over the hint.
pub fn field_note(f: Field, error: Option<SharedString>, hint: Option<SharedString>) -> Field {
  match (error, hint) {
    (Some(message), _) => f.description_fn(move |_, cx| {
      div()
        .text_color(cx.theme().danger)
        .child(message.clone())
        .into_any_element()
    }),
    (None, Some(text)) => f.description(text),
    (None, None) => f,
  }
}

/// Test feedback shown inside a form dialog, styled by outcome.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub enum FormStatus {
  #[default]
  Idle,
  Testing,
  Ok,
  Error(SharedString),
}

/// The tinted outcome row at the bottom of a form; None while idle.
pub fn form_status_row(status: &FormStatus, cx: &App) -> Option<AnyElement> {
  match status {
    FormStatus::Idle => None,
    FormStatus::Testing => Some(
      h_flex()
        .gap_2()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child("Testing connection…")
        .into_any_element(),
    ),
    FormStatus::Ok => Some(banner(
      cx.theme().green,
      IconName::CircleCheck,
      "Connection ok".into(),
      cx,
    )),
    FormStatus::Error(message) => Some(banner(
      cx.theme().red,
      IconName::CircleX,
      message.clone(),
      cx,
    )),
  }
}

fn banner(color: Hsla, icon: IconName, text: SharedString, cx: &App) -> AnyElement {
  // Pinned line height so the icon box matches the first text line exactly.
  let line = rems(1.25);
  h_flex()
    .items_start()
    .gap_2()
    .px_3()
    .py_2()
    .rounded(cx.theme().radius)
    .bg(color.opacity(0.1))
    .text_color(color)
    .text_sm()
    .line_height(line)
    .child(
      h_flex()
        .h(line)
        .items_center()
        .child(Icon::new(icon).small()),
    )
    .child(div().flex_1().min_w_0().child(text))
    .into_any_element()
}
