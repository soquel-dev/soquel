//! Offline licence validation. The format is specified in `docs/licence-format.md`,
//! published so a buyer can check their own file rather than trust ours.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::error::Error;

/// The public half of the licence signing key, distinct from the updater's.
const PUBLIC_KEY: &str = "mUac/9bOAvFXbUa/lZd5k3qoRjV6O09T1IVuge/rjLk=";

/// When this binary was built, stamped by `build.rs`. The window is compared
/// against it, not against the wall clock: moving the system clock forward must
/// not switch a licence off.
const BUILT: &str = env!("SOQUEL_BUILD_DATE");

/// Bumped only when a change would confuse an older validator. An unknown value
/// is refused rather than guessed at.
const SUPPORTED_VERSION: u32 = 1;

/// Dev only, like the endpoint overrides: an e2e run needs more tabs than the free
/// tier opens, and a shipped build must not be unlockable by an environment variable.
#[cfg(debug_assertions)]
const TAB_LIMIT_ENV: &str = "SOQUEL_TAB_LIMIT";

/// Raises the free tier's limit without claiming a licence: `read` still answers
/// `Free`, so the dialog keeps telling the truth about what is installed.
#[cfg(debug_assertions)]
pub fn tab_limit_override() -> Option<u32> {
  chosen_tab_limit(std::env::var(TAB_LIMIT_ENV).ok())
}

#[cfg(not(debug_assertions))]
pub fn tab_limit_override() -> Option<u32> {
  None
}

/// Pure so its test mutates no process env, and a value that is not a number is
/// ignored rather than read as zero, which would open nothing at all.
#[cfg(debug_assertions)]
fn chosen_tab_limit(configured: Option<String>) -> Option<u32> {
  configured
    .and_then(|limit| limit.trim().parse::<u32>().ok())
    .filter(|limit| *limit > 0)
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Payload {
  v: u32,
  #[allow(dead_code)]
  id: String,
  email: String,
  name: Option<String>,
  #[allow(dead_code)]
  issued: String,
  updates_until: String,
}

/// Three states, not two: a lapsed window looks exactly like no licence at all
/// unless it says so, and that is what gets reported as a regression.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
  tag = "kind",
  rename_all = "kebab-case",
  rename_all_fields = "camelCase"
)]
pub enum LicenceStatus {
  Free,
  Licensed {
    email: String,
    name: Option<String>,
    updates_until: String,
  },
  /// Signature is good, the window closed before this build was made.
  Expired {
    email: String,
    updates_until: String,
  },
}

fn verifying_key(encoded: &str) -> Result<VerifyingKey, Error> {
  let bytes: [u8; 32] = base64::engine::general_purpose::STANDARD
    .decode(encoded)
    .ok()
    .and_then(|raw| raw.try_into().ok())
    .ok_or_else(|| Error::Secret {
      message: "licence public key is not 32 bytes".to_string(),
    })?;
  VerifyingKey::from_bytes(&bytes).map_err(|err| Error::Secret {
    message: format!("licence public key: {err}"),
  })
}

fn invalid(message: &str) -> Error {
  Error::Secret {
    message: message.to_string(),
  }
}

/// Verifies against the bytes as stored. Parsing first and re-serialising is how
/// this kind of format breaks: key order and spacing differ between languages,
/// and the signature stops matching for reasons nobody can see.
fn payload_of(token: &str, key: &VerifyingKey) -> Result<Payload, Error> {
  let (encoded, encoded_signature) = token
    .trim()
    .split_once('.')
    .ok_or_else(|| invalid("that does not look like a licence: expected two parts"))?;

  let signed = URL_SAFE_NO_PAD
    .decode(encoded)
    .map_err(|_| invalid("the licence is not readable"))?;
  // A licence is a long unbroken string, so losing characters off the end is by
  // far the likeliest paste failure. Saying "malformed" leaves the user hunting
  // for a problem that a second copy would fix.
  let signature: [u8; 64] = URL_SAFE_NO_PAD
    .decode(encoded_signature)
    .ok()
    .and_then(|raw| raw.try_into().ok())
    .ok_or_else(|| {
      invalid("this licence looks cut short: copy all of it, to the last character")
    })?;

  key
    .verify(&signed, &Signature::from_bytes(&signature))
    .map_err(|_| invalid("this licence was not issued for soquel"))?;

  let payload: Payload =
    serde_json::from_slice(&signed).map_err(|_| invalid("the licence is not readable"))?;

  if payload.v != SUPPORTED_VERSION {
    return Err(invalid(
      "this licence needs a newer soquel: update, then add it again",
    ));
  }

  Ok(payload)
}

