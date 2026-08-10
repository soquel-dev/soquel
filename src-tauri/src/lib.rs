use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tauri::Manager;

use crate::command_approvals::CommandApprovalsStore;
use crate::connectors::{Connection, SqlSession};
use crate::credentials::SessionSecrets;
use crate::known_hosts::KnownHostsStore;
use crate::profiles::ProfileStore;
use crate::secrets::{FileStore, InMemoryStore, KeyringStore, SecretStore};
use crate::ssh::SshTunnel;
use crate::tunnels::TunnelStore;

mod activation;
mod command_approvals;
mod commands;
// Pub: the gpui frontend consumes the core directly, without the command layer.
pub mod connectors;
pub mod credentials;
mod diagnostics;
pub mod error;
mod export;
mod known_hosts;
mod licence;
mod mcp;
mod mongo;
mod mysql;
mod postgres;
pub mod profiles;
mod redis;
mod secrets;
mod sqlite;
mod ssh;
mod transfer;
mod tunnels;
mod updater;

/// A connected database plus the tunnel carrying it: dropped together.
pub struct ActiveConnection {
  pub connection: Arc<dyn Connection>,
  pub _tunnel: Option<SshTunnel>,
}

pub struct SessionEntry {
  pub connection_id: String,
  pub session: Arc<dyn SqlSession>,
}

pub struct AppState {
  pub profiles: Mutex<ProfileStore>,
  pub tunnels: Mutex<TunnelStore>,
  pub known_hosts: Mutex<KnownHostsStore>,
  pub command_approvals: Mutex<CommandApprovalsStore>,
  pub secrets: Box<dyn SecretStore>,
  /// Probed once at startup: talking to the keyring on every render would be
  /// a D-Bus round trip per keystroke in the form.
  pub secrets_problem: Option<String>,
  pub session_secrets: SessionSecrets,
  pub connections: tokio::sync::Mutex<HashMap<String, ActiveConnection>>,
  pub sessions: tokio::sync::Mutex<HashMap<String, SessionEntry>>,
  pub data_dir: std::path::PathBuf,
  pub mcp: tokio::sync::Mutex<Option<mcp::McpRunning>>,
  /// Agent write requests waiting on the approval dialog.
  pub approvals:
    tokio::sync::Mutex<HashMap<String, tokio::sync::oneshot::Sender<mcp::ApprovalAnswer>>>,
  /// Live "allow writes for a while" grants, keyed by (mcp session, connection).
  pub trust_windows: tokio::sync::Mutex<HashMap<(String, String), mcp::TrustWindow>>,
}

#[cfg(test)]
impl AppState {
  /// Stores rooted in `dir`, nothing connected. Tests that need a live
  /// connection go through the connectors themselves.
  pub fn for_tests(dir: &std::path::Path, secrets: Box<dyn SecretStore>) -> Self {
    Self {
      profiles: Mutex::new(ProfileStore::load(dir.join("connections.json")).unwrap()),
      tunnels: Mutex::new(TunnelStore::load(dir.join("tunnels.json")).unwrap()),
      known_hosts: Mutex::new(KnownHostsStore::load(dir.join("known_hosts.json")).unwrap()),
      command_approvals: Mutex::new(
        CommandApprovalsStore::load(dir.join("command_approvals.json")).unwrap(),
      ),
      secrets,
      secrets_problem: None,
      session_secrets: SessionSecrets::default(),
      connections: tokio::sync::Mutex::new(HashMap::new()),
      sessions: tokio::sync::Mutex::new(HashMap::new()),
      data_dir: dir.to_path_buf(),
      mcp: tokio::sync::Mutex::new(None),
      approvals: tokio::sync::Mutex::new(HashMap::new()),
      trust_windows: tokio::sync::Mutex::new(HashMap::new()),
    }
  }
}

