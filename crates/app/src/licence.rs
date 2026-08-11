//! The licence dialog: status, activate a pasted key (HTTP in the core), or add
//! a licence file. A successful install writes the file, so the open workspace's
//! tab cap lifts on its next open with no reactive plumbing.

use std::sync::Arc;

use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{ActiveTheme, Disableable, Sizable, WindowExt, h_flex, v_flex};
use soquel_core::AppState;
use soquel_core::error::{ActivationReason, Error};
use soquel_core::licence::LicenceStatus;

use crate::core;
use crate::format::format_day;

/// The service says what happened; the app says what to do about it. Exhaustive
/// over the reason union so a new one stops compiling until it has an answer.
pub fn activation_message(reason: ActivationReason) -> &'static str {
  match reason {
    ActivationReason::Offline => {
      "No answer from the licence server. Check your connection and try again."
    }
    ActivationReason::UnknownKey => {
      "That key is not one of ours. Copy it again from your Polar account."
    }
    ActivationReason::WrongProduct => "That key belongs to another product.",
    ActivationReason::Revoked => {
      "That key is no longer valid. Get in touch and we will sort it out."
    }
    ActivationReason::ActivationLimit => {
      "This key has been activated on as many machines as it allows. Free one in your Polar \
       account, or paste the licence file from another machine."
    }
    ActivationReason::UpstreamUnavailable => {
      "The licence server is having trouble reaching Polar. Your key is fine, try again in a \
       minute."
    }
  }
}

/// A licence can install and still unlock nothing, so success cannot be one
/// phrase. An install answers `licensed` or `expired`, never `free`.
pub fn installed_outcome(status: &LicenceStatus) -> (bool, &'static str) {
  match status {
    LicenceStatus::Licensed { .. } => (true, "Licence added. Tabs are unlimited from here."),
    _ => (false, "Licence added, and it does not cover this build."),
  }
}

/// The descriptive line under the title, one per state. Expired and free both
/// limit the app; only saying so tells a lapsed window from a bug.
pub fn status_blurb(status: &LicenceStatus) -> String {
  match status {
    LicenceStatus::Licensed {
      email,
      updates_until,
      ..
    } => match format_day(updates_until) {
      Some(day) => format!("Licensed to {email}, with updates through {day}."),
      None => format!("Licensed to {email}."),
    },
    LicenceStatus::Expired { updates_until, .. } => {
      let day = format_day(updates_until).unwrap_or_else(|| updates_until.clone());
      format!(
        "Your updates ran until {day}, and this build came out after that, so it runs on the free \
         tier. Earlier builds keep working with this licence, and renewing reopens the newer ones."
      )
    }
    LicenceStatus::Free => "The free tier opens two tabs per connection, with everything else \
                            included, agent access and all. A licence lifts the limit for good."
      .to_string(),
  }
}

fn outcome_of(result: Result<LicenceStatus, Error>) -> (bool, SharedString) {
  match result {
    Ok(status) => {
      let (ok, message) = installed_outcome(&status);
      (ok, message.into())
    }
    Err(Error::Activation { reason, .. }) => (false, activation_message(reason).into()),
    Err(error) => (false, error.to_string().into()),
  }
}

pub struct LicenceView {
  state: Arc<AppState>,
  status: LicenceStatus,
  key_input: Entity<InputState>,
  token_input: Entity<InputState>,
  pasting: bool,
  busy: bool,
  outcome: Option<(bool, SharedString)>,
  _task: Task<()>,
  _key_subscription: Subscription,
}

impl LicenceView {
  pub fn new(state: Arc<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
    let key_input = cx.new(|cx| InputState::new(window, cx).placeholder("SOQUEL-XXXX-XXXX-XXXX"));
    let token_input = cx.new(|cx| {
      InputState::new(window, cx)
        .multi_line(true)
        .placeholder("Paste your licence file")
    });
    let _key_subscription = cx.subscribe(&key_input, |this, _, event: &InputEvent, cx| {
      if matches!(event, InputEvent::PressEnter { .. }) {
        this.apply_key(cx);
      }
    });
    Self {
      status: core::licence_status(&state),
      state,
      key_input,
      token_input,
      pasting: false,
      busy: false,
      outcome: None,
      _task: Task::ready(()),
      _key_subscription,
    }
  }

