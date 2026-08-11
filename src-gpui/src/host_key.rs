//! Trust-and-retry dialog for `Error::HostKeyUntrusted`. Dialogs stack in
//! gpui-component, so this opens above a form and pops back to it.

use std::sync::Arc;

use gpui::*;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::{ActiveTheme, WindowExt, h_flex, v_flex};
use soquel_core::AppState;
use soquel_core::error::Error;

use crate::core;

#[derive(Clone)]
pub struct HostKeyPrompt {
  pub host: String,
  pub port: u16,
  pub fingerprint: String,
  pub key: String,
  pub previously_trusted: bool,
}

pub fn open_host_key_dialog<V: 'static>(
  this: Entity<V>,
  state: Arc<AppState>,
  prompt: HostKeyPrompt,
  cx: &mut Context<V>,
  on_done: impl Fn(&mut V, Result<(), Error>, &mut Context<V>) + Clone + 'static,
) {
  cx.defer(move |cx| {
    let Some(window_handle) = cx.active_window() else {
      return;
    };
    let _ = cx.update_window(window_handle, |_, window, cx| {
      window.open_dialog(cx, move |dialog, _, cx| {
        let this = this.clone();
        let state = state.clone();
        let prompt = prompt.clone();
        let on_done = on_done.clone();
        let title = if prompt.previously_trusted {
          "Host key changed"
        } else {
          "Unknown host key"
        };
        let description = if prompt.previously_trusted {
          format!(
            "The key for {}:{} does not match the one trusted before. This can mean the server \
             was reinstalled - or that the connection is being intercepted.",
            prompt.host, prompt.port
          )
        } else {
          format!(
            "First connection to {}:{}. Verify the fingerprint before trusting it.",
            prompt.host, prompt.port
          )
        };
        let trust = if prompt.previously_trusted {
          Button::new("trust-host-key").danger()
        } else {
          Button::new("trust-host-key").primary()
        };
        let trust = trust
          .label("Trust and retry")
          .debug_selector(|| "trust-host-key".into())
          .on_click(move |_, window, cx| {
            let result = core::trust_host_key(&state, &prompt.host, prompt.port, &prompt.key);
            // Popped before the retry so a fresh failure can stack cleanly.
            window.close_dialog(cx);
            let on_done = on_done.clone();
            this.update(cx, move |view, cx| on_done(view, result, cx));
          });
        dialog
          .title(title)
          .w(px(440.))
          // Enter trusts nothing: trusting a key takes an explicit click.
          .on_ok(|_, _, _| false)
          .child(
            v_flex()
              .gap_3()
              .child(div().text_sm().child(description))
              .child(
                div()
                  .px_3()
                  .py_2()
                  .rounded(cx.theme().radius)
                  .bg(cx.theme().muted)
                  .text_xs()
                  .font_family("IBM Plex Mono")
                  .child(prompt.fingerprint.clone()),
              ),
          )
          .footer(
            h_flex()
              .gap_2()
              .justify_end()
              .child(
                Button::new("host-key-cancel")
                  .label("Cancel")
                  .on_click(|_, window, cx| window.close_dialog(cx)),
              )
              .child(trust),
          )
      });
    });
  });
}

#[cfg(test)]
mod tests {
  // The parent globs gpui: shadow `test` back or #[gpui::test] recurses.
  use ::core::prelude::v1::test;
  use gpui::{Modifiers, TestAppContext};
  use gpui_component::WindowExt;

  use super::*;
  use crate::test_support;

  /// Bare view standing in for whichever screen opened the dialog.
  struct Probe {
    outcome: Option<Result<(), Error>>,
  }

  impl Render for Probe {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
      gpui::div()
    }
  }

  fn state_and_prompt() -> (tempfile::TempDir, Arc<AppState>, HostKeyPrompt) {
    let dir = tempfile::tempdir().unwrap();
    let state = Arc::new(soquel_core::AppState::for_tests(
      dir.path(),
      Box::new(soquel_core::secrets::InMemoryStore::default()),
    ));
    let key = std::fs::read_to_string(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/../scripts/test-ssh/id_ed25519.pub"
    ))
    .unwrap()
    .trim()
    .to_string();
    let prompt = HostKeyPrompt {
      host: "bastion.internal".to_string(),
      port: 22,
      fingerprint: "SHA256:abc".to_string(),
      key,
      previously_trusted: false,
    };
    (dir, state, prompt)
  }

  #[gpui::test]
  fn trusting_the_host_key_writes_the_trust_and_reports_back(cx: &mut TestAppContext) {
    let (_dir, state, prompt) = state_and_prompt();
    let (probe, cx) = test_support::shell_window(cx, |_, _| Probe { outcome: None });

    cx.update(|_, cx| {
      probe.update(cx, |_, cx| {
        open_host_key_dialog(
          cx.entity(),
          state.clone(),
          prompt.clone(),
          cx,
          |probe: &mut Probe, result, _| probe.outcome = Some(result),
        );
      });
    });
    test_support::wait_until(cx, "the host key dialog", |cx| {
      cx.update(|window, cx| window.has_active_dialog(cx))
    });

    // Enter trusts nothing; the dialog waits for the click.
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert!(cx.update(|window, cx| window.has_active_dialog(cx)));

    let bounds = cx
      .debug_bounds("trust-host-key")
      .expect("trust button painted");
    cx.simulate_click(bounds.center(), Modifiers::none());
    cx.run_until_parked();

    assert!(!cx.update(|window, cx| window.has_active_dialog(cx)));
    assert!(
      state
        .known_hosts
        .lock()
        .unwrap()
        .get("bastion.internal", 22)
        .is_some(),
      "the trust is written before the retry"
    );
    cx.update(|_, cx| {
      assert!(matches!(probe.read(cx).outcome, Some(Ok(()))));
    });
  }

  #[gpui::test]
  fn escape_trusts_nothing(cx: &mut TestAppContext) {
    let (_dir, state, prompt) = state_and_prompt();
    let (probe, cx) = test_support::shell_window(cx, |_, _| Probe { outcome: None });

    cx.update(|_, cx| {
      probe.update(cx, |_, cx| {
        open_host_key_dialog(
          cx.entity(),
          state.clone(),
          prompt,
          cx,
          |probe: &mut Probe, result, _| probe.outcome = Some(result),
        );
      });
    });
    test_support::wait_until(cx, "the host key dialog", |cx| {
      cx.update(|window, cx| window.has_active_dialog(cx))
    });

    cx.simulate_keystrokes("escape");
    cx.run_until_parked();

    assert!(!cx.update(|window, cx| window.has_active_dialog(cx)));
    assert!(
      state
        .known_hosts
        .lock()
        .unwrap()
        .get("bastion.internal", 22)
        .is_none()
    );
    cx.update(|_, cx| assert!(probe.read(cx).outcome.is_none()));
  }
}