fn specta_builder() -> tauri_specta::Builder<tauri::Wry> {
  tauri_specta::Builder::new()
    .commands(tauri_specta::collect_commands![
      commands::connector_capabilities,
      commands::list_connections,
      commands::get_connection,
      commands::create_connection,
      commands::update_connection,
      commands::delete_connection,
      commands::export_connections,
      commands::open_connections_file,
      commands::preview_connection_import,
      commands::import_connections,
      commands::test_connection,
      commands::connect,
      commands::unlock_secret,
      commands::parse_credential_command,
      commands::approve_credential_command,
      commands::revoke_credential_command,
      commands::disconnect,
      commands::active_connections,
      commands::server_version,
      commands::run_query,
      commands::cancel_query,
      commands::table_rows,
      commands::stream_table_rows,
      commands::export_table_rows,
      commands::export_statement,
      commands::format_statement,
      commands::apply_table_changes,
      commands::schema_snapshot,
      commands::table_ddl,
      commands::scan_keys,
      commands::key_detail,
      commands::kv_set_string,
      commands::kv_delete_key,
      commands::kv_set_ttl,
      commands::kv_run_command,
      commands::kv_databases,
      commands::kv_select_db,
      commands::doc_databases,
      commands::doc_collections,
      commands::doc_find,
      commands::doc_detail,
      commands::doc_replace,
      commands::doc_delete,
      commands::doc_indexes,
      commands::doc_count,
      commands::doc_run_query,
      commands::open_sql_session,
      commands::run_session_query,
      commands::cancel_session_query,
      commands::close_sql_session,
      commands::list_tunnels,
      commands::get_tunnel,
      commands::create_tunnel,
      commands::update_tunnel,
      commands::delete_tunnel,
      commands::test_tunnel,
      commands::default_ssh_keys,
      commands::trust_host_key,
      commands::mcp_status,
      commands::mcp_start,
      commands::mcp_stop,
      commands::mcp_set_port,
      commands::mcp_regenerate_token,
      commands::mcp_audit_log,
      commands::mcp_resolve_approval,
      commands::mcp_trust_windows,
      commands::mcp_revoke_trust,
      commands::licence_status,
      commands::install_licence,
      commands::activate_licence,
      commands::tab_limit_override,
      commands::secrets_status,
      commands::platform,
      commands::diagnostics,
      commands::open_log_folder,
      commands::check_update,
      commands::install_update,
    ])
    .events(tauri_specta::collect_events![
      mcp::McpApprovalRequest,
      transfer::ImportFileRequested,
      updater::UpdateProgress
    ])
    .error_handling(tauri_specta::ErrorHandlingMode::Result)
}

// Anchored to the crate dir: the binary's cwd is wherever the runner spawned it.
#[cfg(debug_assertions)]
const BINDINGS_PATH: &str = concat!(
  env!("CARGO_MANIFEST_DIR"),
  "/../packages/app/src/lib/bindings.ts"
);

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
  let builder = specta_builder();

  #[cfg(debug_assertions)]
  builder
    .export(specta_typescript::Typescript::default(), BINDINGS_PATH)
    .expect("failed to export typescript bindings");

  tauri::Builder::default()
    .plugin(tauri_plugin_dialog::init())
    .plugin(tauri_plugin_updater::Builder::new().build())
    .plugin(tauri_plugin_opener::init())
    // Registered on the builder rather than in setup: the chain runs first, so
    // the keyring probe and everything else at startup lands in the log.
    .plugin(diagnostics::log_plugin())
    .invoke_handler(builder.invoke_handler())
    .setup(move |app| {
      // Typed event channel for the approval dialog.
      builder.mount_events(app);
      // SOQUEL_DATA_DIR isolates e2e runs from the real app data; debug builds
      // get their own subtree so `tauri dev` never touches an installed
      // release's connections.
      let data_dir = match std::env::var("SOQUEL_DATA_DIR") {
        Ok(dir) => std::path::PathBuf::from(dir),
        Err(_) if cfg!(debug_assertions) => app.path().app_data_dir()?.join("dev"),
        Err(_) => app.path().app_data_dir()?,
      };
      let store = ProfileStore::load(data_dir.join("connections.json"))?;
      let tunnels = TunnelStore::load(data_dir.join("tunnels.json"))?;
      let known_hosts = KnownHostsStore::load(data_dir.join("known_hosts.json"))?;
      let command_approvals = CommandApprovalsStore::load(data_dir.join("command_approvals.json"))?;
      // Keychain-less environments: e2e/CI (ephemeral) and WSL dev (plaintext file, opt-in).
      let secrets: Box<dyn SecretStore> = if std::env::var("SOQUEL_EPHEMERAL_SECRETS").is_ok() {
        Box::new(InMemoryStore::default())
      } else if std::env::var("SOQUEL_INSECURE_FILE_SECRETS").is_ok() {
        Box::new(FileStore::load(data_dir.join("secrets.json"))?)
      } else {
        Box::new(KeyringStore)
      };
      let secrets_problem = secrets.probe().err().map(|err| err.to_string());
      app.manage(AppState {
        profiles: Mutex::new(store),
        tunnels: Mutex::new(tunnels),
        known_hosts: Mutex::new(known_hosts),
        command_approvals: Mutex::new(command_approvals),
        secrets,
        secrets_problem,
        session_secrets: SessionSecrets::default(),
        connections: tokio::sync::Mutex::new(HashMap::new()),
        sessions: tokio::sync::Mutex::new(HashMap::new()),
        data_dir,
        mcp: tokio::sync::Mutex::new(None),
        approvals: tokio::sync::Mutex::new(HashMap::new()),
        trust_windows: tokio::sync::Mutex::new(HashMap::new()),
      });
      mcp::autostart(app.handle());
      Ok(())
    })
    .run(tauri::generate_context!())
    .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
  // Regenerates the bindings without launching the app.
  #[test]
  fn export_typescript_bindings() {
    super::specta_builder()
      .export(
        specta_typescript::Typescript::default(),
        super::BINDINGS_PATH,
      )
      .expect("failed to export typescript bindings");
  }
}
