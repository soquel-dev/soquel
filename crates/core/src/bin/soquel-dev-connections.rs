use std::path::{Path, PathBuf};
use std::sync::Arc;

use soquel_core::profiles::{
  AgentAccess, ConnectionInput, ConnectorParams, CredentialSource, Env, MongoParams, RedisParams,
  SqlServerParams, SslMode,
};
use soquel_core::{ops, AppState};

const GROUP: &str = "Docker dev";
const PASSWORD: &str = "soquel";

fn main() {
  if let Err(err) = run() {
    eprintln!("{err}");
    std::process::exit(1);
  }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
  if !cfg!(debug_assertions) {
    return Err("soquel-dev-connections can only run in debug builds".into());
  }

  let Some(command) = std::env::args().nth(1) else {
    return Err("usage: soquel-dev-connections <reset|seed>".into());
  };
  let data_dir = data_dir();
  let state = Arc::new(AppState::load(
    &data_dir,
    soquel_core::secrets::store_from_env(&data_dir)?,
  )?);

  match command.as_str() {
    "reset" => {
      let removed = reset(&state)?;
      println!("removed {removed} dev connections");
    }
    "seed" => {
      let removed = reset(&state)?;
      let created = seed(&state)?;
      println!("removed {removed} dev connections");
      println!("created {created} dev connections");
    }
    _ => return Err("usage: soquel-dev-connections <reset|seed>".into()),
  }

  Ok(())
}

fn data_dir() -> PathBuf {
  if let Some(dir) = std::env::var_os("SOQUEL_DATA_DIR") {
    return PathBuf::from(dir);
  }
  let base = std::env::var_os("XDG_DATA_HOME")
    .map(PathBuf::from)
    .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".local/share")))
    .unwrap_or_default();
  let root = base.join("dev.soquel.app");
  if cfg!(debug_assertions) {
    root.join("dev")
  } else {
    root
  }
}

fn reset(state: &AppState) -> Result<usize, Box<dyn std::error::Error>> {
  let ids: Vec<String> = state
    .profiles
    .lock()
    .unwrap()
    .list()
    .into_iter()
    .filter(|profile| profile.group.as_deref() == Some(GROUP))
    .map(|profile| profile.id)
    .collect();

  let count = ids.len();
  for id in ids {
    ops::delete_connection(state, &id)?;
  }
  Ok(count)
}

fn seed(state: &AppState) -> Result<usize, Box<dyn std::error::Error>> {
  let connections = dev_connections();
  let count = connections.len();
  for input in connections {
    ops::create_connection(state, &input)?;
  }
  Ok(count)
}

fn dev_connections() -> Vec<ConnectionInput> {
  vec![
    ConnectionInput {
      name: "Postgres dev".to_string(),
      env: Env::Dev,
      group: Some(GROUP.to_string()),
      agent_access: AgentAccess::None,
      credential: CredentialSource::Keychain,
      params: ConnectorParams::Postgres(SqlServerParams {
        host: "localhost".to_string(),
        port: 5470,
        database: "soquel_dev".to_string(),
        user: "soquel".to_string(),
        ssl_mode: SslMode::Disable,
        ssl_root_cert: None,
        tunnel_id: None,
      }),
      password: Some(PASSWORD.to_string()),
    },
    ConnectionInput {
      name: "MySQL dev".to_string(),
      env: Env::Dev,
      group: Some(GROUP.to_string()),
      agent_access: AgentAccess::None,
      credential: CredentialSource::Keychain,
      params: ConnectorParams::Mysql(SqlServerParams {
        host: "localhost".to_string(),
        port: 5471,
        database: "soquel_dev".to_string(),
        user: "soquel".to_string(),
        ssl_mode: SslMode::Disable,
        ssl_root_cert: None,
        tunnel_id: None,
      }),
      password: Some(PASSWORD.to_string()),
    },
    ConnectionInput {
      name: "Redis dev".to_string(),
      env: Env::Dev,
      group: Some(GROUP.to_string()),
      agent_access: AgentAccess::None,
      credential: CredentialSource::Keychain,
      params: ConnectorParams::Redis(RedisParams {
        host: "localhost".to_string(),
        port: 5472,
        db: 0,
        username: None,
        tls: false,
        tunnel_id: None,
      }),
      password: Some(PASSWORD.to_string()),
    },
    ConnectionInput {
      name: "Mongo dev".to_string(),
      env: Env::Dev,
      group: Some(GROUP.to_string()),
      agent_access: AgentAccess::None,
      credential: CredentialSource::Keychain,
      params: ConnectorParams::Mongo(MongoParams {
        host: "localhost".to_string(),
        port: 5473,
        database: Some("soquel_dev".to_string()),
        username: Some("soquel".to_string()),
        auth_source: Some("admin".to_string()),
        tls: false,
        tunnel_id: None,
      }),
      password: Some(PASSWORD.to_string()),
    },
  ]
}
