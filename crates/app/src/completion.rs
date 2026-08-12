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

#[cfg(test)]
mod tests {
  use gpui::{AppContext as _, Entity, TestAppContext, VisualTestContext};
  use lsp_types::CompletionTriggerKind;
  use soquel_core::connectors::{ColumnInfo, SchemaInfo, TableInfo, TableKind};

  use super::*;

  fn col(name: &str, data_type: &str) -> ColumnInfo {
    ColumnInfo {
      name: name.to_string(),
      data_type: data_type.to_string(),
      nullable: true,
      default: None,
    }
  }

  fn table(name: &str, estimated_rows: f64, columns: Vec<ColumnInfo>) -> TableInfo {
    TableInfo {
      name: name.to_string(),
      kind: TableKind::Table,
      estimated_rows,
      columns,
      primary_key: Vec::new(),
      indexes: Vec::new(),
      foreign_keys: Vec::new(),
    }
  }

  fn snapshot() -> SchemaSnapshot {
    SchemaSnapshot {
      schemas: vec![SchemaInfo {
        name: "app".to_string(),
        tables: vec![
          table(
            "users",
            1234.,
            vec![col("id", "int4"), col("email", "text")],
          ),
          // A never-analyzed table reports -1: the detail must not echo it.
          table("events", -1., vec![col("id", "int8")]),
        ],
      }],
    }
  }

  fn editor(cx: &mut TestAppContext) -> (Entity<InputState>, &mut VisualTestContext) {
    cx.update(gpui_component::init);
    let cx = cx.add_empty_window();
    let input = cx.update(|window, cx| cx.new(|cx| InputState::new(window, cx)));
    (input, cx)
  }

  /// Runs the provider the way the editor does: cursor at the end of `text`.
  async fn complete(
    provider: &SqlCompletionProvider,
    input: &Entity<InputState>,
    text: &str,
    cx: &mut VisualTestContext,
  ) -> Vec<CompletionItem> {
    let rope = Rope::from(text);
    let offset = text.len();
    let context = CompletionContext {
      trigger_kind: CompletionTriggerKind::INVOKED,
      trigger_character: None,
    };
    let task = cx.update(|window, app| {
      input.update(app, |_, cx| {
        provider.completions(&rope, offset, context, window, cx)
      })
    });
    match task.await.expect("completions") {
      CompletionResponse::Array(items) => items,
      other => panic!("unexpected response shape: {other:?}"),
    }
  }

  #[gpui::test]
  async fn offers_keywords_tables_and_columns_by_prefix(cx: &mut TestAppContext) {
    let (provider, entries) = SqlCompletionProvider::new();
    entries.fill(&snapshot());
    let (input, cx) = editor(cx);

    // Case-insensitive on the typed prefix.
    let items = complete(&provider, &input, "sel", cx).await;
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].label, "SELECT");
    assert_eq!(items[0].kind, Some(CompletionItemKind::KEYWORD));

    // Tables complete as schema.table; the dot is part of the word.
    let items = complete(&provider, &input, "select * from app.us", cx).await;
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].label, "app.users");
    assert_eq!(items[0].kind, Some(CompletionItemKind::CLASS));
    assert_eq!(items[0].detail.as_deref(), Some("table, ~1234 rows"));

    // A never-analyzed estimate clamps to zero.
    let items = complete(&provider, &input, "app.ev", cx).await;
    assert_eq!(items[0].detail.as_deref(), Some("table, ~0 rows"));

    // Columns carry their table and type.
    let items = complete(&provider, &input, "select ema", cx).await;
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].label, "email");
    assert_eq!(items[0].kind, Some(CompletionItemKind::FIELD));
    assert_eq!(items[0].detail.as_deref(), Some("users.email text"));
  }

  #[gpui::test]
  async fn short_prefixes_offer_nothing_and_matches_cap_at_fifty(cx: &mut TestAppContext) {
    let (provider, entries) = SqlCompletionProvider::new();
    let columns = (0..60)
      .map(|ix| col(&format!("col_{ix:02}"), "text"))
      .collect();
    entries.fill(&SchemaSnapshot {
      schemas: vec![SchemaInfo {
        name: "app".to_string(),
        tables: vec![table("wide", 1., columns)],
      }],
    });
    let (input, cx) = editor(cx);

    assert!(complete(&provider, &input, "s", cx).await.is_empty());
    assert!(complete(&provider, &input, "select s", cx).await.is_empty());
    assert_eq!(complete(&provider, &input, "col", cx).await.len(), 50);
  }

  #[gpui::test]
  async fn the_edit_replaces_the_typed_word_in_place(cx: &mut TestAppContext) {
    let (provider, entries) = SqlCompletionProvider::new();
    entries.fill(&snapshot());
    let (input, cx) = editor(cx);

    let text = "select * from app.us";
    let items = complete(&provider, &input, text, cx).await;
    let Some(CompletionTextEdit::Edit(edit)) = &items[0].text_edit else {
      panic!("expected a text edit");
    };
    // The edit spans the whole word, dot included, on the cursor's line.
    assert_eq!(edit.range.start.line, 0);
    assert_eq!(edit.range.start.character, "select * from ".len() as u32);
    assert_eq!(edit.range.end.character, text.len() as u32);
    assert_eq!(edit.new_text, "app.users");
    assert_eq!(items[0].filter_text.as_deref(), Some("app.us"));
  }

  #[gpui::test]
  fn the_trigger_follows_word_chars(cx: &mut TestAppContext) {
    let (provider, _) = SqlCompletionProvider::new();
    let (input, cx) = editor(cx);
    cx.update(|_, app| {
      input.update(app, |_, cx| {
        assert!(provider.is_completion_trigger(0, "a", cx));
        assert!(provider.is_completion_trigger(0, "_", cx));
        assert!(provider.is_completion_trigger(0, "app.", cx));
        assert!(!provider.is_completion_trigger(0, " ", cx));
        assert!(!provider.is_completion_trigger(0, "", cx));
      });
    });
  }
}
