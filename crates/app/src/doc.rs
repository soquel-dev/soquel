//! The mongo document browser: a whole workspace of its own, mounted when a
//! connection browses documents.

use std::sync::Arc;

use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::resizable::{ResizableState, h_resizable, resizable_panel};
use gpui_component::select::{Select, SelectEvent, SelectState};
use gpui_component::text::TextView;
use gpui_component::{
  ActiveTheme, Disableable, Icon, IconName, IndexPath, Selectable, Sizable, StyledExt, h_flex,
  v_flex,
};
use soquel_core::AppState;
use soquel_core::connectors::{
  DocCollection, DocCollectionKind, DocCount, DocDetail, DocEntry, DocFindRequest, IndexInfo,
};
use soquel_core::profiles::ConnectionProfile;

use crate::actions::{FocusEditor, RefreshSchema};
use crate::core::{self, Db};

pub fn doc_kind_badge(kind: DocCollectionKind, cx: &App) -> (&'static str, Hsla) {
  match kind {
    DocCollectionKind::Collection => ("coll", cx.theme().blue),
    DocCollectionKind::View => ("view", cx.theme().magenta),
    DocCollectionKind::Timeseries => ("ts", cx.theme().green),
    DocCollectionKind::Other => ("?", cx.theme().muted_foreground),
  }
}

/// Human label for an extended-JSON value: single-key `$`-wrappers unwrapped
/// ($oid hex, $numberLong digits, $date ISO), documents compacted to {k: v}.
fn extjson_label(value: &serde_json::Value) -> String {
  use serde_json::Value;
  match value {
    Value::Null => "null".to_string(),
    Value::Bool(b) => b.to_string(),
    Value::Number(n) => n.to_string(),
    Value::String(s) => s.clone(),
    Value::Array(items) => format!(
      "[{}]",
      items
        .iter()
        .map(extjson_label)
        .collect::<Vec<_>>()
        .join(", ")
    ),
    Value::Object(map) => {
      if map.len() == 1 {
        let (key, inner) = map.iter().next().unwrap();
        if key.starts_with('$') {
          if key == "$binary" {
            let base64 = inner
              .get("base64")
              .and_then(Value::as_str)
              .unwrap_or_default();
            let shown = if base64.chars().count() > 12 {
              format!("{}…", base64.chars().take(12).collect::<String>())
            } else {
              base64.to_string()
            };
            return format!("bin({shown})");
          }
          if key == "$date" {
            // Relaxed extjson dates are ISO strings; canonical wraps millis.
            return extjson_label(inner);
          }
          return extjson_label(inner);
        }
      }
      format!(
        "{{{}}}",
        map
          .iter()
          .map(|(k, v)| format!("{k}: {}", extjson_label(v)))
          .collect::<Vec<_>>()
          .join(", ")
      )
    }
  }
}

/// Row label for a document's canonical extjson `_id`.
pub fn doc_id_label(id: Option<&str>) -> String {
  let Some(id) = id else {
    return "no _id".to_string();
  };
  match serde_json::from_str::<serde_json::Value>(id) {
    Ok(value) => extjson_label(&value),
    Err(_) => id.to_string(),
  }
}

/// One-line preview of a relaxed extjson document, `_id` excluded.
pub fn doc_preview(doc: &str, max: usize) -> String {
  match serde_json::from_str::<serde_json::Value>(doc) {
    Ok(serde_json::Value::Object(map)) => {
      let preview = map
        .iter()
        .filter(|(key, _)| key.as_str() != "_id")
        .map(|(key, value)| format!("{key}: {}", extjson_label(value)))
        .collect::<Vec<_>>()
        .join("  ");
      if preview.is_empty() {
        "(empty document)".to_string()
      } else {
        truncate(&preview, max)
      }
    }
    _ => truncate(doc, max),
  }
}

fn truncate(text: &str, max: usize) -> String {
  if text.chars().count() > max {
    format!("{}…", text.chars().take(max - 1).collect::<String>())
  } else {
    text.to_string()
  }
}

/// "1.2k", "45m" for collection counts and estimates.
pub fn compact_count(value: f64) -> String {
  let abs = value.abs();
  let (scaled, suffix) = if abs >= 1e12 {
    (value / 1e12, "t")
  } else if abs >= 1e9 {
    (value / 1e9, "b")
  } else if abs >= 1e6 {
    (value / 1e6, "m")
  } else if abs >= 1e3 {
    (value / 1e3, "k")
  } else {
    return (value.round() as i64).to_string();
  };
  let text = format!("{scaled:.1}");
  let text = text.strip_suffix(".0").unwrap_or(&text);
  format!("{text}{suffix}")
}

/// Estimates carry a ~; exact counts don't.
pub fn format_doc_count(count: f64, exact: bool) -> String {
  let label = if count == 1.0 { "doc" } else { "docs" };
  if exact {
    format!("{} {label}", count as i64)
  } else {
    format!("~{} {label}", compact_count(count))
  }
}

pub fn format_bytes(bytes: f64) -> String {
  if bytes < 1024.0 {
    return format!("{} B", bytes.round() as i64);
  }
  let units = ["KB", "MB", "GB", "TB"];
  let mut value = bytes;
  let mut unit = 0usize;
  loop {
    value /= 1024.0;
    if value < 1024.0 || unit == units.len() - 1 {
      break;
    }
    unit += 1;
  }
  if value >= 10.0 {
    format!("{} {}", value.round() as i64, units[unit])
  } else {
    format!("{value:.1} {}", units[unit])
  }
}

const DOC_PAGE: u32 = 100;

#[derive(Clone, Copy, PartialEq, Eq)]
enum DocView {
  Documents,
  Indexes,
  Console,
}

struct ConsoleEntry {
  prompt: String,
  source: String,
  lines: Vec<String>,
  summary: Option<String>,
  ok: bool,
}

pub enum DocWorkspaceEvent {
  Close,
}

pub struct DocWorkspace {
  focus_handle: FocusHandle,
  db: Db,
  name: SharedString,
  server_version: Option<String>,
  sidebar_split: Entity<ResizableState>,
  list_detail_split: Entity<ResizableState>,
  db_select: Entity<SelectState<Vec<String>>>,
  db_names: Vec<String>,
  doc_db: Option<String>,
  collection_filter: Entity<InputState>,
  collections: Vec<DocCollection>,
  collections_seq: u64,
  selected_collection: Option<String>,
  doc_filter: Entity<InputState>,
  filter_error: Option<SharedString>,
  docs: Vec<DocEntry>,
  doc_cursor: Option<String>,
  doc_loading: bool,
  doc_count: Option<DocCount>,
  find_seq: u64,
  selected: Option<usize>,
  detail: Option<DocDetail>,
  doc_editor: Entity<InputState>,
  editing: bool,
  delete_armed: bool,
  view: DocView,
  indexes: Vec<IndexInfo>,
  console_input: Entity<InputState>,
  console_log: Vec<ConsoleEntry>,
  status: SharedString,
  _subscriptions: Vec<Subscription>,
  // One slot per concern: a shared slot would cancel a sibling refresh
  // before its first poll (a save triggers find + detail together).
  _collections_task: Task<()>,
  _find_task: Task<()>,
  _detail_task: Task<()>,
  _op_task: Task<()>,
}

impl EventEmitter<DocWorkspaceEvent> for DocWorkspace {}

impl Focusable for DocWorkspace {
  fn focus_handle(&self, _: &App) -> FocusHandle {
    self.focus_handle.clone()
  }
}

