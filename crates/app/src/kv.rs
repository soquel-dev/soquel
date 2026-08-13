//! The redis / key-value browser: a whole workspace of its own, mounted when
//! a connection browses keys instead of the SQL shell.

use std::sync::Arc;

use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::resizable::{ResizableState, h_resizable, resizable_panel};
use gpui_component::select::{Select, SelectEvent, SelectState};
use gpui_component::{
  ActiveTheme, Icon, IndexPath, Selectable, Sizable, StyledExt, h_flex, v_flex,
};
use soquel_core::AppState;
use soquel_core::connectors::{KeyDetail, KeyEntry, KeyKind, KeyValue, KvDatabases};
use soquel_core::profiles::ConnectionProfile;

use crate::actions::{FocusEditor, RefreshSchema};
use crate::core::{self, Db};
use gpui_component::menu::{ContextMenuExt, PopupMenuItem};

/// Short badge label + colour per redis type.
pub fn key_kind_badge(kind: KeyKind, cx: &App) -> (&'static str, Hsla) {
  match kind {
    KeyKind::String => ("str", cx.theme().blue),
    KeyKind::List => ("list", cx.theme().magenta),
    KeyKind::Set => ("set", cx.theme().yellow),
    KeyKind::Zset => ("zset", cx.theme().red),
    KeyKind::Hash => ("hash", cx.theme().green),
    KeyKind::Stream => ("strm", cx.theme().cyan),
    KeyKind::Other => ("?", cx.theme().muted_foreground),
  }
}

/// A contains-search wrapped into a redis SCAN MATCH glob: escape the glob
/// specials, wrap in `*...*`. Empty stays empty and matches everything.
pub fn contains_pattern(text: &str) -> String {
  if text.is_empty() {
    return String::new();
  }
  let mut escaped = String::with_capacity(text.len() + 2);
  escaped.push('*');
  for ch in text.chars() {
    if matches!(ch, '\\' | '*' | '?' | '[' | ']') {
      escaped.push('\\');
    }
    escaped.push(ch);
  }
  escaped.push('*');
  escaped
}

/// Compact ttl countdown: seconds, minutes, hours, then days.
pub fn format_ttl(ms: f64) -> String {
  let secs = ms / 1000.0;
  if secs < 60.0 {
    format!("{}s", (secs.round() as i64).max(1))
  } else if secs < 3600.0 {
    format!("{}m", (secs / 60.0).round() as i64)
  } else if secs < 86400.0 {
    format!("{}h", (secs / 3600.0).round() as i64)
  } else {
    format!("{}d", (secs / 86400.0).round() as i64)
  }
}

const SCAN_COUNT: u32 = 200;

fn value_kind(value: &KeyValue) -> KeyKind {
  match value {
    KeyValue::String { .. } => KeyKind::String,
    KeyValue::List { .. } => KeyKind::List,
    KeyValue::Set { .. } => KeyKind::Set,
    KeyValue::Zset { .. } => KeyKind::Zset,
    KeyValue::Hash { .. } => KeyKind::Hash,
    KeyValue::Stream { .. } => KeyKind::Stream,
    KeyValue::Other { .. } => KeyKind::Other,
  }
}