pub fn status_of(token: &str, built: &str, key_base64: &str) -> Result<LicenceStatus, Error> {
  let payload = payload_of(token, &verifying_key(key_base64)?)?;

  // Both are RFC 3339 in UTC, so ordering is the string ordering.
  if payload.updates_until.as_str() < built {
    return Ok(LicenceStatus::Expired {
      email: payload.email,
      updates_until: payload.updates_until,
    });
  }

  Ok(LicenceStatus::Licensed {
    email: payload.email,
    name: payload.name,
    updates_until: payload.updates_until,
  })
}

/// A file that no longer verifies reads as no licence: it is not an error the
/// user can act on at startup, and refusing to launch over it would be absurd.
pub fn read(path: &std::path::Path) -> LicenceStatus {
  let Ok(token) = std::fs::read_to_string(path) else {
    return LicenceStatus::Free;
  };
  status_of(&token, BUILT, PUBLIC_KEY).unwrap_or(LicenceStatus::Free)
}

/// Where the installed token lives, under the app data dir. One place so both
/// frontends read and write the same file.
pub fn path(data_dir: &std::path::Path) -> std::path::PathBuf {
  data_dir.join("licence.txt")
}

/// Validated before it is written: a bad paste must not replace a working licence.
pub fn install(path: &std::path::Path, token: &str) -> Result<LicenceStatus, Error> {
  install_with(path, token, BUILT, PUBLIC_KEY)
}

fn install_with(
  path: &std::path::Path,
  token: &str,
  built: &str,
  key_base64: &str,
) -> Result<LicenceStatus, Error> {
  let status = status_of(token, built, key_base64)?;
  if let Some(dir) = path.parent() {
    std::fs::create_dir_all(dir)?;
  }
  std::fs::write(path, token.trim())?;
  Ok(status)
}

#[cfg(test)]
mod tests {
  use base64::engine::general_purpose::STANDARD;
  use ed25519_dalek::{Signer, SigningKey};

  use super::*;

  const BUILT_AT: &str = "2026-08-02T00:00:00Z";

  fn keypair(seed: u8) -> (SigningKey, String) {
    let signing = SigningKey::from_bytes(&[seed; 32]);
    let public = STANDARD.encode(signing.verifying_key().to_bytes());
    (signing, public)
  }

  fn token_with(signing: &SigningKey, payload: &str) -> String {
    let signature = signing.sign(payload.as_bytes());
    format!(
      "{}.{}",
      URL_SAFE_NO_PAD.encode(payload),
      URL_SAFE_NO_PAD.encode(signature.to_bytes())
    )
  }

  fn payload(version: u32, until: &str) -> String {
    format!(
      r#"{{"v":{version},"id":"lic_1","email":"buyer@example.com","name":"Buyer","issued":"2026-01-01T00:00:00Z","updatesUntil":"{until}"}}"#
    )
  }

  #[test]
  fn a_licence_covering_this_build_unlocks_it() {
    let (signing, public) = keypair(7);
    let token = token_with(&signing, &payload(1, "2027-01-01T00:00:00Z"));

    let status = status_of(&token, BUILT_AT, &public).unwrap();

    assert!(
      matches!(status, LicenceStatus::Licensed { email, .. } if email == "buyer@example.com")
    );
  }

  #[test]
  fn a_window_that_closed_before_this_build_says_so_instead_of_going_quiet() {
    let (signing, public) = keypair(7);
    let token = token_with(&signing, &payload(1, "2026-01-01T00:00:00Z"));

    let status = status_of(&token, BUILT_AT, &public).unwrap();

    // Free and Expired both lock the app; only one of them explains why, and
    // the other gets reported as a bug.
    assert_eq!(
      status,
      LicenceStatus::Expired {
        email: "buyer@example.com".to_string(),
        updates_until: "2026-01-01T00:00:00Z".to_string(),
      }
    );
  }