impl DocWorkspace {
  pub fn new(
    _state: Arc<AppState>,
    db: Db,
    profile: ConnectionProfile,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) -> Self {
    let server_version = db.server_version();
    let focus_handle = cx.focus_handle();
    window.focus(&focus_handle, cx);
    let doc_db = match &profile.params {
      soquel_core::profiles::ConnectorParams::Mongo(params) => params.database.clone(),
      _ => None,
    };
    let collection_filter =
      cx.new(|cx| InputState::new(window, cx).placeholder("filter collections"));
    let doc_filter =
      cx.new(|cx| InputState::new(window, cx).placeholder("filter { \"plan\": \"pro\" }"));
    let doc_editor = cx.new(|cx| InputState::new(window, cx).multi_line(true));
    let console_input = cx.new(|cx| {
      InputState::new(window, cx).placeholder("{ \"plan\": \"pro\" } or [{ \"$group\": … }]")
    });
    let db_select = cx.new(|cx| SelectState::new(Vec::<String>::new(), None, window, cx));
    let sidebar_split = cx.new(|_| ResizableState::default());
    let list_detail_split = cx.new(|_| ResizableState::default());

    let subscriptions = vec![
      cx.subscribe(&collection_filter, |this, _, event: &InputEvent, cx| {
        if matches!(event, InputEvent::Change) {
          cx.notify();
          let _ = this;
        }
      }),
      cx.subscribe(&doc_filter, |this, _, event: &InputEvent, cx| {
        if matches!(event, InputEvent::PressEnter { .. }) {
          this.find(true, cx);
        }
      }),
      cx.subscribe_in(
        &console_input,
        window,
        |this, _, event: &InputEvent, window, cx| {
          if matches!(event, InputEvent::PressEnter { .. }) {
            this.run_console(window, cx);
          }
        },
      ),
      cx.subscribe_in(
        &db_select,
        window,
        |this, _, _: &SelectEvent<Vec<String>>, _, cx| {
          this.on_db_selected(cx);
        },
      ),
    ];

    let mut this = Self {
      focus_handle,
      db,
      name: profile.name.clone().into(),
      server_version,
      sidebar_split,
      list_detail_split,
      db_select,
      db_names: Vec::new(),
      doc_db,
      collection_filter,
      collections: Vec::new(),
      collections_seq: 0,
      selected_collection: None,
      doc_filter,
      filter_error: None,
      docs: Vec::new(),
      doc_cursor: None,
      doc_loading: false,
      doc_count: None,
      find_seq: 0,
      selected: None,
      detail: None,
      doc_editor,
      editing: false,
      delete_armed: false,
      view: DocView::Documents,
      indexes: Vec::new(),
      console_input,
      console_log: Vec::new(),
      status: SharedString::default(),
      _subscriptions: subscriptions,
      _collections_task: Task::ready(()),
      _find_task: Task::ready(()),
      _detail_task: Task::ready(()),
      _op_task: Task::ready(()),
    };
    this.load_databases(cx);
    this.load_collections(cx);
    this
  }

  fn load_databases(&mut self, cx: &mut Context<Self>) {
    let task = core::doc_databases(&self.db, cx);
    let db_select = self.db_select.clone();
    cx.spawn(async move |this, cx| {
      let Some(databases) = crate::status::ok_or_log(task.await) else {
        return;
      };
      let Some(handle) = cx.update(|cx| cx.active_window()) else {
        return;
      };
      let _ = cx.update_window(handle, move |_, window, cx| {
        let names: Vec<String> = databases.iter().map(|d| d.name.clone()).collect();
        let labels: Vec<String> = databases
          .iter()
          .map(|d| match d.size_bytes {
            Some(bytes) => format!("{}  {}", d.name, format_bytes(bytes)),
            None => d.name.clone(),
          })
          .collect();
        this
          .update(cx, |this, cx| {
            this.db_names = names.clone();
            // No database on the profile: default to the first one, and load
            // what selecting it by hand would have loaded.
            if this.doc_db.is_none() {
              this.doc_db = names.first().cloned();
              if this.doc_db.is_some() {
                this.load_collections(cx);
              }
            }
            cx.notify();
          })
          .ok();
        let Ok(current) = this.read_with(cx, |view, _| {
          view
            .doc_db
            .as_ref()
            .and_then(|db| names.iter().position(|n| n == db))
        }) else {
          return;
        };
        db_select.update(cx, |select, cx| {
          select.set_items(labels, window, cx);
          if let Some(ix) = current {
            select.set_selected_index(Some(IndexPath::new(ix)), window, cx);
          }
        });
      });
    })
    .detach();
  }

  fn on_db_selected(&mut self, cx: &mut Context<Self>) {
    let ix = self
      .db_select
      .read(cx)
      .selected_index(cx)
      .map_or(0, |ix| ix.row);
    let Some(name) = self.db_names.get(ix).cloned() else {
      return;
    };
    if self.doc_db.as_ref() == Some(&name) {
      return;
    }
    // No reconnect: mongo addresses any db from one client.
    self.doc_db = Some(name);
    self.selected_collection = None;
    self.selected = None;
    self.detail = None;
    self.docs.clear();
    self.load_collections(cx);
  }

  fn load_collections(&mut self, cx: &mut Context<Self>) {
    let Some(db) = self.doc_db.clone() else {
      return;
    };
    self.collections_seq += 1;
    let seq = self.collections_seq;
    let task = core::doc_collections(&self.db, db, cx);
    self._collections_task = cx.spawn(async move |this, cx| {
      let result = task.await;
      let _ = this.update(cx, |this, cx| {
        if this.collections_seq != seq {
          return;
        }
        match result {
          Ok(collections) => this.collections = collections,
          Err(error) => this.status = crate::status::error(&error),
        }
        cx.notify();
      });
    });
  }

  fn select_collection(&mut self, collection: String, cx: &mut Context<Self>) {
    self.selected_collection = Some(collection.clone());
    self.selected = None;
    self.detail = None;
    self.editing = false;
    self.view = DocView::Documents;
    self.find(true, cx);
    self.load_indexes(cx);
  }

  fn applied_filter(&self, cx: &App) -> Option<String> {
    let typed = self.doc_filter.read(cx).value().trim().to_string();
    (!typed.is_empty()).then_some(typed)
  }

  fn find(&mut self, reset: bool, cx: &mut Context<Self>) {
    let (Some(db), Some(collection)) = (self.doc_db.clone(), self.selected_collection.clone())
    else {
      return;
    };
    if reset {
      self.doc_cursor = None;
    }
    self.doc_loading = true;
    self.filter_error = None;
    self.find_seq += 1;
    let seq = self.find_seq;
    cx.notify();
    let request = DocFindRequest {
      db: db.clone(),
      collection: collection.clone(),
      filter: self.applied_filter(cx),
      sort: None,
      limit: DOC_PAGE,
      cursor: self.doc_cursor.clone(),
    };
    let task = core::doc_find(&self.db, request, cx);
    let filter = self.applied_filter(cx);
    let count_task = reset.then(|| core::doc_count(&self.db, db, collection, filter, cx));
    self._find_task = cx.spawn(async move |this, cx| {
      let result = task.await;
      let count = match count_task {
        Some(task) => crate::status::ok_or_log(task.await),
        None => None,
      };
      let _ = this.update(cx, |this, cx| {
        if this.find_seq != seq {
          return;
        }
        this.doc_loading = false;
        match result {
          Ok(page) => {
            this.doc_cursor = page.cursor;
            if reset {
              this.docs = page.docs;
              this.doc_count = count;
            } else {
              this.docs.extend(page.docs);
            }
          }
          Err(error) => this.filter_error = Some(crate::status::message(&error)),
        }
        cx.notify();
      });
    });
  }

  fn load_indexes(&mut self, cx: &mut Context<Self>) {
    let (Some(db), Some(collection)) = (self.doc_db.clone(), self.selected_collection.clone())
    else {
      return;
    };
    let task = core::doc_indexes(&self.db, db, collection, cx);
    cx.spawn(async move |this, cx| {
      if let Some(indexes) = crate::status::ok_or_log(task.await) {
        let _ = this.update(cx, |this, cx| {
          this.indexes = indexes;
          cx.notify();
        });
      }
    })
    .detach();
  }

