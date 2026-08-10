use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::profiles::{ConnectionProfile, ConnectorParams, CredentialSource, Env};
use crate::transfer::crypto::{self, Encryption};
use crate::transfer::{ImportBundle, IncomingConnection, IncomingTunnel};
use crate::tunnels::{SshAuth, TunnelProfile};

/// Bump only for a shape the current reader cannot make sense of; a newer
/// version is refused rather than guessed at.
pub const CURRENT_VERSION: u32 = 1;

const MAGIC: &str = "soquel-connections";

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Envelope {
  soquel: String,
  version: u32,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  encryption: Option<Encryption>,
  /// Present on a plaintext file.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  document: Option<Document>,
  /// Base64 ciphertext of the document; present on an encrypted file.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  payload: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Document {
  connections: Vec<FileConnection>,
  tunnels: Vec<FileTunnel>,
}

/// An `agentAccess` field in a file is deliberately not part of this shape:
/// serde drops it, and the engine writes `none`.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileConnection {
  name: String,
  env: Env,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  group: Option<String>,
  /// How the password is obtained, never the password itself. Absent in files
  /// written before the modes existed: those connections read from the keychain.
  #[serde(default)]
  credential: CredentialSource,
  params: ConnectorParams,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  secret: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileTunnel {
  /// File-local key the connections' `tunnelId` points at.
  id: String,
  name: String,
  host: String,
  port: u16,
  user: String,
  auth: SshAuth,
  /// How the ssh password or key passphrase is obtained, never the secret itself.
  #[serde(default)]
  credential: CredentialSource,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  secret: Option<String>,
}

/// A profile plus the secret to ship with it (None keeps the file shareable).
pub struct ExportEntry<'a> {
  pub profile: &'a ConnectionProfile,
  pub secret: Option<String>,
}

pub struct ExportTunnel<'a> {
  pub tunnel: &'a TunnelProfile,
  pub secret: Option<String>,
}

/// Serializes and, with a passphrase, encrypts before anything reaches the
/// disk: a plaintext secret never exists as a file, not even briefly.
pub fn write(
  path: &Path,
  connections: Vec<ExportEntry<'_>>,
  tunnels: Vec<ExportTunnel<'_>>,
  passphrase: Option<&str>,
) -> Result<(), Error> {
  let document = Document {
    connections: connections
      .into_iter()
      .map(|entry| FileConnection {
        name: entry.profile.name.clone(),
        env: entry.profile.env,
        group: entry.profile.group.clone(),
        credential: entry.profile.credential.clone(),
        params: entry.profile.params.clone(),
        secret: entry.secret,
      })
      .collect(),
    tunnels: tunnels
      .into_iter()
      .map(|entry| FileTunnel {
        id: entry.tunnel.id.clone(),
        name: entry.tunnel.name.clone(),
        host: entry.tunnel.host.clone(),
        port: entry.tunnel.port,
        user: entry.tunnel.user.clone(),
        auth: entry.tunnel.auth.clone(),
        credential: entry.tunnel.credential.clone(),
        secret: entry.secret,
      })
      .collect(),
  };
  let envelope = match passphrase {
    Some(passphrase) => {
      let (encryption, payload) = crypto::seal(passphrase, &serde_json::to_vec(&document)?)?;
      Envelope {
        soquel: MAGIC.to_string(),
        version: CURRENT_VERSION,
        encryption: Some(encryption),
        document: None,
        payload: Some(payload),
      }
    }
    None => Envelope {
      soquel: MAGIC.to_string(),
      version: CURRENT_VERSION,
      encryption: None,
      document: Some(document),
      payload: None,
    },
  };
  if let Some(dir) = path.parent() {
    fs::create_dir_all(dir)?;
  }
  fs::write(path, serde_json::to_string_pretty(&envelope)?)?;
  Ok(())
}

/// `None` bundle means the file is encrypted and the passphrase is still missing.
pub struct ReadFile {
  pub encrypted: bool,
  pub bundle: Option<ImportBundle>,
}

pub fn read(path: &Path, passphrase: Option<&str>) -> Result<ReadFile, Error> {
  let raw = fs::read_to_string(path)?;
  let envelope: Envelope = serde_json::from_str(&raw).map_err(|err| Error::Storage {
    message: format!("this is not a soquel connections file: {err}"),
  })?;
  if envelope.soquel != MAGIC {
    return Err(Error::Storage {
      message: "this is not a soquel connections file".to_string(),
    });
  }
  if envelope.version > CURRENT_VERSION {
    return Err(Error::Unsupported {
      message: format!(
        "this file is version {} and soquel reads up to {CURRENT_VERSION}; update soquel to import it",
        envelope.version
      ),
    });
  }
  let document = match (&envelope.encryption, &envelope.payload) {
    (Some(encryption), Some(payload)) => {
      let Some(passphrase) = passphrase else {
        return Ok(ReadFile {
          encrypted: true,
          bundle: None,
        });
      };
      let plaintext = crypto::open(passphrase, encryption, payload)?;
      serde_json::from_slice(&plaintext).map_err(|err| Error::Storage {
        message: format!("the decrypted file is not readable: {err}"),
      })?
    }
    (None, None) => envelope.document.unwrap_or_default(),
    _ => {
      return Err(Error::Storage {
        message: "the file is encrypted but incomplete (missing payload or header)".to_string(),
      })
    }
  };
  Ok(ReadFile {
    encrypted: envelope.encryption.is_some(),
    bundle: Some(bundle_from(document)),
  })
}

fn bundle_from(document: Document) -> ImportBundle {
  ImportBundle {
    connections: document
      .connections
      .into_iter()
      .map(|entry| {
        let mut params = entry.params;
        // The file's tunnel id is a reference, not an identity: the engine
        // owns the mapping to whatever id the tunnel lands on here.
        let tunnel_ref = params.take_tunnel_id();
        IncomingConnection {
          name: entry.name,
          env: entry.env,
          group: entry.group,
          credential: entry.credential,
          params,
          tunnel_ref,
          secret: entry.secret,
        }
      })
      .collect(),
    tunnels: document
      .tunnels
      .into_iter()
      .map(|entry| IncomingTunnel {
        reference: entry.id,
        name: entry.name,
        host: entry.host,
        port: entry.port,
        user: entry.user,
        auth: entry.auth,
        credential: entry.credential,
        secret: entry.secret,
      })
      .collect(),
  }
}
