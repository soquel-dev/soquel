use serde::{Deserialize, Serialize};
use specta::Type;

/// What a prompt is asking for; drives the dialog's wording.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "kebab-case")]
pub enum SecretSubject {
  Connection,
  Tunnel,
}

/// Why an activation was refused. Each one asks something different of the buyer,
/// which is the whole reason the licence service distinguishes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "kebab-case")]
pub enum ActivationReason {
  /// The request never got an answer: no network, DNS, TLS, or a timeout.
  Offline,
  UnknownKey,
  WrongProduct,
  Revoked,
  ActivationLimit,
  /// The service answered, but could not reach Polar. The only one where nothing
  /// is wrong with the purchase.
  UpstreamUnavailable,
}

/// Normalized error shape crossing the IPC boundary.
#[derive(Debug, thiserror::Error, Serialize, Type)]
#[serde(
  tag = "kind",
  rename_all = "kebab-case",
  rename_all_fields = "camelCase"
)]
pub enum Error {
  #[error("{message}")]
  NotFound { message: String },
  #[error("{message}")]
  Storage { message: String },
  #[error("{message}")]
  Secret { message: String },
  #[error("{message}")]
  Unsupported { message: String },
  #[error("{message}")]
  Database { message: String },
  #[error("{message}")]
  Tunnel { message: String },
  #[error("{message}")]
  HostKeyUntrusted {
    message: String,
    host: String,
    port: u16,
    fingerprint: String,
    key: String,
    previously_trusted: bool,
  },
  /// The profile asks for its password interactively; the caller must supply one.
  #[error("{message}")]
  SecretRequired {
    message: String,
    subject: SecretSubject,
    target_id: String,
    target_name: String,
  },
  #[error("{message}")]
  CredentialCommand {
    message: String,
    program: String,
    stderr: String,
  },
  #[error("{message}")]
  Update { message: String },
  /// `reason` rather than `kind`: that name is already the tag of this enum.
  #[error("{message}")]
  Activation {
    message: String,
    reason: ActivationReason,
  },
  /// A credential command nobody agreed to run yet: it arrived with an import.
  #[error("{message}")]
  CommandApprovalRequired {
    message: String,
    subject: SecretSubject,
    target_id: String,
    target_name: String,
    program: String,
    args: Vec<String>,
  },
}

impl From<tokio_postgres::Error> for Error {
  fn from(err: tokio_postgres::Error) -> Self {
    // db_error carries the server message; otherwise walk the source chain,
    // where the actual cause lives ("invalid configuration" alone says nothing).
    let message = err
      .as_db_error()
      .map(|db| db.message().to_string())
      .unwrap_or_else(|| {
        let mut message = err.to_string();
        let mut source = std::error::Error::source(&err);
        while let Some(cause) = source {
          message.push_str(&format!(": {cause}"));
          source = cause.source();
        }
        message
      });
    Error::Database { message }
  }
}

impl From<mysql_async::Error> for Error {
  fn from(err: mysql_async::Error) -> Self {
    // Server errors carry the useful message; wrap the rest verbatim.
    let message = match err {
      mysql_async::Error::Server(server) => server.message,
      other => other.to_string(),
    };
    Error::Database { message }
  }
}

impl From<redis::RedisError> for Error {
  fn from(err: redis::RedisError) -> Self {
    Error::Database {
      message: err.to_string(),
    }
  }
}

impl From<mongodb::error::Error> for Error {
  fn from(err: mongodb::error::Error) -> Self {
    // Command errors carry the server message; wrap the rest verbatim.
    let message = match &*err.kind {
      mongodb::error::ErrorKind::Command(command) => command.message.clone(),
      _ => err.to_string(),
    };
    Error::Database { message }
  }
}

impl From<rusqlite::Error> for Error {
  fn from(err: rusqlite::Error) -> Self {
    Error::Database {
      message: err.to_string(),
    }
  }
}

impl From<std::io::Error> for Error {
  fn from(err: std::io::Error) -> Self {
    Error::Storage {
      message: err.to_string(),
    }
  }
}

impl From<serde_json::Error> for Error {
  fn from(err: serde_json::Error) -> Self {
    Error::Storage {
      message: err.to_string(),
    }
  }
}

impl From<keyring::Error> for Error {
  fn from(err: keyring::Error) -> Self {
    Error::Secret {
      message: format!("keychain: {err}"),
    }
  }
}