  fn select_doc(&mut self, index: usize, cx: &mut Context<Self>) {
    let Some(entry) = self.docs.get(index).cloned() else {
      return;
    };
    self.selected = Some(index);
    self.detail = None;
    self.editing = false;
    self.delete_armed = false;
    cx.notify();
    let (Some(db), Some(collection), Some(id)) = (
      self.doc_db.clone(),
      self.selected_collection.clone(),
      entry.id.clone(),
    ) else {
      // A doc with no `_id` (a view / projected key) has no address to fetch.
      return;
    };
    let task = core::doc_detail(&self.db, db, collection, id, cx);
    self._detail_task = cx.spawn(async move |this, cx| {
      let result = task.await;
      let _ = this.update(cx, |this, cx| {
        match result {
          Ok(detail) => this.detail = Some(detail),
          Err(error) => this.status = crate::status::error(&error),
        }
        cx.notify();
      });
    });
  }

  fn start_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
    let Some(detail) = &self.detail else {
      return;
    };
    let canonical = pretty_json(&detail.canonical);
    self.editing = true;
    self
      .doc_editor
      .update(cx, |input, cx| input.set_value(canonical, window, cx));
    cx.notify();
  }

  fn save_doc(&mut self, cx: &mut Context<Self>) {
    let (Some(db), Some(collection), Some(detail)) = (
      self.doc_db.clone(),
      self.selected_collection.clone(),
      self.detail.clone(),
    ) else {
      return;
    };
    let Some(id) = detail.id.clone() else {
      return;
    };
    let draft = self.doc_editor.read(cx).value().to_string();
    let task = core::doc_replace(&self.db, db, collection, id.clone(), draft, cx);
    self._op_task = cx.spawn(async move |this, cx| {
      let result = task.await;
      let _ = this.update(cx, |this, cx| {
        match result {
          Ok(()) => {
            this.editing = false;
            let index = this.selected;
            this.find(true, cx);
            if let Some(ix) = index {
              this.select_doc(ix, cx);
            }
          }
          Err(error) => this.status = crate::status::error(&error),
        }
        cx.notify();
      });
    });
  }

  fn delete_doc(&mut self, cx: &mut Context<Self>) {
    let (Some(db), Some(collection), Some(detail)) = (
      self.doc_db.clone(),
      self.selected_collection.clone(),
      self.detail.clone(),
    ) else {
      return;
    };
    let Some(id) = detail.id.clone() else {
      return;
    };
    let task = core::doc_delete(&self.db, db, collection, id, cx);
    self._op_task = cx.spawn(async move |this, cx| {
      let result = task.await;
      let _ = this.update(cx, |this, cx| {
        match result {
          Ok(()) => {
            this.selected = None;
            this.detail = None;
            this.delete_armed = false;
            this.find(true, cx);
            this.load_collections(cx);
          }
          Err(error) => this.status = crate::status::error(&error),
        }
        cx.notify();
      });
    });
  }

  fn run_console(&mut self, _: &mut Window, cx: &mut Context<Self>) {
    let (Some(db), Some(collection)) = (self.doc_db.clone(), self.selected_collection.clone())
    else {
      return;
    };
    let source = self.console_input.read(cx).value().trim().to_string();
    if source.is_empty() {
      return;
    }
    let prompt = format!("{db}.{collection}");
    let task = core::doc_run_query(&self.db, db, collection, source.clone(), cx);
    let input = self.console_input.clone();
    self._op_task = cx.spawn(async move |this, cx| {
      let result = task.await;
      let Some(handle) = cx.update(|cx| cx.active_window()) else {
        return;
      };
      let _ = cx.update_window(handle, move |_, window, cx| {
        let entry = match result {
          Ok(query) => {
            let mut summary = vec![format!(
              "{} doc{}",
              query.docs.len(),
              if query.docs.len() == 1 { "" } else { "s" }
            )];
            if query.truncated {
              summary.push("truncated".to_string());
            }
            summary.push(format!("{} ms", (query.duration_ms.round() as i64).max(1)));
            ConsoleEntry {
              prompt: prompt.clone(),
              source: source.clone(),
              lines: query.docs,
              summary: Some(summary.join(" · ")),
              ok: true,
            }
          }
          Err(error) => ConsoleEntry {
            prompt: prompt.clone(),
            source: source.clone(),
            lines: vec![format!("{error}")],
            summary: None,
            ok: false,
          },
        };
        input.update(cx, |input, cx| input.set_value("", window, cx));
        this
          .update(cx, |this, cx| {
            this.console_log.push(entry);
            cx.notify();
          })
          .ok();
      });
    });
  }
}

fn pretty_json(text: &str) -> String {
  serde_json::from_str::<serde_json::Value>(text)
    .and_then(|value| serde_json::to_string_pretty(&value))
    .unwrap_or_else(|_| text.to_string())
}

impl DocWorkspace {
  fn kind_badge(&self, kind: DocCollectionKind, cx: &App) -> Div {
    let (short, color) = doc_kind_badge(kind, cx);
    crate::ui::tinted_badge(short, color, cx)
  }

  pub(crate) fn footer_connection(&self) -> String {
    match &self.server_version {
      Some(version) => {
        let (engine, version) =
          crate::connections::server_badge(soquel_core::profiles::ConnectorKind::Mongo, version);
        format!("{} - {engine} {version}", self.name)
      }
      None => self.name.to_string(),
    }
  }

  fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
    let filter = self.collection_filter.read(cx).value().to_lowercase();
    let shown: Vec<DocCollection> = self
      .collections
      .iter()
      .filter(|c| filter.is_empty() || c.name.to_lowercase().contains(&filter))
      .cloned()
      .collect();
    let count = self.collections.len();
    v_flex()
      .size_full()
      .child(
        h_flex()
          .px_2()
          .py_2()
          .justify_between()
          .items_center()
          .child(
            Button::new("doc-back")
              .ghost()
              .xsmall()
              .icon(Icon::new(IconName::ChevronLeft))
              .label("Connections")
              .on_click(cx.listener(|_, _, _, cx| cx.emit(DocWorkspaceEvent::Close))),
          )
          .child(
            Button::new("doc-refresh")
              .ghost()
              .xsmall()
              .icon(Icon::new(crate::icons::SoquelIcon::RefreshCw))
              .on_click(cx.listener(|this, _, _, cx| {
                this.load_databases(cx);
                this.load_collections(cx);
              })),
          ),
      )
      .child(
        h_flex()
          .px_2()
          .py_1p5()
          .justify_between()
          .items_center()
          .border_b_1()
          .border_color(cx.theme().border)
          .child(
            div()
              .text_xs()
              .font_semibold()
              .text_color(cx.theme().muted_foreground)
              .child("collections"),
          )
          .child(
            div()
              .w(px(150.))
              .child(Select::new(&self.db_select).small()),
          ),
      )
      .child(
        div()
          .px_2()
          .py_1p5()
          .child(Input::new(&self.collection_filter).small()),
      )
      .child(
        v_flex()
          .id("doc-collections")
          .flex_1()
          .min_h_0()
          .overflow_y_scroll()
          .children(shown.into_iter().map(|collection| {
            let name = collection.name.clone();
            let selected = self.selected_collection.as_deref() == Some(collection.name.as_str());
            crate::ui::list_row(
              SharedString::from(format!("collection-{}", collection.name)),
              selected,
              cx,
            )
            .text_xs()
            .font_family(crate::theme::mono(cx))
            .on_click(cx.listener(move |this, _, _, cx| {
              this.select_collection(name.clone(), cx);
            }))
            .child(self.kind_badge(collection.kind, cx))
            .child(
              div()
                .flex_1()
                .min_w_0()
                .truncate()
                .child(collection.name.clone()),
            )
            .when_some(collection.estimated_docs, |row, n| {
              row.child(
                div()
                  .text_color(cx.theme().muted_foreground)
                  .child(format!("~{}", compact_count(n))),
              )
            })
          })),
      )
      .child(
        h_flex().px_2().py_1().child(
          div()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(format!(
              "{count} collection{}",
              if count == 1 { "" } else { "s" }
            )),
        ),
      )
  }

  fn render_doc_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
    if self.selected_collection.is_none() {
      return v_flex()
        .size_full()
        .items_center()
        .justify_center()
        .text_sm()
        .font_family(crate::theme::mono(cx))
        .text_color(cx.theme().muted_foreground)
        .child("soquel=# select a collection")
        .into_any_element();
    }
    let count_label = self
      .doc_count
      .as_ref()
      .map(|c| format_doc_count(c.count, c.exact));
    v_flex()
      .size_full()
      .child(
        v_flex()
          .px_2()
          .py_1p5()
          .gap_1()
          .border_b_1()
          .border_color(cx.theme().border)
          .child(
            h_flex()
              .gap_1()
              .child(div().flex_1().child(Input::new(&self.doc_filter).small()))
              .child(
                Button::new("doc-run")
                  .ghost()
                  .xsmall()
                  .label("find")
                  .on_click(cx.listener(|this, _, _, cx| this.find(true, cx))),
              ),
          )
          .when_some(self.filter_error.clone(), |col, error| {
            col.child(
              div()
                .text_xs()
                .font_family(crate::theme::mono(cx))
                .text_color(cx.theme().danger)
                .child(error),
            )
          }),
      )
      .child(
        uniform_list(
          "doc-rows",
          self.docs.len(),
          cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
            range
              .map(|ix| {
                let entry = &this.docs[ix];
                let selected = this.selected == Some(ix);
                crate::ui::list_row(ix, selected, cx)
                  .on_click(cx.listener(move |this, _, _, cx| this.select_doc(ix, cx)))
                  .child(
                    v_flex()
                      .flex_1()
                      .min_w_0()
                      .text_xs()
                      .font_family(crate::theme::mono(cx))
                      .child(div().truncate().child(doc_id_label(entry.id.as_deref())))
                      .child(
                        div()
                          .truncate()
                          .text_color(cx.theme().muted_foreground)
                          .child(doc_preview(&entry.doc, 140)),
                      ),
                  )
              })
              .collect::<Vec<_>>()
          }),
        )
        .flex_1()
        .min_h_0(),
      )
      .child(if self.doc_cursor.is_some() {
        h_flex().p_1().child(
          Button::new("doc-more")
            .ghost()
            .xsmall()
            .w_full()
            .label(if self.doc_loading {
              "loading…"
            } else {
              "load more"
            })
            .on_click(cx.listener(|this, _, _, cx| this.find(false, cx))),
        )
      } else {
        h_flex().px_2().py_1().child(
          div()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(count_label.unwrap_or_else(|| "no documents match".to_string())),
        )
      })
      .into_any_element()
  }

  fn render_detail(&self, cx: &mut Context<Self>) -> impl IntoElement {
    let Some(index) = self.selected else {
      return v_flex()
        .size_full()
        .items_center()
        .justify_center()
        .text_sm()
        .font_family(crate::theme::mono(cx))
        .text_color(cx.theme().muted_foreground)
        .child("soquel=# select a document")
        .into_any_element();
    };
    let Some(entry) = self.docs.get(index) else {
      return div().into_any_element();
    };
    let addressable = entry.id.is_some();
    let relaxed = self
      .detail
      .as_ref()
      .map(|d| d.relaxed.clone())
      .unwrap_or_else(|| entry.doc.clone());
    let pretty = pretty_json(&relaxed);

    v_flex()
      .size_full()
      .child(
        h_flex()
          .px_3()
          .py_2()
          .gap_2()
          .items_center()
          .border_b_1()
          .border_color(cx.theme().border)
          .child(
            div()
              .flex_1()
              .min_w_0()
              .truncate()
              .font_family(crate::theme::mono(cx))
              .text_sm()
              .child(doc_id_label(entry.id.as_deref())),
          )
          .when(addressable && !self.editing, |header| {
            header.child(
              Button::new("edit-doc")
                .ghost()
                .xsmall()
                .label("edit")
                .disabled(self.detail.is_none())
                .on_click(cx.listener(|this, _, window, cx| this.start_edit(window, cx))),
            )
          })
          .when(addressable, |header| {
            header.child(if self.delete_armed {
              Button::new("delete-doc")
                .danger()
                .xsmall()
                .label("sure?")
                .on_click(cx.listener(|this, _, _, cx| this.delete_doc(cx)))
            } else {
              Button::new("delete-doc")
                .ghost()
                .xsmall()
                .label("delete")
                .on_click(cx.listener(|this, _, _, cx| {
                  this.delete_armed = true;
                  cx.notify();
                }))
            })
          }),
      )
      .when(!addressable, |body| {
        body.child(
          div()
            .px_3()
            .py_1()
            .text_xs()
            .italic()
            .text_color(cx.theme().muted_foreground)
            .child("no _id on this document - read-only"),
        )
      })
      .child(if self.editing {
        v_flex()
          .flex_1()
          .min_h_0()
          .p_2()
          .gap_2()
          .child(
            div()
              .flex_1()
              .min_h_0()
              .child(Input::new(&self.doc_editor).h_full()),
          )
          .child(
            h_flex()
              .gap_2()
              .child(
                Button::new("save-doc")
                  .primary()
                  .xsmall()
                  .label("Save document")
                  .on_click(cx.listener(|this, _, _, cx| this.save_doc(cx))),
              )
              .child(
                Button::new("cancel-edit")
                  .ghost()
                  .xsmall()
                  .label("Cancel")
                  .on_click(cx.listener(|this, _, _, cx| {
                    this.editing = false;
                    cx.notify();
                  })),
              ),
          )
          .into_any_element()
      } else {
        v_flex()
          .id("doc-json")
          .flex_1()
          .min_h_0()
          .overflow_y_scroll()
          .p_2()
          .text_sm()
          .child(TextView::markdown(
            "doc-json-view",
            format!("```json\n{pretty}\n```"),
          ))
          .into_any_element()
      })
      .into_any_element()
  }

  fn render_indexes(&self, cx: &Context<Self>) -> impl IntoElement {
    if self.selected_collection.is_none() {
      return v_flex()
        .size_full()
        .items_center()
        .justify_center()
        .text_sm()
        .font_family(crate::theme::mono(cx))
        .text_color(cx.theme().muted_foreground)
        .child("soquel=# select a collection")
        .into_any_element();
    }
    v_flex()
      .id("doc-indexes")
      .size_full()
      .overflow_y_scroll()
      .p_2()
      .when(self.indexes.is_empty(), |body| {
        body.child(
          div()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child("no indexes"),
        )
      })
      .children(self.indexes.iter().map(|index| {
        h_flex()
          .py_1()
          .gap_3()
          .border_b_1()
          .border_color(cx.theme().border.opacity(0.4))
          .text_xs()
          .font_family(crate::theme::mono(cx))
          .child(div().w(px(160.)).truncate().child(index.name.clone()))
          .child(
            div()
              .flex_1()
              .min_w_0()
              .text_color(cx.theme().muted_foreground)
              .child(index.definition.clone()),
          )
          .child(div().w(px(56.)).child(if index.unique {
            crate::ui::tinted_badge("unique", cx.theme().yellow, cx)
          } else {
            div()
          }))
      }))
      .into_any_element()
  }

  fn render_console(&self, cx: &Context<Self>) -> impl IntoElement {
    let prompt = match (&self.doc_db, &self.selected_collection) {
      (Some(db), Some(collection)) => format!("{db}.{collection}"),
      _ => "mongo".to_string(),
    };
    let empty = if self.selected_collection.is_some() {
      format!("{prompt}> find filter {{ … }} or pipeline [ … ]")
    } else {
      "mongo> select a collection first".to_string()
    };
    v_flex()
      .size_full()
      .child(
        v_flex()
          .id("doc-console-log")
          .flex_1()
          .min_h_0()
          .overflow_y_scroll()
          .p_3()
          .gap_1()
          .text_xs()
          .font_family(crate::theme::mono(cx))
          .when(self.console_log.is_empty(), |log| {
            log.child(div().text_color(cx.theme().muted_foreground).child(empty))
          })
          .children(self.console_log.iter().map(|entry| {
            v_flex()
              .child(
                h_flex()
                  .gap_1()
                  .child(
                    div()
                      .text_color(cx.theme().muted_foreground)
                      .child(format!("{}>", entry.prompt)),
                  )
                  .child(entry.source.clone()),
              )
              .child(
                div()
                  .whitespace_normal()
                  .when(!entry.ok, |d| d.text_color(cx.theme().danger))
                  .child(entry.lines.join("\n")),
              )
              .when_some(entry.summary.clone(), |block, summary| {
                block.child(div().text_color(cx.theme().muted_foreground).child(summary))
              })
          })),
      )
      .child(
        div()
          .p_2()
          .border_t_1()
          .border_color(cx.theme().border)
          .child(Input::new(&self.console_input).small()),
      )
  }
}

