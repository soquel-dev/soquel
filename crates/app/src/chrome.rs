//! Chrome shared by every window kind: the app-level palette section, the
//! footer, and the dialogs reachable from any window.

use std::rc::Rc;
use std::sync::Arc;

use gpui::prelude::FluentBuilder;
use gpui::{
  App, AppContext, AsKeystroke, IntoElement, ParentElement, SharedString, Styled, Window, div, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::kbd::Kbd;
use gpui_component::list::{List, ListState};
use gpui_component::{ActiveTheme, Icon, IconName, Sizable, WindowExt, h_flex};
use soquel_core::AppState;

use crate::actions::ToggleCommandPalette;
use crate::command_palette::{CommandPaletteDelegate, PaletteItem, PaletteSection, palette_footer};
use crate::core;
use crate::dialogs;
use crate::icons::SoquelIcon;
use crate::mcp::{McpAuditView, McpPanel};
use crate::theme;

/// The screen-independent palette entries, identical in every window.
pub fn app_palette_items(state: Arc<AppState>, cx: &App) -> Vec<PaletteItem> {
  let dark = cx.theme().mode.is_dark();
  let mut items = Vec::new();
  items.push(PaletteItem {
    label: if dark {
      "Switch to light theme".into()
    } else {
      "Switch to dark theme".into()
    },
    hint: None,
    keywords: "toggle theme dark light".to_string(),
    icon: Icon::new(if dark { IconName::Sun } else { IconName::Moon }),
    section: PaletteSection::App,
    run: Rc::new(theme::toggle),
  });
  let panel_state = state.clone();
  items.push(PaletteItem {
    label: "MCP server".into(),
    hint: None,
    keywords: "mcp server agent access port token".to_string(),
    icon: Icon::new(IconName::Bot),
    section: PaletteSection::App,
    run: Rc::new(move |window, cx| open_mcp_panel(panel_state.clone(), window, cx)),
  });
  let audit_state = state.clone();
  items.push(PaletteItem {
    label: "Agent activity".into(),
    hint: None,
    keywords: "agent activity audit log mcp".to_string(),
    icon: Icon::new(IconName::BookOpen),
    section: PaletteSection::App,
    run: Rc::new(move |window, cx| open_audit(audit_state.clone(), window, cx)),
  });
  let licence_state = state.clone();
  items.push(PaletteItem {
    label: "Licence".into(),
    hint: None,
    keywords: "licence license unlock buy activate tabs".to_string(),
    icon: Icon::new(SoquelIcon::Lock),
    section: PaletteSection::App,
    run: Rc::new(move |window, cx| open_licence(licence_state.clone(), window, cx)),
  });
  let diagnostics_state = state;
  items.push(PaletteItem {
    label: "Diagnostics and logs".into(),
    hint: None,
    keywords: "diagnostics logs support bug report".to_string(),
    icon: Icon::new(IconName::Info),
    section: PaletteSection::App,
    run: Rc::new(move |window, cx| open_diagnostics(diagnostics_state.clone(), window, cx)),
  });
  items
}

pub fn open_mcp_panel(state: Arc<AppState>, window: &mut Window, cx: &mut App) {
  window.defer(cx, move |window, cx| {
    let make_approver = crate::mcp::make_approver(cx);
    let panel = cx.new(|cx| McpPanel::new(state, make_approver, window, cx));
    window.open_dialog(cx, move |dialog, window, cx| {
      dialogs::styled(dialog, window, cx)
        .w(px(560.))
        .child(panel.clone())
    });
  });
}

pub fn open_audit(state: Arc<AppState>, window: &mut Window, cx: &mut App) {
  window.defer(cx, move |window, cx| {
    let audit = cx.new(|cx| McpAuditView::new(state, cx));
    window.open_dialog(cx, move |dialog, window, cx| {
      dialogs::styled(dialog, window, cx)
        .title(
          div()
            .font_family(crate::theme::mono(cx))
            .child("Agent activity"),
        )
        .w(px(640.))
        .child(audit.clone())
    });
  });
}

pub fn open_licence(state: Arc<AppState>, window: &mut Window, cx: &mut App) {
  window.defer(cx, move |window, cx| {
    let view = cx.new(|cx| crate::licence::LicenceView::new(state, window, cx));
    window.open_dialog(cx, move |dialog, window, cx| {
      dialogs::styled(dialog, window, cx)
        .title(div().font_family(crate::theme::mono(cx)).child("Licence"))
        .w(px(440.))
        .child(view.clone())
    });
  });
}

pub fn open_diagnostics(state: Arc<AppState>, window: &mut Window, cx: &mut App) {
  window.defer(cx, move |window, cx| {
    let view = cx.new(|cx| crate::diagnostics::DiagnosticsView::new(state, cx));
    window.open_dialog(cx, move |dialog, window, cx| {
      dialogs::styled(dialog, window, cx)
        .title(
          div()
            .font_family(crate::theme::mono(cx))
            .child("Diagnostics and logs"),
        )
        .w(px(520.))
        .child(view.clone())
    });
  });
}

pub fn open_command_palette(items: Vec<PaletteItem>, window: &mut Window, cx: &mut App) {
  window.defer(cx, move |window, cx| {
    let state =
      cx.new(|cx| ListState::new(CommandPaletteDelegate::new(items), window, cx).searchable(true));
    // No buttons in the footer: the List owns enter/escape under its own key context.
    let list = state.clone();
    window.open_dialog(cx, move |dialog, window, cx| {
      dialogs::styled(dialog, window, cx)
        .w(px(620.))
        .p_0()
        .gap_0()
        .close_button(false)
        .child(
          List::new(&list)
            .with_size(gpui_component::Size::Large)
            .search_placeholder("Search connections and actions…")
            .max_h(px(400.)),
        )
        .footer(palette_footer(cx))
    });
    // After the dialog took focus: hand it to the query input so typing
    // filters and Enter reaches the List's own confirm, not the dialog's.
    state.update(cx, |state, cx| state.focus(window, cx));
  });
}

pub fn footer(
  state: Arc<AppState>,
  connection: Option<String>,
  status: SharedString,
  window: &Window,
  cx: &App,
) -> impl IntoElement {
  let palette_key = window
    .highest_precedence_binding_for_action(&ToggleCommandPalette)
    .and_then(|binding| {
      binding
        .keystrokes()
        .first()
        .map(|key| Kbd::format(key.as_keystroke()))
    });
  h_flex()
    .px_3()
    .py_1()
    .gap_3()
    .items_center()
    .bg(theme::canvas(cx))
    .border_t_1()
    .border_color(cx.theme().border)
    .text_xs()
    .text_color(cx.theme().muted_foreground)
    .child(
      div()
        .font_family(theme::mono(cx))
        .child(concat!("soquel ", env!("CARGO_PKG_VERSION"))),
    )
    .when_some(connection, |bar, connection| bar.child(connection))
    .when(!status.is_empty(), |bar| bar.child(status))
    .child(div().flex_1())
    .child(
      h_flex()
        .gap_1()
        .items_center()
        .child({
          let running_port = core::mcp_running_port(&state);
          let mcp_state = state.clone();
          Button::new("footer-mcp")
            .ghost()
            .xsmall()
            .on_click(move |_, window, cx| open_mcp_panel(mcp_state.clone(), window, cx))
            .child(
              h_flex()
                .gap_1p5()
                .items_center()
                .child(div().w_1p5().h_1p5().rounded_full().bg(match running_port {
                  Some(_) => cx.theme().green,
                  None => cx.theme().muted_foreground.opacity(0.5),
                }))
                .child(
                  div()
                    .font_family(theme::mono(cx))
                    .child(match running_port {
                      Some(port) => format!("mcp :{port}"),
                      None => "mcp".to_string(),
                    }),
                ),
            )
        })
        .child(
          Button::new("footer-theme")
            .ghost()
            .xsmall()
            .icon(Icon::new(if cx.theme().mode.is_dark() {
              IconName::Sun
            } else {
              IconName::Moon
            }))
            .on_click(|_, window, cx| theme::toggle(window, cx)),
        )
        .when_some(palette_key, |buttons, label| {
          buttons.child(
            Button::new("footer-palette")
              .ghost()
              .xsmall()
              .label(label)
              .on_click(|_, window, cx| window.dispatch_action(Box::new(ToggleCommandPalette), cx)),
          )
        }),
    )
}
