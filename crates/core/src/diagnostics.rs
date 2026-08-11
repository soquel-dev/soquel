//! A pasteable support block: facts only, no connection names, hosts or database
//! paths, so it can land in a public issue. Counts answer "does this only happen
//! with mongo?" and carry nothing worth hiding. The frontends own where logs live
//! and how a folder opens; this is the UI-agnostic part.

use std::collections::BTreeMap;

use crate::profiles::{ConnectionProfile, ConnectorKind};
use crate::AppState;

const fn kind_label(kind: ConnectorKind) -> &'static str {
  match kind {
    ConnectorKind::Postgres => "postgres",
    ConnectorKind::Mysql => "mysql",
    ConnectorKind::Sqlite => "sqlite",
    ConnectorKind::Redis => "redis",
    ConnectorKind::Mongo => "mongo",
  }
}

/// The environment facts, gathered by the caller so `render` stays testable.
pub struct Facts<'a> {
  pub version: &'a str,
  pub build: &'a str,
  pub keychain: &'a str,
  pub log: &'a str,
  pub mcp: &'a str,
}

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

/// `version`/`build`/`log_path` are the frontend's: it knows its own package
/// version, whether it is a debug build, and where its logs land.
pub async fn block(state: &AppState, version: &str, build: &str, log_path: &str) -> String {
  let keychain = match &state.secrets_problem {
    None => "available".to_string(),
    Some(problem) => format!("unavailable - {problem}"),
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
      version,
      build,
      keychain: &keychain,
      log: log_path,
      mcp: &mcp,
    },
    &profiles,
    tunnels,
  )
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::profiles::{ConnectorParams, Env, SqlServerParams, SslMode};

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
