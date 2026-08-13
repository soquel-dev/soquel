//! Status-line errors also land in the log: the status is overwritten by the
//! next operation, the log line survives into a diagnostics bundle.

use gpui::{App, AppContext, SharedString};
use gpui_component::WindowExt;
use gpui_component::notification::Notification;

/// Log, toast on the active window, and hand back the bare message.
///
/// Falls back to the hub, then any window: an OS prompt (keychain, file
/// picker) can hold focus when the error lands, and the toast must not be
/// dropped for it.
#[track_caller]
pub fn toast_error(error: &impl std::fmt::Display, cx: &mut App) -> SharedString {
  let caller = std::panic::Location::caller();
  log::warn!("{}:{}: {error}", caller.file(), caller.line());
  let message: SharedString = format!("{error}").into();
  let toast = message.clone();
  cx.defer(move |cx| {
    let Some(handle) = cx
      .active_window()
      .or_else(|| {
        cx.try_global::<crate::windows::WindowRegistry>()
          .and_then(|registry| registry.hub_handle())
      })
      .or_else(|| cx.windows().into_iter().next())
    else {
      return;
    };
    let _ = cx.update_window(handle, |_, window, cx| {
      window.push_notification(Notification::error(toast), cx);
    });
  });
  message
}

/// "error: X" for a status line, warned to the log with the caller's location.
#[track_caller]
pub fn error(error: &impl std::fmt::Display) -> SharedString {
  let caller = std::panic::Location::caller();
  log::warn!("{}:{}: {error}", caller.file(), caller.line());
  format!("error: {error}").into()
}

/// The bare message for fields that add their own framing.
#[track_caller]
pub fn message(error: &impl std::fmt::Display) -> SharedString {
  let caller = std::panic::Location::caller();
  log::warn!("{}:{}: {error}", caller.file(), caller.line());
  format!("{error}").into()
}

/// Log-and-drop for results whose failure has no surface to land on.
#[track_caller]
pub fn ok_or_log<T, E: std::fmt::Display>(result: Result<T, E>) -> Option<T> {
  match result {
    Ok(value) => Some(value),
    Err(error) => {
      let caller = std::panic::Location::caller();
      log::warn!("{}:{}: {error}", caller.file(), caller.line());
      None
    }
  }
}
