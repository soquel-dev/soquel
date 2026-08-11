//! What makes a bug report actionable: where logs land, and a pasteable summary.

use std::path::PathBuf;

use tauri::{AppHandle, Manager};
use tauri_plugin_log::{Target, TargetKind};
use tauri_plugin_opener::OpenerExt;

use soquel_core::error::Error;
use soquel_core::AppState;

/// The log dir derives from the identifier, so debug and release share it: only
/// the file name keeps `tauri dev` out of an installed release's log.
#[cfg(debug_assertions)]
pub const LOG_FILE_NAME: &str = "soquel-dev";
#[cfg(not(debug_assertions))]
pub const LOG_FILE_NAME: &str = "soquel";

/// The plugin defaults to 40 kB, which holds about one session.
const MAX_LOG_SIZE: u128 = 2 * 1024 * 1024;

pub fn log_plugin() -> tauri::plugin::TauriPlugin<tauri::Wry> {
  let file_name = Some(LOG_FILE_NAME.to_string());
  let file_target = match log_dir_override() {
    Some(dir) => TargetKind::Folder {
      path: dir,
      file_name,
    },
    None => TargetKind::LogDir { file_name },
  };
  let mut targets = vec![Target::new(file_target)];
  if cfg!(debug_assertions) {
    targets.push(Target::new(TargetKind::Stdout));
  }
  tauri_plugin_log::Builder::default()
    .targets(targets)
    // Info everywhere would bury our own lines under russh, hyper and rustls.
    .level(log::LevelFilter::Warn)
    .level_for("soquel_lib", log::LevelFilter::Info)
    .max_file_size(MAX_LOG_SIZE)
    .build()
}

/// e2e sets SOQUEL_DATA_DIR: its logs belong with its data, not in the real log dir.
fn log_dir_override() -> Option<PathBuf> {
  log_dir_in(std::env::var("SOQUEL_DATA_DIR").ok())
}

/// Pure so its test mutates no process env, which would race every other test in
/// the binary.
fn log_dir_in(data_dir: Option<String>) -> Option<PathBuf> {
  data_dir.map(|dir| PathBuf::from(dir).join("logs"))
}

fn log_dir(app: &AppHandle) -> Result<PathBuf, Error> {
  match log_dir_override() {
    Some(dir) => Ok(dir),
    None => app.path().app_log_dir().map_err(|err| Error::Storage {
      message: err.to_string(),
    }),
  }
}

/// Returns the folder it asked for, because it cannot promise a window: the
/// opener spawns detached, so a session with no file manager fails silently.
pub fn open_log_folder(app: &AppHandle) -> Result<String, Error> {
  let dir = log_dir(app)?;
  // The folder rather than the file: a log opened in a text editor is not what
  // someone about to attach it to a report wants.
  app
    .opener()
    .open_path(dir.to_string_lossy(), None::<&str>)
    .map_err(|err| Error::Unsupported {
      message: format!("could not open the log folder: {err}"),
    })?;
  Ok(dir.display().to_string())
}

pub async fn block(app: &AppHandle, state: &AppState) -> String {
  let log = match log_dir(app) {
    Ok(dir) => dir
      .join(LOG_FILE_NAME)
      .with_extension("log")
      .display()
      .to_string(),
    Err(err) => err.to_string(),
  };
  let build = if cfg!(debug_assertions) {
    "debug"
  } else {
    "release"
  };
  soquel_core::diagnostics::block(state, &app.package_info().version.to_string(), build, &log).await
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn an_isolated_run_keeps_its_logs_with_its_data() {
    // e2e sets SOQUEL_DATA_DIR: logs landing in the real log dir instead would
    // be invisible until someone read a stranger's file.
    assert_eq!(log_dir_in(None), None);
    // Built from components rather than written with a separator: the expectation
    // would otherwise be a unix path a Windows run could never match.
    assert_eq!(
      log_dir_in(Some("run-data".to_string())),
      Some(["run-data", "logs"].iter().collect::<PathBuf>())
    );
  }
}
