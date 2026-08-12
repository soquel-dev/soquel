//! The Cmd/Ctrl-K palette: a searchable `ListState` of curated entries, each
//! carrying a closure the App builds against the current screen.

use std::rc::Rc;

use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::list::{ListDelegate, ListItem, ListState};
use gpui_component::{ActiveTheme, Icon, IndexPath, Sizable, WindowExt, h_flex, v_flex};

pub type PaletteRun = Rc<dyn Fn(&mut Window, &mut App)>;

/// Display order in the palette; empty sections are skipped.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PaletteSection {
  Connections,
  Actions,
  App,
}

const SECTIONS: [PaletteSection; 3] = [
  PaletteSection::Connections,
  PaletteSection::Actions,
  PaletteSection::App,
];

impl PaletteSection {
  fn label(self) -> &'static str {
    match self {
      PaletteSection::Connections => "Connections",
      PaletteSection::Actions => "Actions",
      PaletteSection::App => "App",
    }
  }
}

pub struct PaletteItem {
  pub label: SharedString,
  /// Right-aligned detail (a connection's target).
  pub hint: Option<SharedString>,
  /// Lowercased haystack the filter searches.
  pub keywords: String,
  pub icon: Icon,
  pub section: PaletteSection,
  pub run: PaletteRun,
}

/// Case-insensitive substring filter.
pub fn palette_matches(keywords: &str, query: &str) -> bool {
  let query = query.trim().to_lowercase();
  query.is_empty() || keywords.to_lowercase().contains(&query)
}

fn group(all: &[PaletteItem], query: &str) -> Vec<(PaletteSection, Vec<usize>)> {
  SECTIONS
    .iter()
    .filter_map(|&section| {
      let rows: Vec<usize> = (0..all.len())
        .filter(|&ix| all[ix].section == section && palette_matches(&all[ix].keywords, query))
        .collect();
      (!rows.is_empty()).then_some((section, rows))
    })
    .collect()
}

pub struct CommandPaletteDelegate {
  all: Vec<PaletteItem>,
  groups: Vec<(PaletteSection, Vec<usize>)>,
  selected: IndexPath,
}

impl CommandPaletteDelegate {
  pub fn new(all: Vec<PaletteItem>) -> Self {
    let groups = group(&all, "");
    Self {
      all,
      groups,
      selected: IndexPath::default(),
    }
  }

  fn item(&self, ix: IndexPath) -> Option<&PaletteItem> {
    let (_, rows) = self.groups.get(ix.section)?;
    rows.get(ix.row).map(|&i| &self.all[i])
  }
}

impl ListDelegate for CommandPaletteDelegate {
  type Item = ListItem;

  fn perform_search(
    &mut self,
    query: &str,
    _: &mut Window,
    _: &mut Context<ListState<Self>>,
  ) -> Task<()> {
    self.groups = group(&self.all, query);
    self.selected = IndexPath::default();
    Task::ready(())
  }

  fn sections_count(&self, _: &App) -> usize {
    self.groups.len().max(1)
  }

  fn items_count(&self, section: usize, _: &App) -> usize {
    self.groups.get(section).map_or(0, |(_, rows)| rows.len())
  }

  fn render_section_header(
    &mut self,
    section: usize,
    _: &mut Window,
    cx: &mut Context<ListState<Self>>,
  ) -> Option<impl IntoElement> {
    let (section, _) = self.groups.get(section)?;
    Some(
      div()
        .px_3()
        .pt_3()
        .pb_1()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(section.label()),
    )
  }

  fn render_empty(
    &mut self,
    _: &mut Window,
    cx: &mut Context<ListState<Self>>,
  ) -> impl IntoElement {
    v_flex()
      .items_center()
      .py_8()
      .text_sm()
      .text_color(cx.theme().muted_foreground)
      .child("No matching commands")
      .into_any_element()
  }

