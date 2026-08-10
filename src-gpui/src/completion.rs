use std::cell::RefCell;
use std::rc::Rc;

use anyhow::Result;
use gpui::{Context, Task, Window};
use gpui_component::input::{CompletionProvider, InputState, Rope, RopeExt};
use lsp_types::{
  CompletionContext, CompletionItem, CompletionItemKind, CompletionResponse, CompletionTextEdit,
  TextEdit,
};
use soquel_core::connectors::SchemaSnapshot;

const KEYWORDS: &[&str] = &[
  "SELECT",
  "FROM",
  "WHERE",
  "JOIN",
  "LEFT JOIN",
  "INNER JOIN",
  "ON",
  "GROUP BY",
  "ORDER BY",
  "LIMIT",
  "OFFSET",
  "INSERT INTO",
  "VALUES",
  "UPDATE",
  "SET",
  "DELETE FROM",
  "AND",
  "OR",
  "NOT",
  "NULL",
  "AS",
  "DISTINCT",
  "HAVING",
  "UNION",
  "COUNT",
  "EXPLAIN ANALYZE",
];

#[derive(Clone)]
struct Entry {
  label: String,
  kind: CompletionItemKind,
  detail: Option<String>,
}

pub struct SqlCompletionProvider {
  entries: Rc<RefCell<Vec<Entry>>>,
}

#[derive(Clone)]
pub struct SchemaEntries(Rc<RefCell<Vec<Entry>>>);

impl SchemaEntries {
  pub fn fill(&self, snapshot: &SchemaSnapshot) {
    let mut entries: Vec<Entry> = KEYWORDS
      .iter()
      .map(|kw| Entry {
        label: (*kw).to_string(),
        kind: CompletionItemKind::KEYWORD,
        detail: None,
      })
      .collect();
    for schema in &snapshot.schemas {
      for table in &schema.tables {
        entries.push(Entry {
          label: format!("{}.{}", schema.name, table.name),
          kind: CompletionItemKind::CLASS,
          detail: Some(format!(
            "table, ~{} rows",
            table.estimated_rows.max(0.) as i64
          )),
        });
        for column in &table.columns {
          entries.push(Entry {
            label: column.name.clone(),
            kind: CompletionItemKind::FIELD,
            detail: Some(format!(
              "{}.{} {}",
              table.name, column.name, column.data_type
            )),
          });
        }
      }
    }
    entries.dedup_by(|a, b| a.label == b.label && a.kind == b.kind);
    *self.0.borrow_mut() = entries;
  }
}

impl SqlCompletionProvider {
  pub fn new() -> (Rc<Self>, SchemaEntries) {
    let entries = Rc::new(RefCell::new(Vec::new()));
    (
      Rc::new(Self {
        entries: entries.clone(),
      }),
      SchemaEntries(entries),
    )
  }
}

fn is_word_char(c: char) -> bool {
  c.is_alphanumeric() || c == '_' || c == '.'
}

impl CompletionProvider for SqlCompletionProvider {
  fn completions(
    &self,
    rope: &Rope,
    offset: usize,
    _: CompletionContext,
    _: &mut Window,
    _: &mut Context<InputState>,
  ) -> Task<Result<CompletionResponse>> {
    let mut start = offset;
    while start > 0 {
      match rope.char_at(start - 1) {
        Some(c) if is_word_char(c) => start -= 1,
        _ => break,
      }
    }
    let prefix = rope.slice(start..offset).to_string();
    if prefix.len() < 2 {
      return Task::ready(Ok(CompletionResponse::Array(vec![])));
    }

    let start_pos = rope.offset_to_position(start);
    let end_pos = rope.offset_to_position(offset);
    let needle = prefix.to_lowercase();

    let items = self
      .entries
      .borrow()
      .iter()
      .filter(|entry| entry.label.to_lowercase().starts_with(&needle))
      .take(50)
      .map(|entry| CompletionItem {
        label: entry.label.clone(),
        filter_text: Some(prefix.clone()),
        kind: Some(entry.kind),
        detail: entry.detail.clone(),
        text_edit: Some(CompletionTextEdit::Edit(TextEdit {
          range: lsp_types::Range {
            start: start_pos,
            end: end_pos,
          },
          new_text: entry.label.clone(),
        })),
        ..Default::default()
      })
      .collect();

    Task::ready(Ok(CompletionResponse::Array(items)))
  }

  fn is_completion_trigger(&self, _: usize, new_text: &str, _: &mut Context<InputState>) -> bool {
    new_text.chars().last().is_some_and(is_word_char)
  }
}
