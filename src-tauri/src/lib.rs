use tauri::Manager;

use soquel_core::secrets::{FileStore, InMemoryStore, KeyringStore, SecretStore};
use soquel_core::AppState;

mod commands;
mod diagnostics;
mod mcp;
mod updater;

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
      commands::ImportFileRequested,
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
      // Keychain-less environments: e2e/CI (ephemeral) and WSL dev (plaintext file, opt-in).
      let secrets: Box<dyn SecretStore> = if std::env::var("SOQUEL_EPHEMERAL_SECRETS").is_ok() {
        Box::new(InMemoryStore::default())
      } else if std::env::var("SOQUEL_INSECURE_FILE_SECRETS").is_ok() {
        Box::new(FileStore::load(data_dir.join("secrets.json"))?)
      } else {
        Box::new(KeyringStore)
      };
      app.manage(AppState::load(&data_dir, secrets)?);
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