  fn render_item(
    &mut self,
    ix: IndexPath,
    _: &mut Window,
    cx: &mut Context<ListState<Self>>,
  ) -> Option<Self::Item> {
    let item = self.item(ix)?;
    let selected = ix.section == self.selected.section && ix.row == self.selected.row;
    let row = ListItem::new(ix)
      .selected(selected)
      .mx_1()
      .px_2()
      .py_1p5()
      .rounded_md()
      .child(
        h_flex()
          .w_full()
          .items_center()
          .justify_between()
          .gap_3()
          .child(
            h_flex()
              .items_center()
              .gap_2()
              .child(
                item
                  .icon
                  .clone()
                  .small()
                  .text_color(cx.theme().muted_foreground),
              )
              .child(div().text_sm().child(item.label.clone())),
          )
          .when_some(item.hint.clone(), |row, hint| {
            row.child(
              div()
                .text_xs()
                .font_family("IBM Plex Mono")
                .text_color(cx.theme().muted_foreground)
                .whitespace_nowrap()
                .text_ellipsis()
                .overflow_hidden()
                .child(hint),
            )
          }),
      );
    Some(row)
  }

  fn set_selected_index(
    &mut self,
    ix: Option<IndexPath>,
    _: &mut Window,
    _: &mut Context<ListState<Self>>,
  ) {
    if let Some(ix) = ix {
      self.selected = ix;
    }
  }

  fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<ListState<Self>>) {
    let run = self.item(self.selected).map(|item| item.run.clone());
    window.close_dialog(cx);
    if let Some(run) = run {
      run(window, cx);
    }
  }

  fn cancel(&mut self, window: &mut Window, cx: &mut Context<ListState<Self>>) {
    window.close_dialog(cx);
  }
}

/// The keyboard-hint strip pinned under the list.
pub fn palette_footer(cx: &App) -> impl IntoElement {
  let key = |label: &'static str, cx: &App| {
    div()
      .px_1()
      .rounded_sm()
      .bg(cx.theme().muted)
      .text_color(cx.theme().muted_foreground)
      .child(label)
  };
  h_flex()
    .px_3()
    .py_2()
    .gap_4()
    .border_t_1()
    .border_color(cx.theme().border)
    .text_xs()
    .text_color(cx.theme().muted_foreground)
    .child(
      h_flex()
        .gap_1()
        .child(key("↑", cx))
        .child(key("↓", cx))
        .child("navigate"),
    )
    .child(h_flex().gap_1().child(key("↵", cx)).child("run"))
    .child(h_flex().gap_1().child(key("esc", cx)).child("close"))
}

#[cfg(test)]
mod tests {
  use ::core::prelude::v1::test;

  use super::*;

  #[test]
  fn matching_is_case_insensitive_substring() {
    assert!(palette_matches("toggle theme dark light", "THEME"));
    assert!(palette_matches("connect prod warehouse 10.0.0.1", "ware"));
    assert!(!palette_matches("run query execute", "schema"));
  }

  #[test]
  fn an_empty_query_matches_everything() {
    assert!(palette_matches("anything", ""));
    assert!(palette_matches("anything", "   "));
  }

  fn item(label: &str, section: PaletteSection) -> PaletteItem {
    PaletteItem {
      label: label.to_string().into(),
      hint: None,
      keywords: label.to_lowercase(),
      icon: Icon::empty(),
      section,
      run: Rc::new(|_, _| {}),
    }
  }

  #[test]
  fn grouping_orders_sections_and_drops_empty_ones() {
    let all = vec![
      item("Licence", PaletteSection::App),
      item("warehouse", PaletteSection::Connections),
      item("New connection", PaletteSection::Actions),
    ];
    let groups = group(&all, "");
    let sections: Vec<PaletteSection> = groups.iter().map(|(s, _)| *s).collect();
    assert_eq!(
      sections,
      vec![
        PaletteSection::Connections,
        PaletteSection::Actions,
        PaletteSection::App
      ]
    );

    let groups = group(&all, "licence");
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].0, PaletteSection::App);
    assert_eq!(groups[0].1, vec![0]);
  }
}
