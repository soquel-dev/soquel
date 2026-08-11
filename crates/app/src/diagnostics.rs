//! The diagnostics dialog: a pasteable support block (built in the core, no
//! names or hosts), with copy and a best-effort "open log folder". The path
//! stays on screen because opening spawns detached and can silently do nothing.

use std::sync::Arc;

use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::{ActiveTheme, Disableable, WindowExt, h_flex, v_flex};
use soquel_core::AppState;

use crate::core;

pub struct DiagnosticsView {
  state: Arc<AppState>,
  block: Option<SharedString>,
  error: Option<SharedString>,
  _task: Task<()>,
}

impl DiagnosticsView {
  pub fn new(state: Arc<AppState>, cx: &mut Context<Self>) -> Self {
    let task = core::diagnostics(state.clone(), cx);
    let _task = cx.spawn(async move |this, cx| {
      let block = task.await;
      let _ = this.update(cx, |this, cx| {
        this.block = Some(block.into());
        cx.notify();
      });
    });
    Self {
      state,
      block: None,
      error: None,
      _task,
    }
  }

  /// The block already names the log; pulling it out saves fishing for it by hand.
  fn log_path(&self) -> Option<String> {
    let block = self.block.as_ref()?;
    block
      .lines()
      .find_map(|line| line.strip_prefix("log: "))
      .map(str::to_string)
  }

  fn copy_block(&mut self, window: &mut Window, cx: &mut Context<Self>) {
    if let Some(block) = &self.block {
      cx.write_to_clipboard(ClipboardItem::new_string(block.to_string()));
      window.push_notification("Diagnostics copied", cx);
    }
  }

  fn copy_path(&mut self, window: &mut Window, cx: &mut Context<Self>) {
    if let Some(path) = self.log_path() {
      cx.write_to_clipboard(ClipboardItem::new_string(path));
      window.push_notification("Log path copied", cx);
    }
  }

  fn open_folder(&mut self, cx: &mut Context<Self>) {
    match core::open_log_folder(&self.state) {
      Ok(_) => self.error = None,
      Err(error) => self.error = Some(error.to_string().into()),
    }
    cx.notify();
  }
}

impl Render for DiagnosticsView {
  fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    let has_block = self.block.is_some();

    v_flex()
      .w_full()
      .gap_3()
      .child(
        div()
          .text_sm()
          .text_color(cx.theme().muted_foreground)
          .child("Facts only, no connection names or hosts. Safe to paste into an issue."),
      )
      .when_some(self.block.clone(), |this, block| {
        this.child(
          div()
            .id("diagnostics-block")
            .max_h(px(280.))
            .overflow_y_scroll()
            .rounded(cx.theme().radius)
            .bg(cx.theme().muted)
            .px_3()
            .py_2()
            .font_family("IBM Plex Mono")
            .text_xs()
            .child(block),
        )
      })
      .when_some(self.error.clone(), |this, error| {
        this.child(
          div()
            .font_family("IBM Plex Mono")
            .text_xs()
            .text_color(cx.theme().danger)
            .child(error),
        )
      })
      .child(
        h_flex()
          .justify_between()
          .gap_2()
          .child(
            h_flex()
              .gap_2()
              .child(
                Button::new("open-log-folder")
                  .outline()
                  .label("Open log folder")
                  .debug_selector(|| "open-log-folder".into())
                  .on_click(cx.listener(|this, _, _, cx| this.open_folder(cx))),
              )
              .child(
                Button::new("copy-log-path")
                  .outline()
                  .label("Copy path")
                  .debug_selector(|| "copy-log-path".into())
                  .on_click(cx.listener(|this, _, window, cx| this.copy_path(window, cx))),
              ),
          )
          .child(
            Button::new("copy-diagnostics")
              .primary()
              .label("Copy")
              .disabled(!has_block)
              .debug_selector(|| "copy-diagnostics".into())
              .on_click(cx.listener(|this, _, window, cx| this.copy_block(window, cx))),
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

  fn test_state() -> (tempfile::TempDir, Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let state = Arc::new(soquel_core::AppState::for_tests(
      dir.path(),
      Box::new(soquel_core::secrets::InMemoryStore::default()),
    ));
    (dir, state)
  }

  #[gpui::test]
  fn it_loads_a_pasteable_block_naming_no_connection(cx: &mut TestAppContext) {
    let (_dir, state) = test_state();
    soquel_core::ops::create_connection(
      &state,
      &soquel_core::profiles::ConnectionInput {
        name: "prod billing".to_string(),
        env: soquel_core::profiles::Env::Prod,
        group: None,
        agent_access: Default::default(),
        credential: soquel_core::profiles::CredentialSource::Keychain,
        params: soquel_core::profiles::ConnectorParams::Postgres(
          soquel_core::profiles::SqlServerParams {
            host: "db.internal".to_string(),
            port: 5432,
            database: "shop".to_string(),
            user: "app".to_string(),
            ssl_mode: soquel_core::profiles::SslMode::Prefer,
            ssl_root_cert: None,
            tunnel_id: None,
          },
        ),
        password: None,
      },
    )
    .unwrap();

    let (view, cx) = test_support::shell_window(cx, {
      let state = state.clone();
      move |_, cx| DiagnosticsView::new(state, cx)
    });
    test_support::wait_until(cx, "the diagnostics block", |cx| {
      cx.update(|_, cx| view.read(cx).block.is_some())
    });

    cx.update(|_, cx| {
      let view = view.read(cx);
      let block = view.block.as_ref().unwrap();
      assert!(block.contains("connections: 1"), "{block}");
      assert!(block.contains("postgres 1"), "{block}");
      assert!(!block.contains("prod billing"), "{block}");
      assert!(!block.contains("db.internal"), "{block}");
      // The log line is pulled out for "Copy path".
      assert!(view.log_path().is_some_and(|p| p.ends_with(".log")));
    });
  }
}
