//! Update checks and installs, kept behind the command layer like every other capability.

use serde::{Deserialize, Serialize};
use specta::Type;
use tauri::AppHandle;
use tauri_plugin_updater::{Update, UpdaterExt};
use tauri_specta::Event as _;

use soquel_core::error::Error;

// The From impl cannot live in core anymore (orphan rule), so map here.
fn update_error(err: tauri_plugin_updater::Error) -> Error {
  Error::Update {
    message: err.to_string(),
  }
}

/// Dev-only endpoint override: a release build must not be redirectable.
#[cfg(debug_assertions)]
const ENDPOINT_ENV: &str = "SOQUEL_UPDATE_ENDPOINT";

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInfo {
  pub version: String,
  pub current_version: String,
  pub notes: Option<String>,
  pub pub_date: Option<String>,
}

/// Emitted while the bundle downloads.
#[derive(Debug, Clone, Serialize, Deserialize, Type, tauri_specta::Event)]
#[serde(rename_all = "camelCase")]
pub struct UpdateProgress {
  pub downloaded: f64,
  /// Absent when the server sends no content-length.
  pub total: Option<f64>,
}

impl From<&Update> for UpdateInfo {
  fn from(update: &Update) -> Self {
    Self {
      version: update.version.clone(),
      current_version: update.current_version.clone(),
      notes: update.body.clone(),
      pub_date: update.date.and_then(format_pub_date),
    }
  }
}

/// RFC 3339 so the webview can hand it straight to `Date`.
fn format_pub_date(date: time::OffsetDateTime) -> Option<String> {
  date
    .format(&time::format_description::well_known::Rfc3339)
    .ok()
}

/// An empty value would blank the endpoint list instead of leaving it alone.
#[cfg(debug_assertions)]
fn endpoint_override() -> Option<String> {
  chosen_endpoint(std::env::var(ENDPOINT_ENV).ok())
}

/// Pure so its test mutates no process env: setenv beside a getenv on another
/// thread is undefined behaviour, and the suite runs its tests in parallel.
#[cfg(debug_assertions)]
fn chosen_endpoint(configured: Option<String>) -> Option<String> {
  configured.filter(|endpoint| !endpoint.trim().is_empty())
}

/// Roughly a hundred events over the download, whatever its size.
fn emit_step(total: Option<u64>) -> u64 {
  const FLOOR: u64 = 1 << 20;
  total.map_or(FLOOR, |total| (total / 100).max(FLOOR))
}

async fn pending(app: &AppHandle) -> Result<Option<Update>, Error> {
  #[cfg_attr(not(debug_assertions), allow(unused_mut))]
  let mut builder = app.updater_builder();
  #[cfg(debug_assertions)]
  if let Some(endpoint) = endpoint_override() {
    let url = endpoint
      .parse::<tauri::Url>()
      .map_err(|err| Error::Update {
        message: format!("invalid {ENDPOINT_ENV}: {err}"),
      })?;
    builder = builder.endpoints(vec![url]).map_err(update_error)?;
  }
  builder
    .build()
    .map_err(update_error)?
    .check()
    .await
    .map_err(update_error)
}

pub async fn check(app: &AppHandle) -> Result<Option<UpdateInfo>, Error> {
  // A dev build has no bundle to replace, so only the override makes a check
  // worth a network round trip.
  #[cfg(debug_assertions)]
  if endpoint_override().is_none() {
    return Ok(None);
  }
  Ok(pending(app).await?.as_ref().map(UpdateInfo::from))
}

/// Re-checks rather than holding the `Update` in state: one extra round trip
/// against a mutex and a lifecycle.
pub async fn install(app: AppHandle) -> Result<(), Error> {
  let Some(update) = pending(&app).await? else {
    return Err(Error::Update {
      message: "no update available".to_string(),
    });
  };
  let progress = app.clone();
  let mut downloaded = 0u64;
  let mut emitted = 0u64;
  update
    .download_and_install(
      move |chunk, total| {
        downloaded += chunk as u64;
        // One event per chunk floods the IPC channel, and the webview then sees
        // them out of order: the bar jumps ahead and falls back.
        if downloaded - emitted < emit_step(total) && Some(downloaded) != total {
          return;
        }
        emitted = downloaded;
        let _ = UpdateProgress {
          downloaded: downloaded as f64,
          total: total.map(|total| total as f64),
        }
        .emit(&progress);
      },
      || {},
    )
    .await
    .map_err(update_error)?;
  // Windows exits on its own during the install step; elsewhere we drive it.
  app.restart();
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn pub_date_is_rfc3339() {
    let date = time::OffsetDateTime::from_unix_timestamp(1_754_136_000).unwrap();
    assert_eq!(
      format_pub_date(date).as_deref(),
      Some("2025-08-02T12:00:00Z")
    );
  }

  #[test]
  fn emit_step_scales_with_the_download() {
    // ~100 events on a real bundle, and no flood on a tiny one.
    assert_eq!(emit_step(Some(200 * 1024 * 1024)), 2 * 1024 * 1024);
    assert_eq!(emit_step(Some(4 * 1024 * 1024)), 1024 * 1024);
    assert_eq!(emit_step(None), 1024 * 1024);
  }

  #[test]
  fn blank_endpoint_override_is_ignored() {
    // An empty value would blank the endpoint list instead of leaving it alone.
    assert_eq!(chosen_endpoint(None), None);
    assert_eq!(chosen_endpoint(Some("  ".to_string())), None);
    assert_eq!(
      chosen_endpoint(Some("http://127.0.0.1:9000/{{target}}".to_string())).as_deref(),
      Some("http://127.0.0.1:9000/{{target}}")
    );
  }
}
