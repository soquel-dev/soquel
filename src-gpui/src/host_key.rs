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
