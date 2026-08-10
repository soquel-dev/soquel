use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use crate::error::Error;

/// TOFU store: `host:port` -> openssh public key, persisted in the data dir.
pub struct KnownHostsStore {
  path: PathBuf,
  keys: HashMap<String, String>,
}

impl KnownHostsStore {
  pub fn load(path: PathBuf) -> Result<Self, Error> {
    let keys = match fs::read_to_string(&path) {
      Ok(raw) => serde_json::from_str(&raw)?,
      Err(err) if err.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
      Err(err) => return Err(err.into()),
    };
    Ok(Self { path, keys })
  }

  fn entry(host: &str, port: u16) -> String {
    format!("{host}:{port}")
  }

  pub fn get(&self, host: &str, port: u16) -> Option<String> {
    self.keys.get(&Self::entry(host, port)).cloned()
  }

  pub fn trust(&mut self, host: &str, port: u16, key: &str) -> Result<(), Error> {
    self.keys.insert(Self::entry(host, port), key.to_string());
    if let Some(dir) = self.path.parent() {
      fs::create_dir_all(dir)?;
    }
    fs::write(&self.path, serde_json::to_string_pretty(&self.keys)?)?;
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn trust_and_reload_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("known_hosts.json");

    let mut store = KnownHostsStore::load(path.clone()).unwrap();
    assert_eq!(store.get("bastion", 22), None);
    store.trust("bastion", 22, "ssh-ed25519 AAAA...").unwrap();

    let reloaded = KnownHostsStore::load(path).unwrap();
    assert_eq!(
      reloaded.get("bastion", 22).as_deref(),
      Some("ssh-ed25519 AAAA...")
    );
    assert_eq!(reloaded.get("bastion", 2222), None);
  }
}
