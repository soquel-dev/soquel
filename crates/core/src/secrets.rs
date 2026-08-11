use crate::error::Error;

// Debug builds keep their own keychain scope: dev must never touch the
// secrets of an installed release.
#[cfg(debug_assertions)]
const SERVICE: &str = "dev.soquel.app.dev";
#[cfg(not(debug_assertions))]
const SERVICE: &str = "dev.soquel.app";

/// What a stored secret belongs to. Typed rather than a prefixed string: the
/// stores, the session cache and the prompt all key off the same thing.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SecretKey {
  Connection(String),
  Tunnel(String),
  /// The bearer token the MCP server hands to agents.
  McpToken,
}

impl SecretKey {
  /// Entry name in the store, one namespace per kind.
  pub fn storage_id(&self) -> String {
    match self {
      Self::Connection(id) => format!("connection:{id}"),
      Self::Tunnel(id) => format!("tunnel:{id}"),
      Self::McpToken => "mcp-token".to_string(),
    }
  }
}

pub trait SecretStore: Send + Sync {
  fn set(&self, key: &SecretKey, secret: &str) -> Result<(), Error>;
  fn get(&self, key: &SecretKey) -> Result<Option<String>, Error>;
  fn delete(&self, key: &SecretKey) -> Result<(), Error>;

  /// Whether the store can hold a secret at all. Probed at startup: a session
  /// with no keyring must not be discovered after the user typed a password.
  fn probe(&self) -> Result<(), Error> {
    Ok(())
  }
}

/// What the frontend needs to know about secret storage.
#[derive(Debug, Clone, serde::Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct SecretsStatus {
  pub keychain: bool,
  /// Why the keychain is unusable, ready to show as-is.
  pub problem: Option<String>,
}

/// OS keychain: Keychain (macOS), Credential Manager (Windows), Secret Service (Linux).
pub struct KeyringStore;

impl KeyringStore {
  fn entry(key: &SecretKey) -> Result<keyring::Entry, Error> {
    Ok(keyring::Entry::new(SERVICE, &key.storage_id())?)
  }
}

impl SecretStore for KeyringStore {
  fn set(&self, key: &SecretKey, secret: &str) -> Result<(), Error> {
    Self::entry(key)?.set_password(secret)?;
    Ok(())
  }

  fn get(&self, key: &SecretKey) -> Result<Option<String>, Error> {
    match Self::entry(key)?.get_password() {
      Ok(secret) => Ok(Some(secret)),
      Err(keyring::Error::NoEntry) => Ok(None),
      Err(err) => Err(err.into()),
    }
  }

  fn delete(&self, key: &SecretKey) -> Result<(), Error> {
    match Self::entry(key)?.delete_credential() {
      Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
      Err(err) => Err(err.into()),
    }
  }

  fn probe(&self) -> Result<(), Error> {
    // store_status reports the init itself: Entry::new would only say "no
    // default store", which hides why the platform backend refused.
    match keyring::Entry::store_status() {
      Ok(()) => Ok(()),
      Err(err) => {
        // The D-Bus detail is log material: in the form it buries the one
        // sentence that says what to do.
        log::warn!("keyring unavailable: {err}");
        Err(Error::Secret {
          message: KEYRING_MISSING.to_string(),
        })
      }
    }
  }
}

#[cfg(target_os = "linux")]
const KEYRING_MISSING: &str = "No keyring on this session. Install one that speaks Secret Service \
                               (gnome-keyring, kwallet, KeePassXC), or use \"Ask every time\" or \
                               \"From a command\".";

#[cfg(not(target_os = "linux"))]
const KEYRING_MISSING: &str =
  "The OS keychain did not answer. Use \"Ask every time\" or \"From a command\" to keep going.";

/// Plaintext secrets on disk. Explicit opt-in for keychain-less dev machines
/// (WSL) only: never the default, never shipped as one.
pub struct FileStore {
  path: std::path::PathBuf,
  secrets: std::sync::Mutex<std::collections::HashMap<String, String>>,
}

impl FileStore {
  pub fn load(path: std::path::PathBuf) -> Result<Self, Error> {
    let secrets = match std::fs::read_to_string(&path) {
      Ok(raw) => serde_json::from_str(&raw)?,
      Err(err) if err.kind() == std::io::ErrorKind::NotFound => Default::default(),
      Err(err) => return Err(err.into()),
    };
    Ok(Self {
      path,
      secrets: std::sync::Mutex::new(secrets),
    })
  }