  fn apply_key(&mut self, cx: &mut Context<Self>) {
    let key = self.key_input.read(cx).value().trim().to_string();
    if key.is_empty() || self.busy {
      return;
    }
    let rx = core::licence_activate(self.state.clone(), key);
    self.run(rx, cx);
  }

  fn apply_file(&mut self, cx: &mut Context<Self>) {
    let token = self.token_input.read(cx).value().trim().to_string();
    if token.is_empty() || self.busy {
      return;
    }
    let rx = core::licence_install(self.state.clone(), token);
    self.run(rx, cx);
  }

  fn run(
    &mut self,
    rx: futures::channel::oneshot::Receiver<Result<LicenceStatus, Error>>,
    cx: &mut Context<Self>,
  ) {
    self.busy = true;
    self.outcome = None;
    cx.notify();
    self._task = cx.spawn(async move |this, cx| {
      let result = rx.await;
      let _ = this.update(cx, |this, cx| {
        this.busy = false;
        if let Ok(result) = result {
          if result.is_ok() {
            this.status = core::licence_status(&this.state);
          }
          this.outcome = Some(outcome_of(result));
        }
        cx.notify();
      });
    });
  }
}

impl Render for LicenceView {
  fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    let (outcome_ok, outcome_message) = match &self.outcome {
      Some((ok, message)) => (*ok, Some(message.clone())),
      None => (false, None),
    };

    v_flex()
      .w_full()
      .gap_3()
      .child(
        div()
          .text_sm()
          .text_color(cx.theme().muted_foreground)
          .child(status_blurb(&self.status)),
      )
      .child(Input::new(&self.key_input).small())
      .child(
        Button::new("licence-file-toggle")
          .ghost()
          .xsmall()
          .label(if self.pasting {
            "I have a licence file  ▾"
          } else {
            "I have a licence file  ▸"
          })
          .debug_selector(|| "licence-file-toggle".into())
          .on_click(cx.listener(|this, _, _, cx| {
            this.pasting = !this.pasting;
            cx.notify();
          })),
      )
      .when(self.pasting, |this| {
        this.child(
          v_flex()
            .gap_2()
            .child(div().h(px(96.)).child(Input::new(&self.token_input)))
            .child(
              Button::new("apply-licence-file")
                .outline()
                .small()
                .label("Add licence file")
                .disabled(self.busy)
                .debug_selector(|| "apply-licence-file".into())
                .on_click(cx.listener(|this, _, _, cx| this.apply_file(cx))),
            ),
        )
      })
      .when_some(outcome_message, |this, message| {
        this.child(
          div()
            .font_family("IBM Plex Mono")
            .text_xs()
            .text_color(if outcome_ok {
              cx.theme().green
            } else {
              cx.theme().danger
            })
            .child(message),
        )
      })
      .child(
        h_flex()
          .justify_end()
          .gap_2()
          .child(
            Button::new("licence-close")
              .outline()
              .label("Close")
              .on_click(|_, window, cx| window.close_dialog(cx)),
          )
          .child(
            Button::new("apply-licence")
              .primary()
              .label(if self.busy { "Checking…" } else { "Activate" })
              .disabled(self.busy)
              .debug_selector(|| "apply-licence".into())
              .on_click(cx.listener(|this, _, _, cx| this.apply_key(cx))),
          ),
      )
  }
}

#[cfg(test)]
mod tests {
  use ::core::prelude::v1::test;
  use gpui::TestAppContext;

  use super::*;
  use crate::test_support;