impl Render for DocWorkspace {
  fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    let view = self.view;
    let toggle = |id: &'static str, label: &'static str, target: DocView| {
      Button::new(id)
        .ghost()
        .xsmall()
        .label(label)
        .selected(view == target)
    };

    v_flex()
      .size_full()
      .track_focus(&self.focus_handle)
      .on_action(cx.listener(|this, _: &RefreshSchema, _, cx| {
        this.load_collections(cx);
        this.find(true, cx);
        this.load_indexes(cx);
      }))
      .on_action(cx.listener(|this, _: &FocusEditor, window, cx| {
        this.view = DocView::Console;
        this
          .console_input
          .update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
      }))
      .bg(crate::theme::canvas(cx))
      .when(!self.status.is_empty(), |this| {
        this.child(
          div()
            .px_4()
            .py_1()
            .text_sm()
            .text_color(cx.theme().danger)
            .child(self.status.clone()),
        )
      })
      .child(
        h_flex().flex_1().min_h_0().child(
          h_resizable("doc-sidebar-main")
            .with_state(&self.sidebar_split)
            .child(
              resizable_panel()
                .size(px(260.))
                .size_range(px(180.)..px(520.))
                .child(self.render_sidebar(cx).into_any_element()),
            )
            .child(
              resizable_panel().child(
                v_flex()
                  .size_full()
                  .bg(crate::theme::panel(cx))
                  .border_l_1()
                  .border_color(cx.theme().border)
                  .child(
                    h_flex()
                      .px_2()
                      .py_1()
                      .gap_1()
                      .border_b_1()
                      .border_color(cx.theme().border)
                      .child(
                        toggle("doc-view-docs", "documents", DocView::Documents).on_click(
                          cx.listener(|this, _, _, cx| {
                            this.view = DocView::Documents;
                            cx.notify();
                          }),
                        ),
                      )
                      .child(
                        toggle("doc-view-indexes", "indexes", DocView::Indexes).on_click(
                          cx.listener(|this, _, _, cx| {
                            this.view = DocView::Indexes;
                            cx.notify();
                          }),
                        ),
                      )
                      .child(
                        toggle("doc-view-console", "console", DocView::Console).on_click(
                          cx.listener(|this, _, _, cx| {
                            this.view = DocView::Console;
                            cx.notify();
                          }),
                        ),
                      ),
                  )
                  .child(match view {
                    DocView::Documents => h_resizable("doc-list-detail")
                      .with_state(&self.list_detail_split)
                      .child(
                        resizable_panel()
                          .size(px(340.))
                          .size_range(px(200.)..px(640.))
                          .child(self.render_doc_list(cx).into_any_element()),
                      )
                      .child(resizable_panel().child(self.render_detail(cx).into_any_element()))
                      .into_any_element(),
                    DocView::Indexes => self.render_indexes(cx).into_any_element(),
                    DocView::Console => self.render_console(cx).into_any_element(),
                  }),
              ),
            ),
        ),
      )
  }
}

#[cfg(test)]
mod tests {
  use std::collections::BTreeMap;
  use std::sync::Mutex;

  use ::core::prelude::v1::test;
  use soquel_core::connectors::{Connection, DocBrowse, DocDatabase, DocPage, DocQueryResult};
  use soquel_core::error::Error;
  use soquel_core::profiles::ConnectorKind;

  use super::*;
  use crate::test_support::shell_window;

  #[derive(Clone)]
  struct DocRecord {
    id: Option<String>,
    doc: String,
  }

