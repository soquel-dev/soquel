//! Trading a licence key for a signed licence file; the HTTP stays in the
//! core like every other capability.

use rustls_platform_verifier::BuilderVerifierExt;
use serde::Deserialize;

use crate::error::{ActivationReason, Error};

const ENDPOINT: &str = "https://releases.soquel.dev/activate";

/// Dev only, same rule as the updater: a shipped build is not redirectable.
#[cfg(debug_assertions)]
const ENDPOINT_ENV: &str = "SOQUEL_ACTIVATION_ENDPOINT";

const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

#[derive(Deserialize)]
struct Issued {
  licence: String,
}

/// What the service answers with on every refusal.
#[derive(Deserialize)]
struct Refusal {
  error: RefusalDetail,
}

#[derive(Deserialize)]
struct RefusalDetail {
  kind: String,
  message: String,
}

/// Pure so its test mutates no process env: losing that race reaches the real service.
#[cfg(debug_assertions)]
fn chosen_endpoint(configured: Option<String>) -> String {
  configured
    .map(|endpoint| endpoint.trim().to_string())
    .filter(|endpoint| !endpoint.is_empty())
    .unwrap_or_else(|| ENDPOINT.to_string())
}

#[cfg(debug_assertions)]
fn endpoint() -> String {
  chosen_endpoint(std::env::var(ENDPOINT_ENV).ok())
}

#[cfg(not(debug_assertions))]
fn endpoint() -> String {
  ENDPOINT.to_string()
}

/// The compile target and not the hostname: this only labels an activation in Polar.
fn platform() -> &'static str {
  std::env::consts::OS
}

fn offline(message: String) -> Error {
  Error::Activation {
    message,
    reason: ActivationReason::Offline,
  }
}

/// The body decides, the status is the fallback: the service answers 403 both for a
/// revoked key and for one belonging to another product, so nothing else tells them
/// apart.
fn refusal_of(status: u16, body: &str) -> (ActivationReason, String) {
  if let Ok(refusal) = serde_json::from_str::<Refusal>(body) {
    if let Some(reason) = reason_of(&refusal.error.kind) {
      return (reason, refusal.error.message);
    }
  }
  (
    reason_of_status(status),
    format!("the licence server answered {status}"),
  )
}

fn reason_of(kind: &str) -> Option<ActivationReason> {
  Some(match kind {
    "unknown_key" => ActivationReason::UnknownKey,
    "wrong_product" => ActivationReason::WrongProduct,
    "revoked" => ActivationReason::Revoked,
    "activation_limit" => ActivationReason::ActivationLimit,
    // invalid_request means this build sent a body the service refused, which is
    // our bug and not the buyer's. Retrying later is the only advice either way.
    "upstream_unavailable" | "invalid_request" => ActivationReason::UpstreamUnavailable,
    _ => return None,
  })
}

fn reason_of_status(status: u16) -> ActivationReason {
  match status {
    404 => ActivationReason::UnknownKey,
    // Of the two the body would have told apart, this is the one that does not
    // accuse the buyer of pasting a key that is not theirs.
    403 => ActivationReason::Revoked,
    409 => ActivationReason::ActivationLimit,
    _ => ActivationReason::UpstreamUnavailable,
  }
}

/// Returns the licence token, unverified: `licence::install` checks the signature
/// before it replaces anything on disk.
pub async fn activate(key: &str) -> Result<String, Error> {
  activate_at(&endpoint(), key).await
}

/// The endpoint is a parameter so no test can reach the real service by accident,
/// the same reason the validator takes its public key as one.
async fn activate_at(endpoint: &str, key: &str) -> Result<String, Error> {
  // Handed a config rather than left to build its own: reqwest is pinned to
  // `rustls-no-provider` so the graph keeps one crypto provider, and it panics
  // outright if a client is built without one.
  let tls = rustls::ClientConfig::builder()
    .with_platform_verifier()
    .map_err(|err| offline(format!("tls setup: {err}")))?
    .with_no_client_auth();

  // Built per call rather than held in state: this runs once when someone pastes a
  // key, never on a hot path.
  let client = reqwest::Client::builder()
    .use_preconfigured_tls(tls)
    .timeout(TIMEOUT)
    .build()
    .map_err(|err| offline(err.to_string()))?;

  let response = client
    .post(endpoint)
    .json(&serde_json::json!({ "key": key, "platform": platform() }))
    .send()
    .await
    .map_err(|err| offline(err.to_string()))?;

  let status = response.status();
  let body = response.text().await.unwrap_or_default();

  if !status.is_success() {
    let (reason, message) = refusal_of(status.as_u16(), &body);
    return Err(Error::Activation { message, reason });
  }

  serde_json::from_str::<Issued>(&body)
    .map(|issued| issued.licence)
    .map_err(|_| Error::Activation {
      message: "the licence server sent something this build cannot read".to_string(),
      reason: ActivationReason::UpstreamUnavailable,
    })
}

#[cfg(test)]
mod tests {
  use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

  use super::*;

  fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
      .enable_all()
      .build()
      .unwrap()
  }

  fn http(status: u16, body: &str) -> String {
    format!(
      "HTTP/1.1 {status} OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
      body.len()
    )
  }

  /// Reads to the end of the declared body: reqwest may put the headers and the JSON
  /// on the wire separately, and asserting on a partial read would fail at random.
  async fn read_request(socket: &mut tokio::net::TcpStream) -> String {
    let mut request = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
      let read = socket.read(&mut chunk).await.unwrap();
      if read == 0 {
        break;
      }
      request.extend_from_slice(&chunk[..read]);
      let text = String::from_utf8_lossy(&request);
      let Some((headers, body)) = text.split_once("\r\n\r\n") else {
        continue;
      };
      let declared = headers
        .lines()
        .find_map(|line| {
          let (name, value) = line.split_once(':')?;
          name
            .eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);
      if body.len() >= declared {
        break;
      }
    }
    String::from_utf8_lossy(&request).to_string()
  }

  /// One request, one canned answer, and the raw request handed back so a test can
  /// see what actually went out.
  async fn served(response: String, key: &str) -> (Result<String, Error>, String) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/activate", listener.local_addr().unwrap());
    let stub = tokio::spawn(async move {
      let (mut socket, _) = listener.accept().await.unwrap();
      let request = read_request(&mut socket).await;
      socket.write_all(response.as_bytes()).await.unwrap();
      socket.flush().await.unwrap();
      request
    });
    let issued = activate_at(&url, key).await;
    (issued, stub.await.unwrap())
  }

  fn refusal(kind: &str) -> String {
    format!(r#"{{"error":{{"kind":"{kind}","message":"from the service"}}}}"#)
  }

  #[test]
  fn each_refusal_the_service_names_keeps_its_own_reason() {
    for (kind, expected) in [
      ("unknown_key", ActivationReason::UnknownKey),
      ("wrong_product", ActivationReason::WrongProduct),
      ("revoked", ActivationReason::Revoked),
      ("activation_limit", ActivationReason::ActivationLimit),
      (
        "upstream_unavailable",
        ActivationReason::UpstreamUnavailable,
      ),
    ] {
      let (reason, message) = refusal_of(403, &refusal(kind));
      assert_eq!(reason, expected, "{kind}");
      assert_eq!(message, "from the service", "{kind}");
    }
  }

  #[test]
  fn the_two_that_share_a_status_are_told_apart_by_the_body() {
    // 403 carries both. Reading the status alone would report one of them as the
    // other, and they call for opposite things from the buyer.
    assert_eq!(
      refusal_of(403, &refusal("wrong_product")).0,
      ActivationReason::WrongProduct
    );
    assert_eq!(
      refusal_of(403, &refusal("revoked")).0,
      ActivationReason::Revoked
    );
  }

  #[test]
  fn an_unreadable_body_falls_back_to_the_status() {
    // What a proxy or a gateway in front of the service would send.
    for (status, expected) in [
      (404, ActivationReason::UnknownKey),
      (403, ActivationReason::Revoked),
      (409, ActivationReason::ActivationLimit),
      (503, ActivationReason::UpstreamUnavailable),
      (502, ActivationReason::UpstreamUnavailable),
    ] {
      assert_eq!(
        refusal_of(status, "<html>nope</html>").0,
        expected,
        "{status}"
      );
    }
  }

  #[test]
  fn a_kind_this_build_does_not_know_falls_back_rather_than_guessing() {
    let (reason, message) = refusal_of(418, &refusal("something_newer"));

    assert_eq!(reason, ActivationReason::UpstreamUnavailable);
    assert!(message.contains("418"), "{message}");
  }

  #[cfg(debug_assertions)]
  #[test]
  fn a_blank_endpoint_override_is_ignored() {
    assert_eq!(chosen_endpoint(None), ENDPOINT);
    assert_eq!(chosen_endpoint(Some("  ".to_string())), ENDPOINT);
    assert_eq!(
      chosen_endpoint(Some(" http://127.0.0.1:3003/activate ".to_string())),
      "http://127.0.0.1:3003/activate"
    );
  }

  #[test]
  fn a_service_that_issues_a_licence_hands_its_token_back() {
    let body = r#"{"licence":"cGF5bG9hZA.c2ln"}"#;

    let (issued, request) = runtime().block_on(served(http(200, body), "SOQUEL-1234"));

    assert_eq!(issued.unwrap(), "cGF5bG9hZA.c2ln");
    // The service takes the key and the platform, and nothing else about the
    // machine goes out with them.
    assert!(request.contains("POST /activate"), "{request}");
    assert!(request.contains(r#""key":"SOQUEL-1234""#), "{request}");
    assert!(
      request.contains(&format!(r#""platform":"{}""#, platform())),
      "{request}"
    );
  }

  #[test]
  fn a_success_this_build_cannot_read_is_not_a_licence() {
    // A 200 carrying anything else must not reach the installer as a token.
    let (issued, _) = runtime().block_on(served(http(200, r#"{"nope":1}"#), "SOQUEL-1234"));

    assert!(matches!(
      issued.unwrap_err(),
      Error::Activation {
        reason: ActivationReason::UpstreamUnavailable,
        ..
      }
    ));
  }

  #[test]
  fn a_refusal_yields_no_token_at_all() {
    // What keeps a working licence on disk: the command installs what this returns,
    // so a refusal has to come back empty-handed rather than with something.
    let (issued, _) = runtime().block_on(served(http(403, &refusal("revoked")), "SOQUEL-1234"));

    assert!(matches!(
      issued.unwrap_err(),
      Error::Activation {
        reason: ActivationReason::Revoked,
        ..
      }
    ));
  }

  #[test]
  fn a_request_that_goes_nowhere_reads_as_offline() {
    // Nothing listens on port 9: the transport fails, which is a different thing
    // for the user than a service that answered.
    let refused = runtime()
      .block_on(activate_at("http://127.0.0.1:9/activate", "SOQUEL-0000"))
      .unwrap_err();

    assert!(
      matches!(
        refused,
        Error::Activation {
          reason: ActivationReason::Offline,
          ..
        }
      ),
      "{refused:?}"
    );
  }

  #[test]
  fn the_platform_is_one_the_service_accepts() {
    // The service takes macos, windows or linux; anything else would come back as
    // invalid_request, which is no help to whoever is holding a key.
    assert!(
      matches!(platform(), "macos" | "windows" | "linux"),
      "{}",
      platform()
    );
  }
}
