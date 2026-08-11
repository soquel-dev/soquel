//! The global "an agent wants to write" dialog. Dismissing it denies: silence
//! must never mean yes. The buttons answer explicitly; escape, the overlay and
//! the close button all fall through to deny, guarded so a click never resolves
//! the same request twice.

use std::cell::Cell;
use std::rc::Rc;

use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::{ActiveTheme, WindowExt, h_flex, v_flex};
use soquel_core::ApprovalAnswer;
use soquel_core::mcp::McpApprovalRequest;

pub fn open_mcp_approval_dialog<V: 'static>(
  this: Entity<V>,
  request: McpApprovalRequest,
  more_waiting: usize,
  cx: &mut Context<V>,
  on_answer: impl Fn(&mut V, ApprovalAnswer, &mut Context<V>) + Clone + 'static,
) {
  cx.defer(move |cx| {
    let Some(window_handle) = cx.active_window() else {
      return;
    };
    let _ = cx.update_window(window_handle, |_, window, cx| {
      // One dialog, one answer: shared across the buttons and the dismiss path.
      let resolved = Rc::new(Cell::new(false));
      window.open_dialog(cx, move |dialog, _, cx| {
        let this = this.clone();
        let request = request.clone();
        let on_answer = on_answer.clone();
        let resolved = resolved.clone();

        let answer = {
          let this = this.clone();
          let on_answer = on_answer.clone();
          let resolved = resolved.clone();
          move |ans: ApprovalAnswer, window: &mut Window, cx: &mut App| {
            if resolved.replace(true) {
              return;
            }
            window.close_dialog(cx);
            let on_answer = on_answer.clone();
            this.update(cx, move |view, cx| on_answer(view, ans, cx));
          }
        };

        let deny = {
          let answer = answer.clone();
          Button::new("approval-deny")
            .outline()
            .label("Deny")
            .debug_selector(|| "approval-deny".into())
            .on_click(move |_, window, cx| answer(ApprovalAnswer::Deny, window, cx))
        };
        let allow_window = {
          let answer = answer.clone();
          Button::new("approval-allow-window")
            .outline()
            .label("Allow for 15 min")
            .debug_selector(|| "approval-allow-window".into())
            .on_click(move |_, window, cx| answer(ApprovalAnswer::ForWindow, window, cx))
        };
        let run = {
          let answer = answer.clone();
          Button::new("approval-allow")
            .danger()
            .label("Run this write")
            .debug_selector(|| "approval-allow".into())
            .on_click(move |_, window, cx| answer(ApprovalAnswer::Once, window, cx))
        };

        dialog
          .title(
            div()
              .font_family("IBM Plex Mono")
              .child("An agent wants to write"),
          )
          .w(px(520.))
          // Enter allows nothing: writing takes an explicit button.
          .on_ok(|_, _, _| false)
          .on_close({
            let this = this.clone();
            let on_answer = on_answer.clone();
            let resolved = resolved.clone();
            move |_, _, cx| {
              if resolved.replace(true) {
                return;
              }
              let on_answer = on_answer.clone();
              this.update(cx, move |view, cx| {
                on_answer(view, ApprovalAnswer::Deny, cx)
              });
            }
          })
          .child(
            v_flex()
              .gap_3()
              .child(div().text_sm().child(format!(
                "This changes data on {}. It runs only if you allow it.",
                request.connection_name
              )))
              .child(
                div()
                  .id("approval-operation")
                  .max_h(px(240.))
                  .overflow_y_scroll()
                  .rounded(cx.theme().radius)
                  .bg(cx.theme().muted)
                  .px_3()
                  .py_2()
                  .font_family("IBM Plex Mono")
                  .text_xs()
                  .child(request.operation.clone()),
              )
              .when_some(request.payload.clone(), |this, payload| {
                this.child(
                  v_flex()
                    .gap_1p5()
                    .child(
                      div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child("What it writes"),
                    )
                    .child(
                      div()
                        .id("approval-payload")
                        .max_h(px(240.))
                        .overflow_y_scroll()
                        .rounded(cx.theme().radius)
                        .bg(cx.theme().muted)
                        .px_3()
                        .py_2()
                        .font_family("IBM Plex Mono")
                        .text_xs()
                        .child(payload),
                    ),
                )
              })
              .when(more_waiting > 0, |this| {
                this.child(
                  div()
                    .font_family("IBM Plex Mono")
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!("{more_waiting} more waiting")),
                )
              }),
          )
          .footer(
            h_flex()
              .justify_between()
              .child(deny)
              .child(h_flex().gap_2().child(allow_window).child(run)),
          )
      });
    });
  });
}
