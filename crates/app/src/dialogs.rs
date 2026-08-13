//! Global dialogs open from contexts with no Window in hand, plus the form
//! chrome shared by the connection and tunnel dialogs.

use std::rc::Rc;

use gpui::{
  AnyElement, App, AppContext, Hsla, InteractiveElement, IntoElement, ParentElement, SharedString,
  Styled, Window, div, px, rems,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::dialog::Dialog;
use gpui_component::form::Field;
use gpui_component::{ActiveTheme, Icon, IconName, Sizable, WindowExt, h_flex};

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

/// Defers, then runs on the window the entity currently lives in; no-op when
/// the entity or its window is gone. `with_window` keeps the entity unleased,
/// so `f` may update it.
pub fn defer_on_entity_window<T: 'static>(
  entity: gpui::WeakEntity<T>,
  cx: &mut App,
  f: impl FnOnce(&mut Window, &mut App) + 'static,
) {
  cx.defer(move |cx| {
    let _ = cx.with_window(entity.entity_id(), f);
  });
}

/// Active window if still alive, else any window: for global dialogs that must
/// never be dropped (MCP approvals). The active handle can be stale right
/// after a close, hence the contains filter.
pub fn defer_on_some_window(cx: &mut App, f: impl FnOnce(&mut Window, &mut App) + 'static) {
  cx.defer(move |cx| {
    let windows = cx.windows();
    let Some(window_handle) = cx
      .active_window()
      .filter(|handle| windows.contains(handle))
      .or_else(|| windows.into_iter().next())
    else {
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

/// A destructive confirmation: title, rebuilt-per-frame body, Cancel, and a
/// danger button that runs `on_confirm` after closing the dialog.
pub fn confirm_danger(
  window: &mut Window,
  cx: &mut App,
  title: impl Into<SharedString>,
  body: impl Fn(&App) -> AnyElement + 'static,
  confirm_label: &'static str,
  confirm_selector: &'static str,
  on_confirm: impl Fn(&mut Window, &mut App) + 'static,
) {
  let title: SharedString = title.into();
  let on_confirm = Rc::new(on_confirm);
  window.open_dialog(cx, move |dialog, window, cx| {
    let on_confirm = on_confirm.clone();
    styled(dialog, window, cx)
      .title(title.clone())
      .w(px(400.))
      .child(body(cx))
      .footer(
        h_flex()
          .gap_2()
          .justify_end()
          .child(
            Button::new("confirm-cancel")
              .label("Cancel")
              .on_click(|_, window, cx| window.close_dialog(cx)),
          )
          .child(
            Button::new("confirm-danger")
              .danger()
              .label(confirm_label)
              .debug_selector(move || confirm_selector.to_string())
              .on_click(move |_, window, cx| {
                window.close_dialog(cx);
                on_confirm(window, cx);
              }),
          ),
      )
  });
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