  fn save(&self, secrets: &std::collections::HashMap<String, String>) -> Result<(), Error> {
    if let Some(dir) = self.path.parent() {
      std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&self.path, serde_json::to_string(secrets)?)?;
    #[cfg(unix)]
    {
      use std::os::unix::fs::PermissionsExt;
      std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
  }
}

impl SecretStore for FileStore {
  fn set(&self, key: &SecretKey, secret: &str) -> Result<(), Error> {
    let mut secrets = self.secrets.lock().unwrap();
    secrets.insert(key.storage_id(), secret.to_string());
    self.save(&secrets)
  }

  fn get(&self, key: &SecretKey) -> Result<Option<String>, Error> {
    Ok(self.secrets.lock().unwrap().get(&key.storage_id()).cloned())
  }

  fn delete(&self, key: &SecretKey) -> Result<(), Error> {
    let mut secrets = self.secrets.lock().unwrap();
    secrets.remove(&key.storage_id());
    self.save(&secrets)
  }
}

/// Ephemeral store for e2e/CI environments without an OS keychain.
#[derive(Default)]
pub struct InMemoryStore(std::sync::Mutex<std::collections::HashMap<String, String>>);

impl SecretStore for InMemoryStore {
  fn set(&self, key: &SecretKey, secret: &str) -> Result<(), Error> {
    self
      .0
      .lock()
      .unwrap()
      .insert(key.storage_id(), secret.to_string());
    Ok(())
  }

  fn get(&self, key: &SecretKey) -> Result<Option<String>, Error> {
    Ok(self.0.lock().unwrap().get(&key.storage_id()).cloned())
  }

  fn delete(&self, key: &SecretKey) -> Result<(), Error> {
    self.0.lock().unwrap().remove(&key.storage_id());
    Ok(())
  }
}

/// Keychain-less environments pick their store by env var: e2e/CI run
/// ephemeral, WSL dev opts into a plaintext file, everything else keyrings.
pub fn store_from_env(
  data_dir: &std::path::Path,
) -> Result<Box<dyn SecretStore>, crate::error::Error> {
  if std::env::var("SOQUEL_EPHEMERAL_SECRETS").is_ok() {
    return Ok(Box::new(InMemoryStore::default()));
  }
  if std::env::var("SOQUEL_INSECURE_FILE_SECRETS").is_ok() {
    return Ok(Box::new(FileStore::load(data_dir.join("secrets.json"))?));
  }
  Ok(Box::new(KeyringStore))
}

#[cfg(test)]
mod tests {
  use super::*;

  fn connection(id: &str) -> SecretKey {
    SecretKey::Connection(id.to_string())
  }

  #[test]
  fn set_get_delete_roundtrip() {
    let store = InMemoryStore::default();
    let key = connection("a");
    assert_eq!(store.get(&key).unwrap(), None);
    store.set(&key, "s3cret").unwrap();
    assert_eq!(store.get(&key).unwrap(), Some("s3cret".to_string()));
    store.delete(&key).unwrap();
    assert_eq!(store.get(&key).unwrap(), None);
  }

  #[test]
  fn a_tunnel_and_a_connection_of_the_same_id_are_two_secrets() {
    let store = InMemoryStore::default();
    store.set(&connection("a"), "db").unwrap();
    store
      .set(&SecretKey::Tunnel("a".to_string()), "bastion")
      .unwrap();

    assert_eq!(store.get(&connection("a")).unwrap(), Some("db".to_string()));
    assert_eq!(
      store.get(&SecretKey::Tunnel("a".to_string())).unwrap(),
      Some("bastion".to_string())
    );
  }

  #[test]
  fn each_kind_owns_its_namespace() {
    assert_eq!(connection("a").storage_id(), "connection:a");
    assert_eq!(SecretKey::Tunnel("a".to_string()).storage_id(), "tunnel:a");
    assert_eq!(SecretKey::McpToken.storage_id(), "mcp-token");
  }

  #[test]
  fn file_store_survives_reload() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("secrets.json");
    let key = connection("a");

    let store = FileStore::load(path.clone()).unwrap();
    store.set(&key, "s3cret").unwrap();

    let reloaded = FileStore::load(path.clone()).unwrap();
    assert_eq!(reloaded.get(&key).unwrap(), Some("s3cret".to_string()));

    reloaded.delete(&key).unwrap();
    let emptied = FileStore::load(path).unwrap();
    assert_eq!(emptied.get(&key).unwrap(), None);
  }
}