/// Sample length for a collection (None for string/other): the value carries a
/// bounded sample while `size` is the full length.
fn sample_len(value: &KeyValue) -> Option<usize> {
  match value {
    KeyValue::List { entries } | KeyValue::Set { entries } => Some(entries.len()),
    KeyValue::Zset { entries } => Some(entries.len()),
    KeyValue::Hash { entries } => Some(entries.len()),
    KeyValue::Stream { entries } => Some(entries.len()),
    KeyValue::String { .. } | KeyValue::Other { .. } => None,
  }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum KvView {
  Key,
  Console,
}

struct ConsoleEntry {
  command: String,
  lines: Vec<String>,
  ok: bool,
}

pub struct KvWorkspace {
  state: Arc<AppState>,
  focus_handle: FocusHandle,
  db: Db,
  connection_id: String,
  name: SharedString,
  server_version: Option<String>,
  sidebar_split: Entity<ResizableState>,
  search: Entity<InputState>,
  glob: bool,
  keys: Vec<KeyEntry>,
  cursor: Option<String>,
  scanning: bool,
  scan_seq: u64,
  databases: Option<KvDatabases>,
  db_select: Entity<SelectState<Vec<String>>>,
  db_indices: Vec<u32>,
  selected_key: Option<String>,
  view: KvView,
  detail: Option<KeyDetail>,
  detail_loading: bool,
  string_draft: Entity<InputState>,
  ttl_input: Entity<InputState>,
  delete_armed: bool,
  console_input: Entity<InputState>,
  console_log: Vec<ConsoleEntry>,
  status: SharedString,
  _subscriptions: Vec<Subscription>,
  // One slot per concern: a shared slot would cancel a sibling refresh
  // before its first poll (a write triggers scan + detail together).
  _scan_task: Task<()>,
  _detail_task: Task<()>,
  _op_task: Task<()>,
}

impl Focusable for KvWorkspace {
  fn focus_handle(&self, _: &App) -> FocusHandle {
    self.focus_handle.clone()
  }
}

impl KvWorkspace {
  pub fn new(
    state: Arc<AppState>,
    db: Db,
    profile: ConnectionProfile,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) -> Self {
    let server_version = db.server_version();
    let focus_handle = cx.focus_handle();
    window.focus(&focus_handle, cx);
    let search = cx.new(|cx| InputState::new(window, cx).placeholder("search keys"));
    let string_draft = cx.new(|cx| {
      InputState::new(window, cx)
        .multi_line(true)
        .auto_grow(3, 16)
    });
    let ttl_input = cx.new(|cx| InputState::new(window, cx).placeholder("seconds"));
    let console_input = cx.new(|cx| InputState::new(window, cx).placeholder("redis command"));
    let db_select = cx.new(|cx| SelectState::new(vec!["db 0".to_string()], None, window, cx));
    let sidebar_split = cx.new(|_| ResizableState::default());

    let subscriptions = vec![
      cx.subscribe(&search, |this, _, event: &InputEvent, cx| {
        if matches!(event, InputEvent::PressEnter { .. }) {
          this.scan(true, cx);
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
      state,
      focus_handle,
      db,
      connection_id: profile.id.clone(),
      name: profile.name.clone().into(),
      server_version,
      sidebar_split,
      search,
      glob: false,
      keys: Vec::new(),
      cursor: None,
      scanning: false,
      scan_seq: 0,
      databases: None,
      db_select,
      db_indices: vec![0],
      selected_key: None,
      view: KvView::Key,
      detail: None,
      detail_loading: false,
      string_draft,
      ttl_input,
      delete_armed: false,
      console_input,
      console_log: Vec::new(),
      status: SharedString::default(),
      _subscriptions: subscriptions,
      _scan_task: Task::ready(()),
      _detail_task: Task::ready(()),
      _op_task: Task::ready(()),
    };
    this.scan(true, cx);
    this.load_databases(cx);
    this
  }

  fn pattern(&self, cx: &App) -> String {
    let typed = self.search.read(cx).value().to_string();
    if self.glob {
      typed
    } else {
      contains_pattern(&typed)
    }
  }

  /// A cursor SCAN drains empty MATCH pages: with a filter most pages return
  /// nothing, so hop until keys arrive, the cursor closes, or a hop cap.
  fn scan(&mut self, reset: bool, cx: &mut Context<Self>) {
    if reset {
      self.cursor = None;
    }
    self.scanning = true;
    self.scan_seq += 1;
    let seq = self.scan_seq;
    cx.notify();
    let db = self.db.clone();
    let pattern = self.pattern(cx);
    let start = self.cursor.clone();
    self._scan_task = cx.spawn(async move |this, cx| {
      let mut cursor = start;
      let mut fresh: Vec<KeyEntry> = Vec::new();
      let mut done = false;
      for _ in 0..50 {
        match core::kv_scan(&db, pattern.clone(), cursor.clone(), SCAN_COUNT, cx).await {
          Ok(page) => {
            fresh.extend(page.keys);
            cursor = page.cursor.clone();
            if page.cursor.is_none() {
              done = true;
              break;
            }
            if !fresh.is_empty() {
              break;
            }
          }
          Err(error) => {
            let _ = this.update(cx, |this, cx| {
              this.scanning = false;
              this.status = crate::status::error(&error);
              cx.notify();
            });
            return;
          }
        }
      }
      let _ = this.update(cx, |this, cx| {
        // A newer scan superseded this one: drop the stale page.
        if this.scan_seq != seq {
          return;
        }
        this.scanning = false;
        this.cursor = if done { None } else { cursor };
        if reset {
          this.keys = fresh;
        } else {
          this.keys.extend(fresh);
        }
        cx.notify();
      });
    });
  }

  fn load_databases(&mut self, cx: &mut Context<Self>) {
    let task = core::kv_databases(&self.db, cx);
    let db_select = self.db_select.clone();
    cx.spawn(async move |this, cx| {
      let Some(databases) = crate::status::ok_or_log(task.await) else {
        return;
      };
      let _ = this.update_in(cx, move |this, window, cx| {
        let indices: Vec<u32> = (0..databases.total).collect();
        let labels: Vec<String> = indices
          .iter()
          .map(|db| {
            let count = databases
              .used
              .iter()
              .find(|entry| entry.db == *db)
              .map(|entry| format!("  {} keys", entry.keys as u64))
              .unwrap_or_default();
            format!("db {db}{count}")
          })
          .collect();
        let current = databases.current as usize;
        this.db_indices = indices;
        this.databases = Some(databases);
        cx.notify();
        db_select.update(cx, |select, cx| {
          select.set_items(labels, window, cx);
          select.set_selected_index(Some(IndexPath::new(current)), window, cx);
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
    let Some(&db) = self.db_indices.get(ix) else {
      return;
    };
    if self.databases.as_ref().map(|d| d.current) == Some(db) {
      return;
    }
    let task = core::kv_select_db(self.state.clone(), self.connection_id.clone(), db, cx);
    self._op_task = cx.spawn(async move |this, cx| match task.await {
      Ok(fresh) => {
        let _ = this.update(cx, |this, cx| {
          this.db = fresh;
          this.selected_key = None;
          this.detail = None;
          this.scan(true, cx);
          this.load_databases(cx);
        });
      }
      Err(error) => {
        let _ = this.update(cx, |this, cx| {
          this.status = crate::status::error(&error);
          cx.notify();
        });
      }
    });
  }

  fn select_key(&mut self, key: String, window: &mut Window, cx: &mut Context<Self>) {
    self.selected_key = Some(key.clone());
    self.view = KvView::Key;
    self.detail = None;
    self.detail_loading = true;
    self.delete_armed = false;
    cx.notify();
    let task = core::kv_key_detail(&self.db, key, cx);
    let string_draft = self.string_draft.clone();
    let ttl_input = self.ttl_input.clone();
    let _ = window;
    self._detail_task = cx.spawn(async move |this, cx| {
      let result = task.await;
      let _ = this.update_in(cx, move |this, window, cx| {
        this.detail_loading = false;
        cx.notify();
        match result {
          Ok(detail) => {
            let value = match &detail.value {
              KeyValue::String { value } => value.clone(),
              _ => String::new(),
            };
            let ttl_secs = detail
              .ttl_ms
              .map(|ms| ((ms / 1000.0).ceil() as i64).to_string())
              .unwrap_or_default();
            string_draft.update(cx, |input, cx| input.set_value(value, window, cx));
            ttl_input.update(cx, |input, cx| input.set_value(ttl_secs, window, cx));
            this.detail = Some(detail);
            cx.notify();
          }
          Err(error) => {
            this.status = crate::status::error(&error);
            cx.notify();
          }
        }
      });
    });
  }

  fn run_console(&mut self, _: &mut Window, cx: &mut Context<Self>) {
    let command = self.console_input.read(cx).value().trim().to_string();
    if command.is_empty() {
      return;
    }
    let task = core::kv_run_command(&self.db, command.clone(), cx);
    let input = self.console_input.clone();
    self._op_task = cx.spawn(async move |this, cx| {
      let result = task.await;
      let _ = this.update_in(cx, move |this, window, cx| {
        let entry = match result {
          Ok(lines) => ConsoleEntry {
            command: command.clone(),
            lines,
            ok: true,
          },
          Err(error) => ConsoleEntry {
            command: command.clone(),
            lines: vec![format!("{error}")],
            ok: false,
          },
        };
        input.update(cx, |input, cx| input.set_value("", window, cx));
        this.console_log.push(entry);
        // A write via the console shifts the keyspace.
        this.scan(true, cx);
        this.load_databases(cx);
        cx.notify();
      });
    });
  }

  fn save_string(&mut self, cx: &mut Context<Self>) {
    let (Some(key), value) = (
      self.selected_key.clone(),
      self.string_draft.read(cx).value().to_string(),
    ) else {
      return;
    };
    let task = core::kv_set_string(&self.db, key.clone(), value, cx);
    self._op_task = self.after_write(task, key, cx);
  }

  fn apply_ttl(&mut self, ttl_ms: Option<f64>, cx: &mut Context<Self>) {
    let Some(key) = self.selected_key.clone() else {
      return;
    };
    let task = core::kv_set_ttl(&self.db, key.clone(), ttl_ms, cx);
    self._op_task = self.after_write(task, key, cx);
  }

  fn submit_ttl(&mut self, cx: &mut Context<Self>) {
    match self.ttl_input.read(cx).value().trim().parse::<f64>() {
      Ok(seconds) if seconds > 0.0 => self.apply_ttl(Some(seconds * 1000.0), cx),
      _ => {
        self.status = "TTL must be a positive number of seconds".into();
        cx.notify();
      }
    }
  }

  fn delete_key(&mut self, cx: &mut Context<Self>) {
    let Some(key) = self.selected_key.clone() else {
      return;
    };
    let task = core::kv_delete_key(&self.db, key, cx);
    self._op_task = cx.spawn(async move |this, cx| {
      let result = task.await;
      let _ = this.update(cx, |this, cx| {
        match result {
          Ok(()) => {
            this.selected_key = None;
            this.detail = None;
            this.delete_armed = false;
            this.scan(true, cx);
            this.load_databases(cx);
          }
          Err(error) => this.status = crate::status::error(&error),
        }
        cx.notify();
      });
    });
  }

  /// A key write reloads its detail and re-scans (ttl/size may have moved).
  fn after_write(
    &self,
    task: Task<Result<(), soquel_core::error::Error>>,
    key: String,
    cx: &mut Context<Self>,
  ) -> Task<()> {
    cx.spawn(async move |this, cx| {
      let result = task.await;
      let _ = this.update_in(cx, move |this, window, cx| match result {
        Ok(()) => {
          this.scan(true, cx);
          this.load_databases(cx);
          this.select_key(key, window, cx);
        }
        Err(error) => {
          this.status = crate::status::error(&error);
          cx.notify();
        }
      });
    })
  }

  fn type_badge(&self, kind: KeyKind, full: bool, cx: &App) -> Div {
    let (short, color) = key_kind_badge(kind, cx);
    let label = if full {
      match kind {
        KeyKind::String => "string",
        KeyKind::List => "list",
        KeyKind::Set => "set",
        KeyKind::Zset => "zset",
        KeyKind::Hash => "hash",
        KeyKind::Stream => "stream",
        KeyKind::Other => "other",
      }
    } else {
      short
    };
    crate::ui::tinted_badge(label, color, cx)
  }

  pub(crate) fn footer_connection(&self) -> String {
    match &self.server_version {
      Some(version) => {
        let (engine, version) =
          crate::connections::server_badge(soquel_core::profiles::ConnectorKind::Redis, version);
        format!("{} - {engine} {version}", self.name)
      }
      None => self.name.to_string(),
    }
  }

  fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
    let count = self.keys.len();
    v_flex()
      .size_full()
      .child(
        h_flex()
          .px_2()
          .h(px(34.))
          .flex_none()
          .justify_between()
          .items_center()
          .border_b_1()
          .border_color(cx.theme().border)
          .child(
            div()
              .text_xs()
              .font_semibold()
              .text_color(cx.theme().muted_foreground)
              .child("keys"),
          )
          .child(
            h_flex()
              .gap_1()
              .items_center()
              .child(
                div()
                  .w(px(150.))
                  .child(Select::new(&self.db_select).small()),
              )
              .child(
                Button::new("kv-refresh")
                  .ghost()
                  .xsmall()
                  .icon(Icon::new(crate::icons::SoquelIcon::RefreshCw))
                  .tooltip("Rescan keys")
                  .on_click(cx.listener(|this, _, _, cx| {
                    this.scan(true, cx);
                    this.load_databases(cx);
                  })),
              ),
          ),
      )
      .child(
        h_flex()
          .px_2()
          .py_1p5()
          .gap_1()
          .child(div().flex_1().child(Input::new(&self.search).small()))
          .child(
            Button::new("key-glob")
              .ghost()
              .xsmall()
              .label("glob")
              .map(|button| {
                if self.glob {
                  button.selected(true)
                } else {
                  button
                }
              })
              .on_click(cx.listener(|this, _, window, cx| {
                this.glob = !this.glob;
                let placeholder = if this.glob {
                  "match pattern (*)"
                } else {
                  "search keys"
                };
                this.search.update(cx, |input, cx| {
                  input.set_placeholder(placeholder, window, cx);
                });
                this.scan(true, cx);
              })),
          ),
      )
      .child(
        uniform_list(
          "kv-keys",
          self.keys.len(),
          cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
            let view = cx.entity();
            range
              .map(|ix| {
                let entry = &this.keys[ix];
                let key = entry.key.clone();
                let menu_key = entry.key.clone();
                let view = view.clone();
                let selected = this.selected_key.as_deref() == Some(entry.key.as_str());
                crate::ui::list_row(ix, selected, cx)
                  .text_xs()
                  .font_family(crate::theme::mono(cx))
                  .on_click(cx.listener(move |this, _, window, cx| {
                    this.select_key(key.clone(), window, cx);
                  }))
                  .child(this.type_badge(entry.kind, false, cx))
                  .child(div().flex_1().min_w_0().truncate().child(entry.key.clone()))
                  .when_some(entry.ttl_ms, |row, ttl| {
                    row.child(
                      div()
                        .text_color(cx.theme().muted_foreground)
                        .child(format_ttl(ttl)),
                    )
                  })
                  .context_menu(move |menu, window, _| {
                    let copy_key = menu_key.clone();
                    // Arm, never delete outright: the "sure?" step stays in the
                    // detail header like the button path.
                    let arm = window.listener_for(&view, {
                      let key = menu_key.clone();
                      move |this: &mut KvWorkspace, _: &ClickEvent, window, cx| {
                        this.select_key(key.clone(), window, cx);
                        this.delete_armed = true;
                        cx.notify();
                      }
                    });
                    menu
                      .item(PopupMenuItem::new("Copy key").on_click(move |_, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(copy_key.clone()));
                      }))
                      .separator()
                      .item(PopupMenuItem::new("Delete key…").on_click(arm))
                  })
              })
              .collect::<Vec<_>>()
          }),
        )
        .flex_1()
        .min_h_0(),
      )
      .child(if self.cursor.is_some() {
        h_flex().p_1().child(
          Button::new("scan-more")
            .ghost()
            .xsmall()
            .w_full()
            .label(if self.scanning {
              "scanning…"
            } else {
              "scan more"
            })
            .on_click(cx.listener(|this, _, _, cx| this.scan(false, cx))),
        )
      } else {
        h_flex().px_2().py_1().child(
          div()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(format!("{count} key{}", if count == 1 { "" } else { "s" })),
        )
      })
  }

  fn render_value(&self, detail: &KeyDetail, cx: &Context<Self>) -> AnyElement {
    let row = |left: String, right: String| {
      h_flex()
        .gap_3()
        .py_0p5()
        .border_b_1()
        .border_color(cx.theme().border.opacity(0.4))
        .text_xs()
        .font_family(crate::theme::mono(cx))
        .child(
          div()
            .min_w(px(56.))
            .text_color(cx.theme().muted_foreground)
            .child(left),
        )
        .child(div().flex_1().min_w_0().child(right))
    };
    match &detail.value {
      KeyValue::String { .. } => v_flex()
        .gap_2()
        .child(
          div()
            .text_color(cx.theme().muted_foreground)
            .child("The string is editable below.".to_string()),
        )
        .child(Input::new(&self.string_draft))
        .child(
          Button::new("save-string")
            .primary()
            .xsmall()
            .label("Save value")
            .on_click(cx.listener(|this, _, _, cx| this.save_string(cx))),
        )
        .into_any_element(),
      KeyValue::List { entries } | KeyValue::Set { entries } => v_flex()
        .children(
          entries
            .iter()
            .enumerate()
            .map(|(ix, v)| row(ix.to_string(), v.clone())),
        )
        .into_any_element(),
      KeyValue::Zset { entries } => v_flex()
        .children(
          entries
            .iter()
            .map(|m| row(format!("{}", m.score), m.member.clone())),
        )
        .into_any_element(),
      KeyValue::Hash { entries } => v_flex()
        .children(
          entries
            .iter()
            .map(|f| row(f.field.clone(), f.value.clone())),
        )
        .into_any_element(),
      KeyValue::Stream { entries } => v_flex()
        .gap_2()
        .children(entries.iter().map(|entry| {
          v_flex()
            .pb_1p5()
            .border_b_1()
            .border_color(cx.theme().border.opacity(0.4))
            .text_xs()
            .font_family(crate::theme::mono(cx))
            .child(
              div()
                .text_color(cx.theme().muted_foreground)
                .child(entry.id.clone()),
            )
            .children(entry.fields.iter().map(|f| {
              h_flex()
                .gap_1()
                .child(
                  div()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!("{}:", f.field)),
                )
                .child(f.value.clone())
            }))
        }))
        .into_any_element(),
      KeyValue::Other { type_name } => div()
        .text_xs()
        .font_family(crate::theme::mono(cx))
        .text_color(cx.theme().muted_foreground)
        .child(format!("unsupported type {type_name}"))
        .into_any_element(),
    }
  }

  fn render_detail(&self, cx: &mut Context<Self>) -> AnyElement {
    let Some(detail) = &self.detail else {
      let text = if self.detail_loading {
        "loading key…".to_string()
      } else {
        "soquel=# select a key".to_string()
      };
      return v_flex()
        .size_full()
        .items_center()
        .justify_center()
        .text_sm()
        .font_family(crate::theme::mono(cx))
        .text_color(cx.theme().muted_foreground)
        .child(text)
        .into_any_element();
    };

    let kind = value_kind(&detail.value);
    let size = detail.size as u64;
    let unit = if matches!(detail.value, KeyValue::String { .. }) {
      "bytes"
    } else {
      "entries"
    };
    let sample_note = sample_len(&detail.value).filter(|&len| (len as f64) < detail.size);
    let ttl_label = detail
      .ttl_ms
      .map(format_ttl)
      .unwrap_or_else(|| "none".to_string());

    v_flex()
      .size_full()
      .p_3()
      .gap_2()
      .child(
        h_flex()
          .gap_2()
          .items_center()
          .child(self.type_badge(kind, true, cx))
          .child(
            div()
              .flex_1()
              .min_w_0()
              .truncate()
              .font_family(crate::theme::mono(cx))
              .text_sm()
              .child(detail.key.clone()),
          )
          .child(
            div()
              .text_color(cx.theme().muted_foreground)
              .text_xs()
              .font_family(crate::theme::mono(cx))
              .child(format!("{size} {unit}")),
          )
          .child(if self.delete_armed {
            Button::new("delete-key")
              .danger()
              .xsmall()
              .label("sure?")
              .on_click(cx.listener(|this, _, _, cx| this.delete_key(cx)))
          } else {
            Button::new("delete-key")
              .ghost()
              .xsmall()
              .label("delete")
              .on_click(cx.listener(|this, _, _, cx| {
                this.delete_armed = true;
                cx.notify();
              }))
          }),
      )
      .child(
        h_flex()
          .gap_2()
          .items_center()
          .text_xs()
          .font_family(crate::theme::mono(cx))
          .text_color(cx.theme().muted_foreground)
          .child(format!("ttl {ttl_label}"))
          .child(div().w(px(96.)).child(Input::new(&self.ttl_input).small()))
          .child(
            Button::new("ttl-apply")
              .ghost()
              .xsmall()
              .label("set ttl")
              .on_click(cx.listener(|this, _, _, cx| this.submit_ttl(cx))),
          )
          .when(detail.ttl_ms.is_some(), |row| {
            row.child(
              Button::new("ttl-persist")
                .ghost()
                .xsmall()
                .label("persist")
                .on_click(cx.listener(|this, _, _, cx| this.apply_ttl(None, cx))),
            )
          }),
      )
      .child(
        v_flex()
          .id("kv-value")
          .flex_1()
          .min_h_0()
          .overflow_y_scroll()
          .child(self.render_value(detail, cx))
          .when_some(sample_note, |body, shown| {
            body.child(
              div()
                .mt_2()
                .text_xs()
                .italic()
                .text_color(cx.theme().muted_foreground)
                .child(format!(
                  "showing the first {shown} of {} entries",
                  detail.size as u64
                )),
            )
          }),
      )
      .into_any_element()
  }

  fn render_console(&self, cx: &Context<Self>) -> AnyElement {
    v_flex()
      .size_full()
      .child(
        v_flex()
          .id("kv-console-log")
          .flex_1()
          .min_h_0()
          .overflow_y_scroll()
          .p_3()
          .gap_1()
          .text_xs()
          .font_family(crate::theme::mono(cx))
          .when(self.console_log.is_empty(), |log| {
            log.child(
              div()
                .text_color(cx.theme().muted_foreground)
                .child("redis> type a command (SET, GET, KEYS, …)"),
            )
          })
          .children(self.console_log.iter().map(|entry| {
            v_flex()
              .child(
                h_flex()
                  .gap_1()
                  .child(
                    div()
                      .text_color(cx.theme().muted_foreground)
                      .child("redis>"),
                  )
                  .child(entry.command.clone()),
              )
              .child(
                div()
                  .whitespace_normal()
                  .when(!entry.ok, |d| d.text_color(cx.theme().danger))
                  .child(entry.lines.join("\n")),
              )
          })),
      )
      .child(
        div()
          .p_2()
          .border_t_1()
          .border_color(cx.theme().border)
          .child(Input::new(&self.console_input).small()),
      )
      .into_any_element()
  }
}

impl Render for KvWorkspace {
  fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    let view = self.view;

    v_flex()
      .size_full()
      .track_focus(&self.focus_handle)
      .on_action(cx.listener(|this, _: &RefreshSchema, _, cx| {
        this.scan(true, cx);
        this.load_databases(cx);
      }))
      .on_action(cx.listener(|this, _: &FocusEditor, window, cx| {
        this.view = KvView::Console;
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
          h_resizable("kv-sidebar-main")
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
                      .h(px(34.))
                      .flex_none()
                      .gap_1()
                      .items_center()
                      .border_b_1()
                      .border_color(cx.theme().border)
                      .child(
                        Button::new("kv-view-key")
                          .ghost()
                          .xsmall()
                          .label("key")
                          .selected(view == KvView::Key)
                          .on_click(cx.listener(|this, _, _, cx| {
                            this.view = KvView::Key;
                            cx.notify();
                          })),
                      )
                      .child(
                        Button::new("kv-view-console")
                          .ghost()
                          .xsmall()
                          .label("console")
                          .selected(view == KvView::Console)
                          .on_click(cx.listener(|this, _, _, cx| {
                            this.view = KvView::Console;
                            cx.notify();
                          })),
                      ),
                  )
                  .child(match view {
                    KvView::Key => self.render_detail(cx),
                    KvView::Console => self.render_console(cx),
                  }),
              ),
            ),
        ),
      )
  }
}

#[cfg(test)]
mod tests {
  use std::sync::Mutex;

  use ::core::prelude::v1::test;
  use soquel_core::connectors::{Connection, HashField, KeyScanPage, KvBrowse, KvDatabaseKeys};
  use soquel_core::error::Error;
  use soquel_core::profiles::ConnectorKind;

  use super::*;
  use crate::test_support::shell_window;

  /// In-memory kv connection: every future resolves on its first poll, so the
  /// view runs fully deterministic under `run_until_parked`.
  struct FakeKv {
    keys: Mutex<std::collections::BTreeMap<String, (KeyValue, Option<f64>)>>,
    /// When non-empty, scan_keys pops these pages instead of reading `keys`.
    scan_script: Mutex<Vec<KeyScanPage>>,
    patterns: Mutex<Vec<String>>,
  }

  impl FakeKv {
    fn new(seed: &[(&str, KeyValue, Option<f64>)]) -> Arc<Self> {
      Arc::new(Self {
        keys: Mutex::new(
          seed
            .iter()
            .map(|(key, value, ttl)| (key.to_string(), (value.clone(), *ttl)))
            .collect(),
        ),
        scan_script: Mutex::new(Vec::new()),
        patterns: Mutex::new(Vec::new()),
      })
    }

    fn entry(key: &str, value: &KeyValue, ttl_ms: Option<f64>) -> KeyEntry {
      KeyEntry {
        key: key.to_string(),
        kind: value_kind(value),
        ttl_ms,
      }
    }

    fn matches(pattern: &str, key: &str) -> bool {
      let needle = pattern.trim_matches('*');
      needle.is_empty() || key.contains(needle)
    }

    fn missing(key: &str) -> Error {
      Error::Unsupported {
        message: format!("no such key {key}"),
      }
    }

    fn ttl_of(&self, key: &str) -> Option<f64> {
      self.keys.lock().unwrap().get(key).and_then(|entry| entry.1)
    }
  }

  #[async_trait::async_trait]
  impl Connection for FakeKv {
    async fn health(&self) -> Result<(), Error> {
      Ok(())
    }
    async fn close(&self) -> Result<(), Error> {
      Ok(())
    }
    fn kv(&self) -> Option<&dyn KvBrowse> {
      Some(self)
    }
  }

  #[async_trait::async_trait]
  impl KvBrowse for FakeKv {
    async fn databases(&self) -> Result<KvDatabases, Error> {
      let keys = self.keys.lock().unwrap();
      Ok(KvDatabases {
        current: 0,
        total: 16,
        used: vec![KvDatabaseKeys {
          db: 0,
          keys: keys.len() as f64,
        }],
      })
    }

    async fn scan_keys(
      &self,
      pattern: &str,
      _: Option<&str>,
      _: u32,
    ) -> Result<KeyScanPage, Error> {
      self.patterns.lock().unwrap().push(pattern.to_string());
      let scripted = {
        let mut script = self.scan_script.lock().unwrap();
        if script.is_empty() {
          None
        } else {
          Some(script.remove(0))
        }
      };
      if let Some(page) = scripted {
        return Ok(page);
      }
      let keys = self.keys.lock().unwrap();
      Ok(KeyScanPage {
        keys: keys
          .iter()
          .filter(|(key, _)| Self::matches(pattern, key))
          .map(|(key, (value, ttl))| Self::entry(key, value, *ttl))
          .collect(),
        cursor: None,
      })
    }

    async fn key_detail(&self, key: &str) -> Result<KeyDetail, Error> {
      let keys = self.keys.lock().unwrap();
      let (value, ttl_ms) = keys.get(key).ok_or_else(|| Self::missing(key))?;
      let size = match value {
        KeyValue::String { value } => value.len() as f64,
        other => sample_len(other).unwrap_or(0) as f64,
      };
      Ok(KeyDetail {
        key: key.to_string(),
        ttl_ms: *ttl_ms,
        size,
        value: value.clone(),
      })
    }

    async fn set_string(&self, key: &str, value: &str) -> Result<(), Error> {
      let mut keys = self.keys.lock().unwrap();
      let ttl = keys.get(key).and_then(|entry| entry.1);
      keys.insert(
        key.to_string(),
        (
          KeyValue::String {
            value: value.to_string(),
          },
          ttl,
        ),
      );
      Ok(())
    }

    async fn delete_key(&self, key: &str) -> Result<(), Error> {
      self
        .keys
        .lock()
        .unwrap()
        .remove(key)
        .map(|_| ())
        .ok_or_else(|| Self::missing(key))
    }

    async fn set_ttl(&self, key: &str, ttl_ms: Option<f64>) -> Result<(), Error> {
      let mut keys = self.keys.lock().unwrap();
      let entry = keys.get_mut(key).ok_or_else(|| Self::missing(key))?;
      entry.1 = ttl_ms;
      Ok(())
    }

    async fn run_command(&self, command: &str) -> Result<Vec<String>, Error> {
      let parts: Vec<&str> = command.split_whitespace().collect();
      match parts.as_slice() {
        ["PING"] => Ok(vec!["PONG".to_string()]),
        ["SET", key, value] => {
          self.keys.lock().unwrap().insert(
            key.to_string(),
            (
              KeyValue::String {
                value: value.to_string(),
              },
              None,
            ),
          );
          Ok(vec!["OK".to_string()])
        }
        _ => Err(Error::Unsupported {
          message: format!("fake console: unknown command {command}"),
        }),
      }
    }
  }

  fn string_value(value: &str) -> KeyValue {
    KeyValue::String {
      value: value.to_string(),
    }
  }

  fn fake_profile() -> ConnectionProfile {
    ConnectionProfile {
      id: "kv-fake".to_string(),
      name: "kv fake".to_string(),
      env: soquel_core::profiles::Env::Dev,
      group: None,
      agent_access: Default::default(),
      credential: Default::default(),
      params: soquel_core::profiles::ConnectorParams::Redis(soquel_core::profiles::RedisParams {
        host: "localhost".to_string(),
        port: 6379,
        db: 0,
        username: None,
        tls: false,
        tunnel_id: None,
      }),
    }
  }

  fn kv_view(
    fake: Arc<FakeKv>,
    cx: &mut gpui::TestAppContext,
  ) -> (Entity<KvWorkspace>, &mut gpui::VisualTestContext) {
    // The state is only reached by db switching, which these tests skip.
    let dir = tempfile::tempdir().unwrap();
    let state = std::sync::Arc::new(soquel_core::AppState::for_tests(
      dir.path(),
      Box::new(soquel_core::secrets::InMemoryStore::default()),
    ));
    let db = crate::core::fake_db(fake, ConnectorKind::Redis);
    shell_window(cx, move |window, cx| {
      KvWorkspace::new(state, db, fake_profile(), window, cx)
    })
  }

  #[gpui::test]
  fn scan_drains_empty_match_pages_until_keys_arrive(cx: &mut gpui::TestAppContext) {
    let fake = FakeKv::new(&[]);
    *fake.scan_script.lock().unwrap() = vec![
      KeyScanPage {
        keys: Vec::new(),
        cursor: Some("1".to_string()),
      },
      KeyScanPage {
        keys: Vec::new(),
        cursor: Some("2".to_string()),
      },
      KeyScanPage {
        keys: vec![FakeKv::entry("a", &string_value("x"), None)],
        cursor: Some("3".to_string()),
      },
    ];
    let (view, cx) = kv_view(fake.clone(), cx);
    cx.run_until_parked();
    cx.update(|_, cx| {
      let view = view.read(cx);
      assert_eq!(view.keys.len(), 1);
      assert_eq!(view.cursor.as_deref(), Some("3"));
      assert!(!view.scanning);
    });

    // "scan more" continues from the cursor and extends the list.
    *fake.scan_script.lock().unwrap() = vec![KeyScanPage {
      keys: vec![FakeKv::entry("b", &string_value("y"), None)],
      cursor: None,
    }];
    cx.update(|_, cx| view.update(cx, |view, cx| view.scan(false, cx)));
    cx.run_until_parked();
    cx.update(|_, cx| {
      let view = view.read(cx);
      assert_eq!(view.keys.len(), 2);
      assert!(view.cursor.is_none());
    });
  }

  #[gpui::test]
  fn the_scan_hop_cap_holds_on_a_hostile_keyspace(cx: &mut gpui::TestAppContext) {
    let fake = FakeKv::new(&[]);
    *fake.scan_script.lock().unwrap() = (0..60)
      .map(|ix| KeyScanPage {
        keys: Vec::new(),
        cursor: Some(ix.to_string()),
      })
      .collect();
    let (view, cx) = kv_view(fake.clone(), cx);
    cx.run_until_parked();
    cx.update(|_, cx| {
      let view = view.read(cx);
      assert!(view.keys.is_empty());
      // 50 hops, then the cap: the cursor stays open for "scan more".
      assert_eq!(view.cursor.as_deref(), Some("49"));
      assert!(!view.scanning);
    });
    assert_eq!(fake.scan_script.lock().unwrap().len(), 10);
  }

  #[gpui::test(iterations = 10)]
  fn a_superseded_scan_keeps_only_the_newest_result(cx: &mut gpui::TestAppContext) {
    let fake = FakeKv::new(&[
      ("alpha", string_value("1"), None),
      ("beta", string_value("2"), None),
    ]);
    let (view, cx) = kv_view(fake, cx);
    cx.run_until_parked();
    cx.update(|_, cx| assert_eq!(view.read(cx).keys.len(), 2));

    // Two scans before the executor runs: the first lands stale and is dropped.
    cx.update(|window, cx| {
      view.update(cx, |view, cx| {
        view
          .search
          .update(cx, |input, cx| input.set_value("alpha", window, cx));
        view.scan(true, cx);
        view
          .search
          .update(cx, |input, cx| input.set_value("beta", window, cx));
        view.scan(true, cx);
      });
    });
    cx.run_until_parked();
    cx.update(|_, cx| {
      let keys: Vec<&str> = view.read(cx).keys.iter().map(|k| k.key.as_str()).collect();
      assert_eq!(keys, vec!["beta"]);
    });
  }

  #[gpui::test]
  fn selecting_a_key_fills_the_drafts_from_the_detail(cx: &mut gpui::TestAppContext) {
    let fake = FakeKv::new(&[
      ("s", string_value("hello"), Some(90_000.0)),
      (
        "h",
        KeyValue::Hash {
          entries: vec![HashField {
            field: "f".to_string(),
            value: "v".to_string(),
          }],
        },
        None,
      ),
    ]);
    let (view, cx) = kv_view(fake, cx);
    cx.run_until_parked();

    cx.update(|window, cx| {
      view.update(cx, |view, cx| view.select_key("s".to_string(), window, cx));
    });
    cx.run_until_parked();
    cx.update(|_, cx| {
      let this = view.read(cx);
      assert!(!this.detail_loading);
      let detail = this.detail.as_ref().expect("string detail");
      assert_eq!(detail.key, "s");
      assert_eq!(detail.size, 5.0);
      assert_eq!(this.string_draft.read(cx).value().to_string(), "hello");
      assert_eq!(this.ttl_input.read(cx).value().to_string(), "90");
    });

    // A collection key leaves the string draft and ttl empty.
    cx.update(|window, cx| {
      view.update(cx, |view, cx| view.select_key("h".to_string(), window, cx));
    });
    cx.run_until_parked();
    cx.update(|_, cx| {
      let this = view.read(cx);
      assert!(matches!(
        this.detail.as_ref().map(|d| &d.value),
        Some(KeyValue::Hash { .. })
      ));
      assert_eq!(this.string_draft.read(cx).value().to_string(), "");
      assert_eq!(this.ttl_input.read(cx).value().to_string(), "");
    });
  }

  #[gpui::test(iterations = 10)]
  fn save_string_writes_and_reloads_keeping_the_ttl(cx: &mut gpui::TestAppContext) {
    let fake = FakeKv::new(&[("s", string_value("hello"), Some(90_000.0))]);
    let (view, cx) = kv_view(fake.clone(), cx);
    cx.run_until_parked();
    cx.update(|window, cx| {
      view.update(cx, |view, cx| view.select_key("s".to_string(), window, cx));
    });
    cx.run_until_parked();

    cx.update(|window, cx| {
      view.update(cx, |view, cx| {
        view
          .string_draft
          .update(cx, |input, cx| input.set_value("world", window, cx));
        view.save_string(cx);
      });
    });
    cx.run_until_parked();
    assert!(
      matches!(&fake.keys.lock().unwrap()["s"], (KeyValue::String { value }, Some(ttl)) if value == "world" && *ttl == 90_000.0)
    );
    cx.update(|_, cx| {
      let this = view.read(cx);
      assert!(matches!(
        this.detail.as_ref().map(|d| &d.value),
        Some(KeyValue::String { value }) if value == "world"
      ));
    });
  }

  #[gpui::test]
  fn ttl_validates_then_applies_and_persist_clears(cx: &mut gpui::TestAppContext) {
    let fake = FakeKv::new(&[("s", string_value("hello"), None)]);
    let (view, cx) = kv_view(fake.clone(), cx);
    cx.run_until_parked();
    cx.update(|window, cx| {
      view.update(cx, |view, cx| view.select_key("s".to_string(), window, cx));
    });
    cx.run_until_parked();

    // Garbage never reaches the connection.
    cx.update(|window, cx| {
      view.update(cx, |view, cx| {
        view
          .ttl_input
          .update(cx, |input, cx| input.set_value("nope", window, cx));
        view.submit_ttl(cx);
      });
    });
    cx.run_until_parked();
    cx.update(|_, cx| {
      assert_eq!(
        view.read(cx).status.to_string(),
        "TTL must be a positive number of seconds"
      );
    });
    assert_eq!(fake.ttl_of("s"), None);

    // Seconds convert to milliseconds and the detail reloads.
    cx.update(|window, cx| {
      view.update(cx, |view, cx| {
        view
          .ttl_input
          .update(cx, |input, cx| input.set_value("5", window, cx));
        view.submit_ttl(cx);
      });
    });
    cx.run_until_parked();
    assert_eq!(fake.ttl_of("s"), Some(5_000.0));
    cx.update(|_, cx| {
      let this = view.read(cx);
      assert_eq!(this.detail.as_ref().and_then(|d| d.ttl_ms), Some(5_000.0));
      assert_eq!(this.ttl_input.read(cx).value().to_string(), "5");
    });

    // Persist clears the expiry.
    cx.update(|_, cx| view.update(cx, |view, cx| view.apply_ttl(None, cx)));
    cx.run_until_parked();
    assert_eq!(fake.ttl_of("s"), None);
  }

  #[gpui::test(iterations = 10)]
  fn deleting_clears_the_selection_and_rescans(cx: &mut gpui::TestAppContext) {
    let fake = FakeKv::new(&[
      ("s", string_value("1"), None),
      ("t", string_value("2"), None),
    ]);
    let (view, cx) = kv_view(fake.clone(), cx);
    cx.run_until_parked();
    cx.update(|window, cx| {
      view.update(cx, |view, cx| view.select_key("s".to_string(), window, cx));
    });
    cx.run_until_parked();

    cx.update(|_, cx| view.update(cx, |view, cx| view.delete_key(cx)));
    cx.run_until_parked();
    assert!(!fake.keys.lock().unwrap().contains_key("s"));
    cx.update(|_, cx| {
      let this = view.read(cx);
      assert!(this.selected_key.is_none());
      assert!(this.detail.is_none());
      let keys: Vec<&str> = this.keys.iter().map(|k| k.key.as_str()).collect();
      assert_eq!(keys, vec!["t"]);
    });
  }

  #[gpui::test(iterations = 10)]
  fn the_console_logs_replies_and_errors_and_rescans(cx: &mut gpui::TestAppContext) {
    let fake = FakeKv::new(&[("s", string_value("1"), None)]);
    let (view, cx) = kv_view(fake, cx);
    cx.run_until_parked();

    // Empty input runs nothing.
    cx.update(|window, cx| {
      view.update(cx, |view, cx| view.run_console(window, cx));
    });
    cx.run_until_parked();
    cx.update(|_, cx| assert!(view.read(cx).console_log.is_empty()));

    let run = |cx: &mut gpui::VisualTestContext, command: &str| {
      let command = command.to_string();
      cx.update(|window, cx| {
        view.update(cx, |view, cx| {
          view
            .console_input
            .update(cx, |input, cx| input.set_value(command, window, cx));
          view.run_console(window, cx);
        });
      });
      cx.run_until_parked();
    };

    run(cx, "PING");
    cx.update(|_, cx| {
      let this = view.read(cx);
      let entry = this.console_log.last().expect("a log entry");
      assert!(entry.ok);
      assert_eq!(entry.lines, vec!["PONG"]);
      assert_eq!(this.console_input.read(cx).value().to_string(), "");
    });

    run(cx, "WHATEVER");
    cx.update(|_, cx| {
      let this = view.read(cx);
      let entry = this.console_log.last().expect("a log entry");
      assert!(!entry.ok);
      assert!(entry.lines[0].contains("unknown command"));
    });

    run(cx, "SET fresh 1");
    cx.update(|_, cx| {
      let this = view.read(cx);
      assert!(this.console_log.last().is_some_and(|entry| entry.ok));
      // A console write rescans: the new key shows up in the sidebar.
      assert!(this.keys.iter().any(|k| k.key == "fresh"));
    });
  }

  #[gpui::test]
  fn glob_toggles_the_scan_pattern(cx: &mut gpui::TestAppContext) {
    let fake = FakeKv::new(&[("cache:a", string_value("1"), None)]);
    let (view, cx) = kv_view(fake.clone(), cx);
    cx.run_until_parked();
    assert_eq!(
      fake.patterns.lock().unwrap().last().map(String::as_str),
      Some("")
    );

    cx.update(|window, cx| {
      view.update(cx, |view, cx| {
        view
          .search
          .update(cx, |input, cx| input.set_value("cache", window, cx));
        view.scan(true, cx);
      });
    });
    cx.run_until_parked();
    assert_eq!(
      fake.patterns.lock().unwrap().last().map(String::as_str),
      Some("*cache*")
    );

    cx.update(|_, cx| {
      view.update(cx, |view, cx| {
        view.glob = true;
        view.scan(true, cx);
      });
    });
    cx.run_until_parked();
    assert_eq!(
      fake.patterns.lock().unwrap().last().map(String::as_str),
      Some("cache")
    );
  }

  #[test]
  fn contains_wraps_and_escapes_glob_specials() {
    assert_eq!(contains_pattern("session"), "*session*");
    assert_eq!(contains_pattern(""), "");
    assert_eq!(contains_pattern("cache:user:*"), "*cache:user:\\**");
    assert_eq!(contains_pattern("a?b[c]d\\e"), "*a\\?b\\[c\\]d\\\\e*");
  }

  #[test]
  fn ttl_counts_down_in_the_biggest_fitting_unit() {
    assert_eq!(format_ttl(500.0), "1s");
    assert_eq!(format_ttl(42_000.0), "42s");
    assert_eq!(format_ttl(300_000.0), "5m");
    assert_eq!(format_ttl(7_200_000.0), "2h");
    assert_eq!(format_ttl(172_800_000.0), "2d");
  }

  /// The view against a live redis: it scans on open, a click loads the
  /// detail, the console runs. Skipped silently without SOQUEL_TEST_REDIS.
  #[gpui::test]
  fn integration_kv_workspace_scans_selects_and_runs(cx: &mut gpui::TestAppContext) {
    // Real redis IO wakes from tokio's driver thread.
    cx.executor().allow_parking();
    let Some(coord) = soquel_core::integration_env("SOQUEL_TEST_REDIS") else {
      return;
    };
    let (host, port) = coord.split_once(':').expect("host:port");
    let profile = ConnectionProfile {
      id: "kv-view".to_string(),
      name: "redis view".to_string(),
      env: soquel_core::profiles::Env::Dev,
      group: None,
      agent_access: soquel_core::profiles::AgentAccess::None,
      credential: soquel_core::profiles::CredentialSource::Prompt,
      params: soquel_core::profiles::ConnectorParams::Redis(soquel_core::profiles::RedisParams {
        host: host.to_string(),
        port: port.parse().expect("port"),
        db: 0,
        username: None,
        tls: false,
        tunnel_id: None,
      }),
    };
    let db = crate::core::connect_with_blocking(
      profile.clone(),
      soquel_core::credentials::Credentials::fixed(Some("soquel".to_string())),
    )
    .expect("connects to the compose redis");

    // Seed one key this test owns, and a couple more to browse.
    let prefix = format!("gpui_view_{}", std::process::id());
    for line in [
      format!("HSET {prefix}:h field1 v1 field2 v2"),
      format!("SET {prefix}:s hello"),
    ] {
      crate::core::kv_run_command_blocking(&db, line).expect("seed");
    }

    let dir = tempfile::tempdir().unwrap();
    let state = std::sync::Arc::new(soquel_core::AppState::for_tests(
      dir.path(),
      Box::new(soquel_core::secrets::InMemoryStore::default()),
    ));
    let (view, cx) = crate::test_support::shell_window(cx, {
      let state = state.clone();
      move |window, cx| KvWorkspace::new(state, db, profile, window, cx)
    });

    // Filter to this test's keys, then the scan (from `new`) settles.
    cx.update(|window, cx| {
      view.update(cx, |view, cx| {
        view
          .search
          .update(cx, |input, cx| input.set_value(prefix.clone(), window, cx));
        view.scan(true, cx);
      });
    });
    let hash_key = format!("{prefix}:h");
    crate::test_support::wait_until(cx, "the seeded keys to scan in", |cx| {
      cx.update(|_, cx| view.read(cx).keys.iter().any(|k| k.key == hash_key))
    });

    // A click loads the hash detail with its two fields.
    cx.update(|window, cx| {
      view.update(cx, |view, cx| view.select_key(hash_key.clone(), window, cx));
    });
    crate::test_support::wait_until(cx, "the hash detail", |cx| {
      cx.update(|_, cx| view.read(cx).detail.is_some())
    });
    cx.update(|_, cx| {
      let detail = view.read(cx).detail.clone().unwrap();
      assert_eq!(detail.key, hash_key);
      let KeyValue::Hash { entries } = &detail.value else {
        panic!("hash value");
      };
      assert_eq!(entries.len(), 2);
    });

    // The console runs a command and logs its reply.
    cx.update(|window, cx| {
      view.update(cx, |view, cx| {
        view
          .console_input
          .update(cx, |input, cx| input.set_value("PING", window, cx));
        view.run_console(window, cx);
      });
    });
    crate::test_support::wait_until(cx, "the console reply", |cx| {
      cx.update(|_, cx| {
        view
          .read(cx)
          .console_log
          .last()
          .is_some_and(|entry| entry.ok && entry.lines.iter().any(|l| l == "PONG"))
      })
    });

    // Cleanup.
    for suffix in ["h", "s"] {
      let _ = crate::core::kv_run_command_blocking(
        &db_handle(&view, cx),
        format!("DEL {prefix}:{suffix}"),
      );
    }
  }

  fn db_handle(view: &Entity<KvWorkspace>, cx: &mut gpui::VisualTestContext) -> Db {
    cx.update(|_, cx| view.read(cx).db.clone())
  }
}
