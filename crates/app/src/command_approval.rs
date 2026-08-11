//! Approve-and-retry dialog for `Error::CommandApprovalRequired`: an imported
//! credential command waits here until its argv has been read.

use std::sync::Arc;

use gpui::*;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::{ActiveTheme, WindowExt, h_flex, v_flex};
use soquel_core::AppState;
use soquel_core::error::{Error, SecretSubject};

use crate::core;

#[derive(Clone)]
pub struct CommandApprovalPrompt {
  pub subject: SecretSubject,
  pub target_id: String,
  pub target_name: String,
  /// Resolved argv off the error, placeholders substituted: what will run.
  pub program: String,
  pub args: Vec<String>,
}

pub fn open_command_approval_dialog<V: 'static>(
  this: Entity<V>,
  state: Arc<AppState>,
  prompt: CommandApprovalPrompt,
  cx: &mut Context<V>,
  on_approved: impl Fn(&mut V, Result<(), Error>, &mut Context<V>) + Clone + 'static,
) {
  let this = this.downgrade();
  crate::dialogs::defer_on_active_window(cx, move |window, cx| {
    window.open_dialog(cx, move |dialog, _, cx| {
      let this = this.clone();
      {
        let state = state.clone();
        let prompt = prompt.clone();
        let on_approved = on_approved.clone();
        let noun = match prompt.subject {
          SecretSubject::Tunnel => "ssh tunnel",
          _ => "connection",
        };
        let approve = Button::new("approve-command")
          .danger()
          .label("Approve and run")
          .debug_selector(|| "approve-command".into())
          .on_click({
            let prompt = prompt.clone();
            move |_, window, cx| {
              let result =
                core::approve_credential_command(&state, prompt.subject, prompt.target_id.clone());
              // Popped before the retry so a fresh failure can stack cleanly.
              window.close_dialog(cx);
              let on_approved = on_approved.clone();
              this
                .update(cx, move |view, cx| on_approved(view, result, cx))
                .ok();
            }
          });
        dialog
          .title(
            div()
              .font_family("IBM Plex Mono")
              .child(format!("Run a command for {}?", prompt.target_name)),
          )
          .w(px(440.))
          // Enter approves nothing: running a program takes an explicit click.
          .on_ok(|_, _, _| false)
          .child(
            v_flex()
              .gap_3()
              .child(div().text_sm().child(format!(
                "This {noun} gets its password by running a program on this machine. \
                 It came from an import, so nothing has run yet. Read it before approving.",
              )))
              .child(
                h_flex()
                  .flex_wrap()
                  .gap_1()
                  .px_3()
                  .py_2()
                  .rounded(cx.theme().radius)
                  .bg(cx.theme().muted)
                  .text_xs()
                  .font_family("IBM Plex Mono")
                  .children(
                    std::iter::once(prompt.program.clone())
                      .chain(prompt.args.clone())
                      .map(|arg| {
                        div()
                          .px_1()
                          .py_0p5()
                          .rounded(cx.theme().radius)
                          .bg(cx.theme().background)
                          .child(arg)
                      }),
                  ),
              ),
          )
          .footer(
            h_flex()
              .gap_2()
              .justify_end()
              .child(
                Button::new("command-approval-cancel")
                  .label("Cancel")
                  .on_click(|_, window, cx| window.close_dialog(cx)),
              )
              .child(approve),
          )
      }
    });
  });
}
