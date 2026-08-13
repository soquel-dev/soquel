//! Shared visual atoms: chips, badges, and the selectable list row.

use gpui::prelude::FluentBuilder;
use gpui::{
  App, Div, ElementId, Hsla, InteractiveElement, ParentElement, SharedString, Stateful, Styled, div,
};
use gpui_component::{ActiveTheme, h_flex};

/// A filled muted pill for secondary facts (auth mode, agent access, filters).
pub fn chip(text: impl Into<SharedString>, cx: &App) -> Div {
  div()
    .px_1p5()
    .py_0p5()
    .rounded(cx.theme().radius)
    .bg(cx.theme().muted)
    .text_xs()
    .font_family(crate::theme::mono(cx))
    .text_color(cx.theme().muted_foreground)
    .child(text.into())
}

/// A color-tinted badge for classifying facts (env, key and collection kinds).
pub fn tinted_badge(text: impl Into<SharedString>, color: Hsla, cx: &App) -> Div {
  div()
    .px_1p5()
    .rounded(cx.theme().radius)
    .bg(color.opacity(0.12))
    .text_color(color)
    .text_xs()
    .font_family(crate::theme::mono(cx))
    .child(text.into())
}

/// The rounded selectable row of the kv and doc lists; content chains onto it.
pub fn list_row(id: impl Into<ElementId>, selected: bool, cx: &App) -> Stateful<Div> {
  h_flex()
    .id(id)
    .w_full()
    .mx_1()
    .px_2()
    .py_1()
    .gap_2()
    .items_center()
    .cursor_default()
    .rounded(cx.theme().radius)
    .when(selected, |row| row.bg(cx.theme().accent))
    .hover(|row| row.bg(cx.theme().accent.opacity(0.5)))
}
