use base64::Engine as _;
use chacha20poly1305::aead::rand_core::RngCore;
use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use serde::{Deserialize, Serialize};
use specta::Type;

use crate::error::Error;

const KDF: &str = "argon2id";
const CIPHER: &str = "xchacha20poly1305";
const SALT_LEN: usize = 16;
const KEY_LEN: usize = 32;

const BASE64: base64::engine::general_purpose::GeneralPurpose =
  base64::engine::general_purpose::STANDARD;

/// Everything needed to re-derive the key, minus the passphrase. Doubles as the
/// AEAD associated data, so tampering with the cost parameters breaks the open.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct Encryption {
  pub kdf: String,
  pub memory_kib: u32,
  pub iterations: u32,
  pub parallelism: u32,
  pub salt: String,
  pub cipher: String,
  pub nonce: String,
}

fn secret_error(message: &str) -> Error {
  Error::Secret {
    message: message.to_string(),
  }
}

fn derive(passphrase: &str, header: &Encryption) -> Result<Key, Error> {
  let salt = BASE64
    .decode(&header.salt)
    .map_err(|_| secret_error("the encryption header is corrupted (salt)"))?;
  let params = argon2::Params::new(
    header.memory_kib,
    header.iterations,
    header.parallelism,
    Some(KEY_LEN),
  )
  .map_err(|err| secret_error(&format!("unsupported argon2 parameters: {err}")))?;
  let argon = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
  let mut key = [0u8; KEY_LEN];
  argon
    .hash_password_into(passphrase.as_bytes(), &salt, &mut key)
    .map_err(|err| secret_error(&format!("key derivation failed: {err}")))?;
  Ok(Key::from(key))
}

/// Base64 ciphertext plus the header describing how to get back in.
pub fn seal(passphrase: &str, plaintext: &[u8]) -> Result<(Encryption, String), Error> {
  let mut salt = [0u8; SALT_LEN];
  OsRng.fill_bytes(&mut salt);
  let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
  let defaults = argon2::Params::default();
  let header = Encryption {
    kdf: KDF.to_string(),
    memory_kib: defaults.m_cost(),
    iterations: defaults.t_cost(),
    parallelism: defaults.p_cost(),
    salt: BASE64.encode(salt),
    cipher: CIPHER.to_string(),
    nonce: BASE64.encode(nonce),
  };
  let key = derive(passphrase, &header)?;
  let ciphertext = XChaCha20Poly1305::new(&key)
    .encrypt(
      &nonce,
      Payload {
        msg: plaintext,
        aad: &aad(&header)?,
      },
    )
    .map_err(|_| secret_error("encryption failed"))?;
  Ok((header, BASE64.encode(ciphertext)))
}

pub fn open(passphrase: &str, header: &Encryption, ciphertext: &str) -> Result<Vec<u8>, Error> {
  if header.kdf != KDF || header.cipher != CIPHER {
    return Err(secret_error(&format!(
      "unsupported encryption ({}/{}); this file was not written by soquel",
      header.kdf, header.cipher
    )));
  }
  let nonce = BASE64
    .decode(&header.nonce)
    .map_err(|_| secret_error("the encryption header is corrupted (nonce)"))?;
  if nonce.len() != 24 {
    return Err(secret_error("the encryption header is corrupted (nonce)"));
  }
  let ciphertext = BASE64
    .decode(ciphertext)
    .map_err(|_| secret_error("the encrypted payload is corrupted"))?;
  let key = derive(passphrase, header)?;
  XChaCha20Poly1305::new(&key)
    .decrypt(
      XNonce::from_slice(&nonce),
      Payload {
        msg: &ciphertext,
        aad: &aad(header)?,
      },
    )
    .map_err(|_| secret_error("wrong passphrase, or the file has been tampered with"))
}

// Serialized from the parsed struct, not the raw text: reformatting the file
// keeps the same bytes, editing a cost parameter does not.
fn aad(header: &Encryption) -> Result<Vec<u8>, Error> {
  Ok(serde_json::to_vec(header)?)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn seal_open_roundtrip() {
    let (header, payload) = seal("correct horse", b"secret document").unwrap();
    assert_eq!(header.kdf, KDF);
    assert!(!payload.contains("secret"));
    assert_eq!(
      open("correct horse", &header, &payload).unwrap(),
      b"secret document"
    );
  }

  #[test]
  fn wrong_passphrase_is_a_clear_error() {
    let (header, payload) = seal("right", b"doc").unwrap();
    let err = open("wrong", &header, &payload).unwrap_err();
    assert!(
      matches!(&err, Error::Secret { message } if message.contains("wrong passphrase")),
      "{err:?}"
    );
  }

  #[test]
  fn a_tampered_header_breaks_the_open() {
    let (mut header, payload) = seal("right", b"doc").unwrap();
    header.iterations += 1;
    assert!(open("right", &header, &payload).is_err());
  }

  #[test]
  fn each_seal_uses_a_fresh_salt_and_nonce() {
    let (first, first_payload) = seal("same", b"doc").unwrap();
    let (second, second_payload) = seal("same", b"doc").unwrap();
    assert_ne!(first.salt, second.salt);
    assert_ne!(first.nonce, second.nonce);
    assert_ne!(first_payload, second_payload);
  }

  #[test]
  fn a_foreign_cipher_is_refused_by_name() {
    let (mut header, payload) = seal("right", b"doc").unwrap();
    header.cipher = "aes-gcm".to_string();
    let err = open("right", &header, &payload).unwrap_err();
    assert!(
      matches!(&err, Error::Secret { message } if message.contains("unsupported encryption")),
      "{err:?}"
    );
  }
}
