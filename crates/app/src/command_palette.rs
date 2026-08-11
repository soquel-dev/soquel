//! The Cmd/Ctrl-K palette: a searchable `ListState` of curated entries, each
//! carrying a closure the App builds against the current screen.

use std::rc::Rc;

use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::list::{ListDelegate, ListItem, ListState};
use gpui_component::{ActiveTheme, IndexPath, WindowExt, h_flex};

pub type PaletteRun = Rc<dyn Fn(&mut Window, &mut App)>;

pub struct PaletteItem {
  pub label: SharedString,
  /// Right-aligned detail (a connection's target).
  pub hint: Option<SharedString>,
  /// Lowercased haystack the filter searches.
  pub keywords: String,
  pub run: PaletteRun,
}

/// Case-insensitive substring filter.
pub fn palette_matches(keywords: &str, query: &str) -> bool {
  let query = query.trim().to_lowercase();
  query.is_empty() || keywords.to_lowercase().contains(&query)
}

pub struct CommandPaletteDelegate {
  all: Vec<PaletteItem>,
  filtered: Vec<usize>,
  selected: usize,
}

impl CommandPaletteDelegate {
  pub fn new(all: Vec<PaletteItem>) -> Self {
    let filtered = (0..all.len()).collect();
    Self {
      all,
      filtered,
      selected: 0,
    }
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
    self.filtered = (0..self.all.len())
      .filter(|&ix| palette_matches(&self.all[ix].keywords, query))
      .collect();
    self.selected = 0;
    Task::ready(())
  }

  fn items_count(&self, _: usize, _: &App) -> usize {
    self.filtered.len()
  }

  fn render_item(
    &mut self,
    ix: IndexPath,
    _: &mut Window,
    cx: &mut Context<ListState<Self>>,
  ) -> Option<Self::Item> {
    let item = self.filtered.get(ix.row).map(|&i| &self.all[i])?;
    let row = ListItem::new(ix.row)
      .selected(ix.row == self.selected)
      .child(
        h_flex()
          .w_full()
          .justify_between()
          .items_center()
          .gap_2()
          .child(div().text_sm().child(item.label.clone()))
          .when_some(item.hint.clone(), |row, hint| {
            row.child(
              div()
                .text_xs()
                .font_family("IBM Plex Mono")
                .text_color(cx.theme().muted_foreground)
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
      self.selected = ix.row;
    }
  }

  fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<ListState<Self>>) {
    let run = self
      .filtered
      .get(self.selected)
      .map(|&i| self.all[i].run.clone());
    window.close_dialog(cx);
    if let Some(run) = run {
      run(window, cx);
    }
  }

  fn cancel(&mut self, window: &mut Window, cx: &mut Context<ListState<Self>>) {
    window.close_dialog(cx);
  }
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
}