  #[test]
  fn a_build_released_exactly_on_the_last_day_is_covered() {
    let (signing, public) = keypair(7);
    let token = token_with(&signing, &payload(1, BUILT_AT));

    // The boundary belongs to the buyer: they paid for that day.
    let status = status_of(&token, BUILT_AT, &public).unwrap();
    assert!(
      matches!(status, LicenceStatus::Licensed { .. }),
      "{status:?}"
    );
  }

  #[test]
  fn one_flipped_byte_in_the_payload_invalidates_it() {
    let (signing, public) = keypair(7);
    let honest = payload(1, "2027-01-01T00:00:00Z");
    let token = token_with(&signing, &honest);

    // Re-encode a longer window against the original signature.
    let forged = format!(
      "{}.{}",
      URL_SAFE_NO_PAD.encode(payload(1, "2099-01-01T00:00:00Z")),
      token.split_once('.').unwrap().1
    );

    assert!(status_of(&forged, BUILT_AT, &public).is_err());
  }

  #[test]
  fn a_licence_signed_by_another_key_is_refused() {
    let (theirs, _) = keypair(9);
    let (_, ours) = keypair(7);
    let token = token_with(&theirs, &payload(1, "2027-01-01T00:00:00Z"));

    assert!(status_of(&token, BUILT_AT, &ours).is_err());
  }

  #[test]
  fn an_unknown_format_version_asks_for_a_newer_soquel() {
    let (signing, public) = keypair(7);
    let token = token_with(&signing, &payload(2, "2027-01-01T00:00:00Z"));

    // Guessing at a format this build does not know is how a validator starts
    // accepting things it cannot actually check.
    let message = status_of(&token, BUILT_AT, &public)
      .unwrap_err()
      .to_string();
    assert!(message.contains("newer soquel"), "{message}");
  }

  #[test]
  fn a_licence_missing_its_last_character_says_it_was_cut_short() {
    let (signing, public) = keypair(7);
    let token = token_with(&signing, &payload(1, "2027-01-01T00:00:00Z"));
    let truncated = &token[..token.len() - 1];

    // The likeliest way a paste fails, and the one worth naming: every other
    // message sends someone looking for a problem they do not have.
    let message = status_of(truncated, BUILT_AT, &public)
      .unwrap_err()
      .to_string();
    assert!(message.contains("cut short"), "{message}");
  }

  #[test]
  fn a_rejected_paste_leaves_the_licence_that_was_already_there() {
    let (signing, public) = keypair(7);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("licence.txt");
    let good = token_with(&signing, &payload(1, "2027-01-01T00:00:00Z"));
    install_with(&path, &good, BUILT_AT, &public).unwrap();

    // Someone pasting over a working licence and getting it wrong must not lose
    // the one they paid for.
    assert!(install_with(&path, "rubbish", BUILT_AT, &public).is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), good);
  }

  #[test]
  fn anything_that_is_not_a_token_is_refused_without_panicking() {
    let (_, public) = keypair(7);

    for junk in ["", "not-a-licence", "only.one", "...", "a.b.c"] {
      assert!(status_of(junk, BUILT_AT, &public).is_err(), "{junk}");
    }
  }

  #[cfg(debug_assertions)]
  #[test]
  fn a_tab_limit_override_only_takes_a_usable_number() {
    assert_eq!(chosen_tab_limit(None), None);
    assert_eq!(chosen_tab_limit(Some(" 20 ".to_string())), Some(20));
    // Zero would open no tabs at all, and a word would read as zero if parsed
    // loosely: neither is a limit anybody meant to set.
    assert_eq!(chosen_tab_limit(Some("0".to_string())), None);
    assert_eq!(chosen_tab_limit(Some("lots".to_string())), None);
    assert_eq!(chosen_tab_limit(Some(String::new())), None);
  }

  #[test]
  fn the_shipped_public_key_is_a_usable_ed25519_key() {
    // A typo in the constant would only surface the day someone pastes a licence.
    assert!(verifying_key(PUBLIC_KEY).is_ok());
  }
}
