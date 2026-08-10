//! What makes a bug report actionable: where logs land, and a pasteable summary.

use std::collections::BTreeMap;
use std::path::PathBuf;

use tauri::{AppHandle, Manager};
use tauri_plugin_log::{Target, TargetKind};
use tauri_plugin_opener::OpenerExt;

use soquel_core::error::Error;
use soquel_core::profiles::{ConnectionProfile, ConnectorKind};
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

const fn kind_label(kind: ConnectorKind) -> &'static str {
  match kind {
    ConnectorKind::Postgres => "postgres",
    ConnectorKind::Mysql => "mysql",
    ConnectorKind::Sqlite => "sqlite",
    ConnectorKind::Redis => "redis",
    ConnectorKind::Mongo => "mongo",
  }
}

/// The environment facts, gathered from the app handle so `render` stays testable.
struct Facts<'a> {
  version: &'a str,
  build: &'a str,
  keychain: &'a str,
  log: &'a str,
  mcp: &'a str,
}

/// Facts only: no connection names, no hosts, no database paths. Counts answer
/// "does this only happen with mongo?" and carry nothing worth hiding.
fn render(facts: &Facts, profiles: &[ConnectionProfile], tunnels: usize) -> String {
  let mut per_kind: BTreeMap<&str, usize> = BTreeMap::new();
  for profile in profiles {
    *per_kind
      .entry(kind_label(profile.params.kind()))
      .or_default() += 1;
  }
  let kinds = per_kind
    .iter()
    .map(|(kind, count)| format!("{kind} {count}"))
    .collect::<Vec<_>>()
    .join(", ");

  let mut lines = vec![
    format!("soquel {} ({})", facts.version, facts.build),
    format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
    format!("keychain: {}", facts.keychain),
    format!("log: {}", facts.log),
    format!("connections: {}", profiles.len()),
  ];
  if !kinds.is_empty() {
    lines.push(format!("kinds: {kinds}"));
  }
  lines.push(format!("tunnels: {tunnels}"));
  lines.push(format!("mcp: {}", facts.mcp));
  lines.join("\n")
}

pub async fn block(app: &AppHandle, state: &AppState) -> String {
  let keychain = match &state.secrets_problem {
    None => "available".to_string(),
    Some(problem) => format!("unavailable - {problem}"),
  };
  let log = match log_dir(app) {
    Ok(dir) => dir
      .join(LOG_FILE_NAME)
      .with_extension("log")
      .display()
      .to_string(),
    Err(err) => err.to_string(),
  };
  // Read the handle rather than mcp::status: that one mints a keychain token.
  let mcp = match state.mcp.lock().await.as_ref() {
    Some(running) => format!("running on {}", running.port),
    None => "stopped".to_string(),
  };
  let profiles = state.profiles.lock().unwrap().list();
  let tunnels = state.tunnels.lock().unwrap().list().len();

  render(
    &Facts {
      version: &app.package_info().version.to_string(),
      build: if cfg!(debug_assertions) {
        "debug"
      } else {
        "release"
      },
      keychain: &keychain,
      log: &log,
      mcp: &mcp,
    },
    &profiles,
    tunnels,
  )
}

#[cfg(test)]
mod tests {
  use super::*;
  use soquel_core::profiles::{ConnectorParams, Env, SqlServerParams, SslMode};

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

  fn facts() -> Facts<'static> {
    Facts {
      version: "0.1.0",
      build: "debug",
      keychain: "available",
      log: "/tmp/logs/soquel-dev.log",
      mcp: "stopped",
    }
  }

  fn profile(name: &str, params: ConnectorParams) -> ConnectionProfile {
    ConnectionProfile {
      id: "c-1".to_string(),
      name: name.to_string(),
      env: Env::Prod,
      group: None,
      agent_access: Default::default(),
      credential: Default::default(),
      params,
    }
  }

  fn pg(host: &str) -> ConnectorParams {
    ConnectorParams::Postgres(SqlServerParams {
      host: host.to_string(),
      port: 5432,
      database: "shop".to_string(),
      user: "app".to_string(),
      ssl_mode: SslMode::Prefer,
      ssl_root_cert: None,
      tunnel_id: None,
    })
  }

  #[test]
  fn a_pasteable_block_names_no_connection() {
    // This block is meant to land in a public issue: a name or a host in it
    // would be pasted there by someone who never reread it.
    let profiles = vec![
      profile("prod billing", pg("db.internal")),
      profile("staging", pg("staging.internal")),
    ];

    let block = render(&facts(), &profiles, 1);

    assert!(!block.contains("prod billing"), "{block}");
    assert!(!block.contains("db.internal"), "{block}");
    assert!(!block.contains("shop"), "{block}");
    assert!(block.contains("connections: 2"), "{block}");
    assert!(block.contains("postgres 2"), "{block}");
  }

  #[test]
  fn no_connection_yet_means_no_kinds_line() {
    let block = render(&facts(), &[], 0);

    assert!(block.contains("connections: 0"), "{block}");
    assert!(!block.contains("kinds:"), "{block}");
    assert!(block.contains("keychain: available"), "{block}");
  }
}
