//! MongoDB connector: single-node client (direct connection), document browse surface.

use std::sync::Arc;
use std::time::Duration;

use mongodb::bson::doc;
use mongodb::options::{ClientOptions, Credential, ServerAddress, Tls, TlsOptions};
use mongodb::Client;

use crate::connectors::{Capability, Connection, Connector, DocBrowse, LocalForward};
use crate::credentials::Credentials;
use crate::error::Error;
use crate::profiles::{ConnectionProfile, ConnectorParams};

mod browse;

/// find_docs page clamp.
pub const DOC_PAGE_MAX: u32 = 200;
/// Console result cap.
pub const QUERY_SAMPLE: usize = 200;
/// Filtered count scan bound.
pub const COUNT_CAP: u64 = 1_000_000;

pub struct MongoConnector;

#[async_trait::async_trait]
impl Connector for MongoConnector {
  fn capabilities(&self) -> &'static [Capability] {
    &[Capability::DocBrowse]
  }

  async fn connect(
    &self,
    profile: &ConnectionProfile,
    secret: Arc<Credentials>,
    forward: Option<LocalForward>,
  ) -> Result<Box<dyn Connection>, Error> {
    let ConnectorParams::Mongo(params) = &profile.params else {
      return Err(Error::Unsupported {
        message: "this connector needs a mongodb profile".to_string(),
      });
    };
    // The driver derives SNI and cert verification from the dialed host: through
    // a forward it would validate "127.0.0.1", and no override exists (3.8).
    if params.tls && forward.is_some() {
      return Err(Error::Unsupported {
        message: "TLS through an SSH tunnel is not supported for MongoDB yet".to_string(),
      });
    }
    let (host, port) = match forward {
      Some(forward) => ("127.0.0.1".to_string(), forward.port),
      None => (params.host.clone(), params.port),
    };

    let mut options = ClientOptions::builder()
      .hosts(vec![ServerAddress::Tcp {
        host,
        port: Some(port),
      }])
      // Single node v1: never discover and redial advertised replica-set
      // members (mandatory behind a tunnel).
      .direct_connection(true)
      .server_selection_timeout(Duration::from_secs(5))
      .app_name("soquel".to_string())
      .build();
    if params.tls {
      options.tls = Some(Tls::Enabled(TlsOptions::default()));
    }
    if let Some(username) = &params.username {
      let mut credential = Credential::builder().username(username.clone()).build();
      // The driver keeps the credential for the client's life: no refresh hook.
      credential.password = secret.resolve().await?;
      // URI semantics: authSource > path database > driver default (admin).
      credential.source = params
        .auth_source
        .clone()
        .or_else(|| params.database.clone());
      options.credential = Some(credential);
    }

    let client = Client::with_options(options)?;
    let admin = client.database("admin");
    // The client is lazy: this ping is where reachability/auth actually fail.
    admin.run_command(doc! { "ping": 1 }).await?;
    // Best effort: buildInfo can be restricted on hardened deployments.
    let server_version = admin
      .run_command(doc! { "buildInfo": 1 })
      .await
      .ok()
      .and_then(|info| info.get_str("version").ok().map(str::to_string));

    Ok(Box::new(MongoConnection {
      client,
      server_version,
      default_database: params.database.clone(),
    }))
  }
}

pub struct MongoConnection {
  /// Cheap to clone; all clones share one topology and pool.
  client: Client,
  server_version: Option<String>,
  /// Fallback listing when the user lacks listDatabases.
  default_database: Option<String>,
}

#[async_trait::async_trait]
impl Connection for MongoConnection {
  async fn health(&self) -> Result<(), Error> {
    self
      .client
      .database("admin")
      .run_command(doc! { "ping": 1 })
      .await?;
    Ok(())
  }

  async fn close(&self) -> Result<(), Error> {
    // No async Drop: an un-shutdown client leaves orphan cleanup tasks. Immediate
    // because stateless pagination holds no cursors, and an in-flight op on
    // another clone must not wedge disconnect.
    self.client.clone().shutdown().immediate(true).await;
    Ok(())
  }

  fn server_version(&self) -> Option<String> {
    self.server_version.clone()
  }

  fn doc(&self) -> Option<&dyn DocBrowse> {
    Some(self)
  }
}

#[cfg(test)]
mod tests;