  type SeedCollections<'a> = &'a [(&'a str, Vec<DocRecord>)];

  /// In-memory document store: every future resolves on its first poll, so the
  /// view runs fully deterministic under `run_until_parked`.
  struct FakeDoc {
    dbs: Mutex<BTreeMap<String, BTreeMap<String, Vec<DocRecord>>>>,
  }

  impl FakeDoc {
    fn new(dbs: &[(&str, SeedCollections<'_>)]) -> Arc<Self> {
      Arc::new(Self {
        dbs: Mutex::new(
          dbs
            .iter()
            .map(|(db, collections)| {
              (
                db.to_string(),
                collections
                  .iter()
                  .map(|(name, docs)| (name.to_string(), docs.clone()))
                  .collect(),
              )
            })
            .collect(),
        ),
      })
    }

    fn collection(&self, db: &str, collection: &str) -> Result<Vec<DocRecord>, Error> {
      self
        .dbs
        .lock()
        .unwrap()
        .get(db)
        .and_then(|collections| collections.get(collection))
        .cloned()
        .ok_or_else(|| Error::Unsupported {
          message: format!("no such collection {db}.{collection}"),
        })
    }

    fn matches(record: &DocRecord, filter: &serde_json::Map<String, serde_json::Value>) -> bool {
      let Ok(doc) = serde_json::from_str::<serde_json::Value>(&record.doc) else {
        return false;
      };
      filter
        .iter()
        .all(|(key, value)| doc.get(key) == Some(value))
    }

    fn filtered(
      &self,
      db: &str,
      collection: &str,
      filter: Option<&str>,
    ) -> Result<Vec<DocRecord>, Error> {
      let docs = self.collection(db, collection)?;
      let Some(filter) = filter.filter(|f| !f.trim().is_empty()) else {
        return Ok(docs);
      };
      let serde_json::Value::Object(filter) =
        serde_json::from_str(filter).map_err(|_| Error::Unsupported {
          message: "find: the filter is not extended json".to_string(),
        })?
      else {
        return Err(Error::Unsupported {
          message: "find: the filter must be an object".to_string(),
        });
      };
      Ok(
        docs
          .into_iter()
          .filter(|record| Self::matches(record, &filter))
          .collect(),
      )
    }
  }

  #[async_trait::async_trait]
  impl Connection for FakeDoc {
    async fn health(&self) -> Result<(), Error> {
      Ok(())
    }
    async fn close(&self) -> Result<(), Error> {
      Ok(())
    }
    fn doc(&self) -> Option<&dyn DocBrowse> {
      Some(self)
    }
  }

  #[async_trait::async_trait]
  impl DocBrowse for FakeDoc {
    async fn databases(&self) -> Result<Vec<DocDatabase>, Error> {
      Ok(
        self
          .dbs
          .lock()
          .unwrap()
          .keys()
          .map(|name| DocDatabase {
            name: name.clone(),
            size_bytes: Some(2048.0),
            empty: false,
          })
          .collect(),
      )
    }

    async fn collections(&self, db: &str) -> Result<Vec<DocCollection>, Error> {
      let dbs = self.dbs.lock().unwrap();
      let collections = dbs.get(db).ok_or_else(|| Error::Unsupported {
        message: format!("no such database {db}"),
      })?;
      Ok(
        collections
          .iter()
          .map(|(name, docs)| DocCollection {
            name: name.clone(),
            kind: DocCollectionKind::Collection,
            estimated_docs: Some(docs.len() as f64),
            capped: false,
          })
          .collect(),
      )
    }

    async fn find_docs(&self, request: &DocFindRequest) -> Result<DocPage, Error> {
      let docs = self.filtered(&request.db, &request.collection, request.filter.as_deref())?;
      let offset: usize = request
        .cursor
        .as_deref()
        .and_then(|cursor| cursor.parse().ok())
        .unwrap_or(0);
      let end = (offset + request.limit as usize).min(docs.len());
      let page: Vec<DocEntry> = docs[offset..end]
        .iter()
        .map(|record| DocEntry {
          id: record.id.clone(),
          doc: record.doc.clone(),
        })
        .collect();
      Ok(DocPage {
        docs: page,
        cursor: (end < docs.len()).then(|| end.to_string()),
      })
    }

    async fn doc_detail(&self, db: &str, collection: &str, id: &str) -> Result<DocDetail, Error> {
      let docs = self.collection(db, collection)?;
      let record = docs
        .iter()
        .find(|record| record.id.as_deref() == Some(id))
        .ok_or_else(|| Error::Unsupported {
          message: format!("no such document {id}"),
        })?;
      Ok(DocDetail {
        id: record.id.clone(),
        relaxed: record.doc.clone(),
        canonical: record.doc.clone(),
      })
    }

    async fn replace_doc(
      &self,
      db: &str,
      collection: &str,
      id: &str,
      doc: &str,
    ) -> Result<(), Error> {
      let mut dbs = self.dbs.lock().unwrap();
      let docs = dbs
        .get_mut(db)
        .and_then(|collections| collections.get_mut(collection))
        .ok_or_else(|| Error::Unsupported {
          message: format!("no such collection {db}.{collection}"),
        })?;
      let record = docs
        .iter_mut()
        .find(|record| record.id.as_deref() == Some(id))
        .ok_or_else(|| Error::Unsupported {
          message: format!("no such document {id}"),
        })?;
      record.doc = doc.to_string();
      Ok(())
    }

    async fn delete_doc(&self, db: &str, collection: &str, id: &str) -> Result<(), Error> {
      let mut dbs = self.dbs.lock().unwrap();
      let docs = dbs
        .get_mut(db)
        .and_then(|collections| collections.get_mut(collection))
        .ok_or_else(|| Error::Unsupported {
          message: format!("no such collection {db}.{collection}"),
        })?;
      let before = docs.len();
      docs.retain(|record| record.id.as_deref() != Some(id));
      if docs.len() == before {
        return Err(Error::Unsupported {
          message: format!("no such document {id}"),
        });
      }
      Ok(())
    }

    async fn indexes(&self, _: &str, collection: &str) -> Result<Vec<IndexInfo>, Error> {
      if collection == "users" {
        Ok(vec![IndexInfo {
          name: "email_1".to_string(),
          definition: "{ email: 1 }".to_string(),
          unique: true,
        }])
      } else {
        Ok(Vec::new())
      }
    }

    async fn count_docs(
      &self,
      db: &str,
      collection: &str,
      filter: Option<&str>,
    ) -> Result<DocCount, Error> {
      let docs = self.filtered(db, collection, filter)?;
      Ok(DocCount {
        count: docs.len() as f64,
        // Real connectors estimate when unfiltered.
        exact: filter.is_some(),
      })
    }

    async fn run_query(
      &self,
      db: &str,
      collection: &str,
      source: &str,
    ) -> Result<DocQueryResult, Error> {
      let value: serde_json::Value =
        serde_json::from_str(source).map_err(|_| Error::Unsupported {
          message: "console: not extended json".to_string(),
        })?;
      let docs = match value {
        serde_json::Value::Object(_) => self.filtered(db, collection, Some(source))?,
        serde_json::Value::Array(_) => self.collection(db, collection)?,
        _ => {
          return Err(Error::Unsupported {
            message: "console: an object or a pipeline".to_string(),
          });
        }
      };
      Ok(DocQueryResult {
        docs: docs.into_iter().map(|record| record.doc).collect(),
        truncated: false,
        duration_ms: 3.0,
      })
    }
  }

  fn record(id: i64, body: &str) -> DocRecord {
    DocRecord {
      id: Some(id.to_string()),
      doc: format!("{{\"_id\":{id},{}", body.trim_start_matches('{')),
    }
  }

  fn users() -> Vec<DocRecord> {
    vec![
      record(1, "{\"name\":\"Ada\",\"plan\":\"pro\"}"),
      record(2, "{\"name\":\"Alan\",\"plan\":\"free\"}"),
      record(3, "{\"name\":\"Grace\",\"plan\":\"pro\"}"),
    ]
  }

  fn fake_profile(database: Option<&str>) -> ConnectionProfile {
    ConnectionProfile {
      id: "doc-fake".to_string(),
      name: "doc fake".to_string(),
      env: soquel_core::profiles::Env::Dev,
      group: None,
      agent_access: Default::default(),
      credential: Default::default(),
      params: soquel_core::profiles::ConnectorParams::Mongo(soquel_core::profiles::MongoParams {
        host: "localhost".to_string(),
        port: 27017,
        database: database.map(str::to_string),
        username: None,
        auth_source: None,
        tls: false,
        tunnel_id: None,
      }),
    }
  }

  fn doc_view<'a>(
    fake: Arc<FakeDoc>,
    database: Option<&str>,
    cx: &'a mut gpui::TestAppContext,
  ) -> (Entity<DocWorkspace>, &'a mut gpui::VisualTestContext) {
    let dir = tempfile::tempdir().unwrap();
    let state = std::sync::Arc::new(soquel_core::AppState::for_tests(
      dir.path(),
      Box::new(soquel_core::secrets::InMemoryStore::default()),
    ));
    let db = crate::core::fake_db(fake, ConnectorKind::Mongo);
    let profile = fake_profile(database);
    shell_window(cx, move |window, cx| {
      DocWorkspace::new(state, db, profile, window, cx)
    })
  }

  #[gpui::test]
  fn collections_load_for_the_profiles_database(cx: &mut gpui::TestAppContext) {
    let fake = FakeDoc::new(&[
      ("app", &[("users", users()), ("events", Vec::new())]),
      ("other", &[("misc", Vec::new())]),
    ]);
    let (view, cx) = doc_view(fake, Some("app"), cx);
    cx.run_until_parked();
    cx.update(|_, cx| {
      let this = view.read(cx);
      assert_eq!(this.doc_db.as_deref(), Some("app"));
      assert_eq!(this.db_names, vec!["app", "other"]);
      let names: Vec<&str> = this.collections.iter().map(|c| c.name.as_str()).collect();
      assert_eq!(names, vec!["events", "users"]);
    });
  }

  #[gpui::test]
  fn the_first_database_is_picked_without_a_profile_default(cx: &mut gpui::TestAppContext) {
    let fake = FakeDoc::new(&[("alpha", &[("a", Vec::new())]), ("beta", &[])]);
    let (view, cx) = doc_view(fake, None, cx);
    cx.run_until_parked();
    cx.update(|_, cx| {
      let this = view.read(cx);
      assert_eq!(this.doc_db.as_deref(), Some("alpha"));
      assert_eq!(this.collections.len(), 1);
    });
  }

  #[gpui::test]
  fn selecting_a_collection_pages_docs_and_counts(cx: &mut gpui::TestAppContext) {
    let many: Vec<DocRecord> = (0..250)
      .map(|ix| record(ix, "{\"plan\":\"pro\"}"))
      .collect();
    let fake = FakeDoc::new(&[("app", &[("users", many)])]);
    let (view, cx) = doc_view(fake, Some("app"), cx);
    cx.run_until_parked();

    cx.update(|_, cx| {
      view.update(cx, |view, cx| {
        view.select_collection("users".to_string(), cx)
      })
    });
    cx.run_until_parked();
    cx.update(|_, cx| {
      let this = view.read(cx);
      assert_eq!(this.docs.len(), 100);
      assert_eq!(this.doc_cursor.as_deref(), Some("100"));
      assert!(!this.doc_loading);
      // The unfiltered count is an estimate.
      assert!(matches!(this.doc_count, Some(DocCount { count, exact: false }) if count == 250.0));
      // The indexes loaded alongside.
      assert!(this.indexes.iter().any(|index| index.name == "email_1"));
    });

    // "load more" extends until the cursor closes.
    cx.update(|_, cx| view.update(cx, |view, cx| view.find(false, cx)));
    cx.run_until_parked();
    cx.update(|_, cx| assert_eq!(view.read(cx).docs.len(), 200));
    cx.update(|_, cx| view.update(cx, |view, cx| view.find(false, cx)));
    cx.run_until_parked();
    cx.update(|_, cx| {
      let this = view.read(cx);
      assert_eq!(this.docs.len(), 250);
      assert!(this.doc_cursor.is_none());
    });
  }

  #[gpui::test]
  fn a_bad_filter_lands_in_the_error_line_and_keeps_the_docs(cx: &mut gpui::TestAppContext) {
    let fake = FakeDoc::new(&[("app", &[("users", users())])]);
    let (view, cx) = doc_view(fake, Some("app"), cx);
    cx.run_until_parked();
    cx.update(|_, cx| {
      view.update(cx, |view, cx| {
        view.select_collection("users".to_string(), cx)
      })
    });
    cx.run_until_parked();

    cx.update(|window, cx| {
      view.update(cx, |view, cx| {
        view
          .doc_filter
          .update(cx, |input, cx| input.set_value("not json", window, cx));
        view.find(true, cx);
      });
    });
    cx.run_until_parked();
    cx.update(|_, cx| {
      let this = view.read(cx);
      assert!(this.filter_error.is_some());
      assert_eq!(this.docs.len(), 3, "the stale docs stay visible");
    });

    // A valid filter narrows and counts exactly.
    cx.update(|window, cx| {
      view.update(cx, |view, cx| {
        view.doc_filter.update(cx, |input, cx| {
          input.set_value("{\"plan\":\"pro\"}", window, cx)
        });
        view.find(true, cx);
      });
    });
    cx.run_until_parked();
    cx.update(|_, cx| {
      let this = view.read(cx);
      assert!(this.filter_error.is_none());
      assert_eq!(this.docs.len(), 2);
      assert!(matches!(this.doc_count, Some(DocCount { count, exact: true }) if count == 2.0));
    });
  }

  #[gpui::test(iterations = 10)]
  fn a_superseded_find_keeps_only_the_newest_result(cx: &mut gpui::TestAppContext) {
    let fake = FakeDoc::new(&[("app", &[("users", users())])]);
    let (view, cx) = doc_view(fake, Some("app"), cx);
    cx.run_until_parked();
    cx.update(|_, cx| {
      view.update(cx, |view, cx| {
        view.select_collection("users".to_string(), cx)
      })
    });
    cx.run_until_parked();

    cx.update(|window, cx| {
      view.update(cx, |view, cx| {
        view.doc_filter.update(cx, |input, cx| {
          input.set_value("{\"plan\":\"pro\"}", window, cx)
        });
        view.find(true, cx);
        view.doc_filter.update(cx, |input, cx| {
          input.set_value("{\"plan\":\"free\"}", window, cx)
        });
        view.find(true, cx);
      });
    });
    cx.run_until_parked();
    cx.update(|_, cx| {
      let this = view.read(cx);
      assert_eq!(this.docs.len(), 1);
      assert!(this.docs[0].doc.contains("Alan"));
    });
  }

  #[gpui::test]
  fn selecting_a_doc_loads_the_detail_and_no_id_stays_read_only(cx: &mut gpui::TestAppContext) {
    let mut docs = users();
    docs.push(DocRecord {
      id: None,
      doc: "{\"name\":\"ghost\"}".to_string(),
    });
    let fake = FakeDoc::new(&[("app", &[("users", docs)])]);
    let (view, cx) = doc_view(fake, Some("app"), cx);
    cx.run_until_parked();
    cx.update(|_, cx| {
      view.update(cx, |view, cx| {
        view.select_collection("users".to_string(), cx)
      })
    });
    cx.run_until_parked();

    cx.update(|_, cx| view.update(cx, |view, cx| view.select_doc(0, cx)));
    cx.run_until_parked();
    cx.update(|_, cx| {
      let this = view.read(cx);
      assert_eq!(this.selected, Some(0));
      let detail = this.detail.as_ref().expect("addressable detail");
      assert!(detail.relaxed.contains("Ada"));
    });

    // No `_id`: selected, but there is no address to fetch a detail for.
    cx.update(|_, cx| view.update(cx, |view, cx| view.select_doc(3, cx)));
    cx.run_until_parked();
    cx.update(|_, cx| {
      let this = view.read(cx);
      assert_eq!(this.selected, Some(3));
      assert!(this.detail.is_none());
    });
  }

  #[gpui::test(iterations = 10)]
  fn editing_saves_the_draft_and_reloads_the_detail(cx: &mut gpui::TestAppContext) {
    let fake = FakeDoc::new(&[("app", &[("users", users())])]);
    let (view, cx) = doc_view(fake.clone(), Some("app"), cx);
    cx.run_until_parked();
    cx.update(|_, cx| {
      view.update(cx, |view, cx| {
        view.select_collection("users".to_string(), cx)
      })
    });
    cx.run_until_parked();
    cx.update(|_, cx| view.update(cx, |view, cx| view.select_doc(0, cx)));
    cx.run_until_parked();

    cx.update(|window, cx| {
      view.update(cx, |view, cx| view.start_edit(window, cx));
    });
    cx.update(|_, cx| {
      let this = view.read(cx);
      assert!(this.editing);
      // The editor starts from the pretty-printed canonical form.
      assert!(
        this
          .doc_editor
          .read(cx)
          .value()
          .contains("\"name\": \"Ada\"")
      );
    });

    cx.update(|window, cx| {
      view.update(cx, |view, cx| {
        view.doc_editor.update(cx, |input, cx| {
          input.set_value(
            "{\"_id\":1,\"name\":\"Ada II\",\"plan\":\"max\"}",
            window,
            cx,
          )
        });
        view.save_doc(cx);
      });
    });
    cx.run_until_parked();
    assert!(
      fake.collection("app", "users").unwrap()[0]
        .doc
        .contains("Ada II")
    );
    cx.update(|_, cx| {
      let this = view.read(cx);
      assert!(!this.editing);
      assert!(
        this
          .detail
          .as_ref()
          .is_some_and(|detail| detail.relaxed.contains("Ada II"))
      );
    });
  }

  #[gpui::test(iterations = 10)]
  fn deleting_a_doc_clears_the_selection_and_refreshes(cx: &mut gpui::TestAppContext) {
    let fake = FakeDoc::new(&[("app", &[("users", users())])]);
    let (view, cx) = doc_view(fake.clone(), Some("app"), cx);
    cx.run_until_parked();
    cx.update(|_, cx| {
      view.update(cx, |view, cx| {
        view.select_collection("users".to_string(), cx)
      })
    });
    cx.run_until_parked();
    cx.update(|_, cx| view.update(cx, |view, cx| view.select_doc(0, cx)));
    cx.run_until_parked();

    cx.update(|_, cx| view.update(cx, |view, cx| view.delete_doc(cx)));
    cx.run_until_parked();
    assert_eq!(fake.collection("app", "users").unwrap().len(), 2);
    cx.update(|_, cx| {
      let this = view.read(cx);
      assert!(this.selected.is_none());
      assert!(this.detail.is_none());
      assert_eq!(this.docs.len(), 2);
      // The sidebar estimate refreshed with the collection reload.
      assert!(matches!(
        this.collections.first().map(|c| c.estimated_docs),
        Some(Some(count)) if count == 2.0
      ));
    });
  }

  #[gpui::test]
  fn the_console_answers_finds_and_rejects_garbage(cx: &mut gpui::TestAppContext) {
    let fake = FakeDoc::new(&[("app", &[("users", users())])]);
    let (view, cx) = doc_view(fake, Some("app"), cx);
    cx.run_until_parked();

    // No collection selected: the console refuses to guess.
    cx.update(|window, cx| {
      view.update(cx, |view, cx| {
        view
          .console_input
          .update(cx, |input, cx| input.set_value("{}", window, cx));
        view.run_console(window, cx);
      });
    });
    cx.run_until_parked();
    cx.update(|_, cx| assert!(view.read(cx).console_log.is_empty()));

    cx.update(|_, cx| {
      view.update(cx, |view, cx| {
        view.select_collection("users".to_string(), cx)
      })
    });
    cx.run_until_parked();

    let run = |cx: &mut gpui::VisualTestContext, source: &str| {
      let source = source.to_string();
      cx.update(|window, cx| {
        view.update(cx, |view, cx| {
          view
            .console_input
            .update(cx, |input, cx| input.set_value(source, window, cx));
          view.run_console(window, cx);
        });
      });
      cx.run_until_parked();
    };

    run(cx, "{\"plan\":\"pro\"}");
    cx.update(|_, cx| {
      let this = view.read(cx);
      let entry = this.console_log.last().expect("a log entry");
      assert!(entry.ok);
      assert_eq!(entry.prompt, "app.users");
      assert_eq!(entry.lines.len(), 2);
      assert_eq!(entry.summary.as_deref(), Some("2 docs · 3 ms"));
      assert_eq!(this.console_input.read(cx).value().to_string(), "");
    });

    run(cx, "garbage");
    cx.update(|_, cx| {
      let entry = view.read(cx).console_log.last().expect("a log entry");
      assert!(!entry.ok);
      assert!(entry.lines[0].contains("not extended json"));
    });
  }

  #[test]
  fn id_label_unwraps_extjson_wrappers() {
    assert_eq!(doc_id_label(None), "no _id");
    assert_eq!(doc_id_label(Some("{\"$oid\":\"abc123\"}")), "abc123");
    assert_eq!(doc_id_label(Some("{\"$numberLong\":\"42\"}")), "42");
    assert_eq!(doc_id_label(Some("\"user-1\"")), "user-1");
    assert_eq!(doc_id_label(Some("7")), "7");
    assert_eq!(doc_id_label(Some("not json")), "not json");
  }

  #[test]
  fn preview_drops_id_and_truncates() {
    assert_eq!(
      doc_preview("{\"_id\":1,\"name\":\"Ada\",\"plan\":\"pro\"}", 140),
      "name: Ada  plan: pro"
    );
    assert_eq!(doc_preview("{\"_id\":1}", 140), "(empty document)");
    let long = doc_preview("{\"note\":\"aaaaaaaaaa\"}", 8);
    assert_eq!(long.chars().count(), 8);
    assert!(long.ends_with('…'));
  }

  #[test]
  fn counts_and_bytes_format_compactly() {
    assert_eq!(format_doc_count(1.0, true), "1 doc");
    assert_eq!(format_doc_count(100.0, true), "100 docs");
    assert_eq!(format_doc_count(52400.0, false), "~52.4k docs");
    assert_eq!(compact_count(1200.0), "1.2k");
    assert_eq!(compact_count(45_000_000.0), "45m");
    assert_eq!(compact_count(999.0), "999");
    assert_eq!(format_bytes(512.0), "512 B");
    assert_eq!(format_bytes(2048.0), "2.0 KB");
    assert_eq!(format_bytes(13_631_488.0), "13 MB");
  }

  /// The view against a live mongo: select a collection loads docs, a click
  /// loads the detail, the console runs. Skipped without SOQUEL_TEST_MONGO.
  #[gpui::test]
  fn integration_doc_workspace_lists_selects_and_runs(cx: &mut gpui::TestAppContext) {
    // Real mongo IO wakes from tokio's driver thread.
    cx.executor().allow_parking();
    let Some(coord) = soquel_core::integration_env("SOQUEL_TEST_MONGO") else {
      return;
    };
    let (host, port) = coord.split_once(':').expect("host:port");
    let profile = ConnectionProfile {
      id: "doc-view".to_string(),
      name: "mongo view".to_string(),
      env: soquel_core::profiles::Env::Dev,
      group: None,
      agent_access: soquel_core::profiles::AgentAccess::None,
      credential: soquel_core::profiles::CredentialSource::Prompt,
      params: soquel_core::profiles::ConnectorParams::Mongo(soquel_core::profiles::MongoParams {
        host: host.to_string(),
        port: port.parse().expect("port"),
        database: Some("soquel_e2e".to_string()),
        username: Some("soquel".to_string()),
        auth_source: Some("admin".to_string()),
        tls: false,
        tunnel_id: None,
      }),
    };
    let db = crate::core::connect_with_blocking(
      profile.clone(),
      soquel_core::credentials::Credentials::fixed(Some("soquel".to_string())),
    )
    .expect("connects to the compose mongo");

    let dir = tempfile::tempdir().unwrap();
    let state = std::sync::Arc::new(soquel_core::AppState::for_tests(
      dir.path(),
      Box::new(soquel_core::secrets::InMemoryStore::default()),
    ));
    let (view, cx) = crate::test_support::shell_window(cx, {
      let state = state.clone();
      move |window, cx| DocWorkspace::new(state, db, profile, window, cx)
    });

    crate::test_support::wait_until(cx, "the collections to load", |cx| {
      cx.update(|_, cx| view.read(cx).collections.iter().any(|c| c.name == "users"))
    });
    cx.update(|_, cx| {
      view.update(cx, |view, cx| {
        view.select_collection("users".to_string(), cx)
      })
    });
    crate::test_support::wait_until(cx, "the users to page in", |cx| {
      cx.update(|_, cx| !view.read(cx).docs.is_empty())
    });

    cx.update(|_, cx| view.update(cx, |view, cx| view.select_doc(0, cx)));
    crate::test_support::wait_until(cx, "the document detail", |cx| {
      cx.update(|_, cx| view.read(cx).detail.is_some())
    });
    cx.update(|_, cx| {
      assert!(
        view
          .read(cx)
          .detail
          .as_ref()
          .unwrap()
          .relaxed
          .contains("email")
      );
    });

    // The indexes load, and the console runs an aggregate.
    crate::test_support::wait_until(cx, "the indexes", |cx| {
      cx.update(|_, cx| view.read(cx).indexes.iter().any(|i| i.name == "email_1"))
    });
    cx.update(|window, cx| {
      view.update(cx, |view, cx| {
        view.console_input.update(cx, |input, cx| {
          input.set_value(
            "[{\"$group\":{\"_id\":\"$plan\",\"n\":{\"$sum\":1}}}]",
            window,
            cx,
          )
        });
        view.run_console(window, cx);
      });
    });
    crate::test_support::wait_until(cx, "the console reply", |cx| {
      cx.update(|_, cx| {
        view
          .read(cx)
          .console_log
          .last()
          .is_some_and(|entry| entry.ok && entry.lines.len() == 2)
      })
    });
  }
}