  #[test]
  fn activation_messages_cover_every_reason() {
    for reason in [
      ActivationReason::Offline,
      ActivationReason::UnknownKey,
      ActivationReason::WrongProduct,
      ActivationReason::Revoked,
      ActivationReason::ActivationLimit,
      ActivationReason::UpstreamUnavailable,
    ] {
      assert!(!activation_message(reason).is_empty());
    }
    assert_eq!(
      activation_message(ActivationReason::WrongProduct),
      "That key belongs to another product."
    );
  }

  #[test]
  fn installed_outcome_only_licensed_unlocks() {
    let licensed = LicenceStatus::Licensed {
      email: "b@example.com".to_string(),
      name: None,
      updates_until: "2030-01-01T00:00:00Z".to_string(),
    };
    assert_eq!(
      installed_outcome(&licensed),
      (true, "Licence added. Tabs are unlimited from here.")
    );
    let expired = LicenceStatus::Expired {
      email: "b@example.com".to_string(),
      updates_until: "2020-01-01T00:00:00Z".to_string(),
    };
    assert!(!installed_outcome(&expired).0);
  }

  #[test]
  fn status_blurb_reads_by_state() {
    let licensed = LicenceStatus::Licensed {
      email: "buyer@example.com".to_string(),
      name: None,
      updates_until: "2027-01-15T00:00:00Z".to_string(),
    };
    assert_eq!(
      status_blurb(&licensed),
      "Licensed to buyer@example.com, with updates through Jan 15, 2027."
    );
    assert!(status_blurb(&LicenceStatus::Free).contains("two tabs per connection"));
    let expired = LicenceStatus::Expired {
      email: "b@example.com".to_string(),
      updates_until: "2024-03-01T00:00:00Z".to_string(),
    };
    assert!(status_blurb(&expired).contains("free tier"));
  }

  #[test]
  fn outcome_of_routes_success_and_each_refusal() {
    let licensed = Ok(LicenceStatus::Licensed {
      email: "b@example.com".to_string(),
      name: None,
      updates_until: "2030-01-01T00:00:00Z".to_string(),
    });
    assert_eq!(
      outcome_of(licensed),
      (true, "Licence added. Tabs are unlimited from here.".into())
    );
    // An install that verifies but does not cover this build is not a success.
    let expired = Ok(LicenceStatus::Expired {
      email: "b@example.com".to_string(),
      updates_until: "2020-01-01T00:00:00Z".to_string(),
    });
    assert!(!outcome_of(expired).0);
    // A refused activation carries why, and the reason picks the message.
    let refused = Err(Error::Activation {
      message: "the service said no".to_string(),
      reason: ActivationReason::Revoked,
    });
    assert_eq!(
      outcome_of(refused),
      (false, activation_message(ActivationReason::Revoked).into())
    );
    // Anything else is already a sentence.
    let other = Err(Error::Unsupported {
      message: "boom".to_string(),
    });
    assert_eq!(outcome_of(other), (false, "boom".into()));
  }

  fn test_state() -> (tempfile::TempDir, Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let state = Arc::new(soquel_core::AppState::for_tests(
      dir.path(),
      Box::new(soquel_core::secrets::InMemoryStore::default()),
    ));
    (dir, state)
  }

  #[gpui::test]
  fn the_file_field_expands_and_an_empty_key_does_nothing(cx: &mut TestAppContext) {
    let (_dir, state) = test_state();
    let (view, cx) = test_support::shell_window(cx, {
      let state = state.clone();
      move |window, cx| LicenceView::new(state, window, cx)
    });

    // Empty key: Activate is a no-op, no outcome.
    cx.update(|_, cx| view.update(cx, |view, cx| view.apply_key(cx)));
    cx.run_until_parked();
    cx.update(|_, cx| assert!(view.read(cx).outcome.is_none()));

    // The licence-file field is folded away until asked for.
    cx.update(|_, cx| assert!(!view.read(cx).pasting));
    let toggle = cx
      .debug_bounds("licence-file-toggle")
      .expect("toggle painted");
    cx.simulate_click(toggle.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    cx.update(|_, cx| assert!(view.read(cx).pasting));
  }
}
