use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::ring;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use rustls_platform_verifier::BuilderVerifierExt;
use tokio_postgres_rustls::MakeRustlsConnect;

use crate::error::Error;
use crate::profiles::SslMode;

// Verification level is the verifier's job; the wire protocol only knows
// whether to attempt TLS.
pub fn config_ssl_mode(mode: SslMode) -> tokio_postgres::config::SslMode {
  match mode {
    SslMode::Disable => tokio_postgres::config::SslMode::Disable,
    SslMode::Prefer => tokio_postgres::config::SslMode::Prefer,
    SslMode::Require | SslMode::VerifyFull => tokio_postgres::config::SslMode::Require,
  }
}

pub fn connector(mode: SslMode, root_cert: Option<&str>) -> Result<MakeRustlsConnect, Error> {
  let config = match (mode, root_cert) {
    // libpq parity: everything below verify-full encrypts without verifying.
    (SslMode::Disable | SslMode::Prefer | SslMode::Require, _) => ClientConfig::builder()
      .dangerous()
      .with_custom_certificate_verifier(Arc::new(AcceptAllVerifier))
      .with_no_client_auth(),
    // sslrootcert: trust exactly the given CA bundle instead of the platform store.
    (SslMode::VerifyFull, Some(path)) => ClientConfig::builder()
      .with_root_certificates(root_store(path)?)
      .with_no_client_auth(),
    (SslMode::VerifyFull, None) => ClientConfig::builder()
      .with_platform_verifier()
      .map_err(|err| Error::Database {
        message: format!("tls setup: {err}"),
      })?
      .with_no_client_auth(),
  };
  Ok(MakeRustlsConnect::new(config))
}

fn root_store(path: &str) -> Result<rustls::RootCertStore, Error> {
  let tls_error = |detail: String| Error::Database {
    message: format!("ssl root cert {path}: {detail}"),
  };
  let file = std::fs::File::open(path).map_err(|err| tls_error(err.to_string()))?;
  let mut reader = std::io::BufReader::new(file);
  let mut store = rustls::RootCertStore::empty();
  for cert in rustls_pemfile::certs(&mut reader) {
    let cert = cert.map_err(|err| tls_error(err.to_string()))?;
    store.add(cert).map_err(|err| tls_error(err.to_string()))?;
  }
  if store.is_empty() {
    return Err(tls_error("no certificates found".to_string()));
  }
  Ok(store)
}

#[cfg(test)]
mod tests {
  use super::*;

  const TEST_CA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/test-tls/ca.crt");

  #[test]
  fn root_store_loads_the_committed_test_ca() {
    let store = root_store(TEST_CA).unwrap();
    assert_eq!(store.len(), 1);
  }

  #[test]
  fn root_store_reports_missing_file_with_the_path() {
    let Err(Error::Database { message }) = root_store("/nope/ca.pem") else {
      panic!("expected a missing file to fail");
    };
    assert!(message.contains("/nope/ca.pem"), "{message}");
  }

  #[test]
  fn root_store_rejects_files_without_certificates() {
    let dir = tempfile::tempdir().unwrap();
    let empty = dir.path().join("empty.pem");
    std::fs::write(&empty, "").unwrap();
    let Err(Error::Database { message }) = root_store(empty.to_str().unwrap()) else {
      panic!("expected an empty file to fail");
    };
    assert!(message.contains("no certificates found"), "{message}");

    // A key alone is not a trust root either.
    let key_only = dir.path().join("key.pem");
    std::fs::write(
      &key_only,
      "-----BEGIN PRIVATE KEY-----\nMAA=\n-----END PRIVATE KEY-----\n",
    )
    .unwrap();
    assert!(root_store(key_only.to_str().unwrap()).is_err());
  }

  #[test]
  fn root_store_rejects_malformed_pem() {
    let dir = tempfile::tempdir().unwrap();
    let bad = dir.path().join("bad.pem");
    std::fs::write(
      &bad,
      "-----BEGIN CERTIFICATE-----\nnot base64 at all!!!\n-----END CERTIFICATE-----\n",
    )
    .unwrap();
    assert!(root_store(bad.to_str().unwrap()).is_err());
  }
}

#[derive(Debug)]
struct AcceptAllVerifier;

impl ServerCertVerifier for AcceptAllVerifier {
  fn verify_server_cert(
    &self,
    _end_entity: &CertificateDer<'_>,
    _intermediates: &[CertificateDer<'_>],
    _server_name: &ServerName<'_>,
    _ocsp_response: &[u8],
    _now: UnixTime,
  ) -> Result<ServerCertVerified, rustls::Error> {
    Ok(ServerCertVerified::assertion())
  }

  fn verify_tls12_signature(
    &self,
    _message: &[u8],
    _cert: &CertificateDer<'_>,
    _dss: &DigitallySignedStruct,
  ) -> Result<HandshakeSignatureValid, rustls::Error> {
    Ok(HandshakeSignatureValid::assertion())
  }

  fn verify_tls13_signature(
    &self,
    _message: &[u8],
    _cert: &CertificateDer<'_>,
    _dss: &DigitallySignedStruct,
  ) -> Result<HandshakeSignatureValid, rustls::Error> {
    Ok(HandshakeSignatureValid::assertion())
  }

  fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
    ring::default_provider()
      .signature_verification_algorithms
      .supported_schemes()
  }
}
