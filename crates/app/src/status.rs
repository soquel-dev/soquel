//! Status-line errors also land in the log: the status is overwritten by the
//! next operation, the log line survives into a diagnostics bundle.

use gpui::SharedString;

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
