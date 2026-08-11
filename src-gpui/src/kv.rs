//! The redis / key-value browser: a whole workspace of its own, mounted when a
//! connection browses keys instead of the SQL shell. Pure logic ported from
//! the webview's lib/kv.ts.

use std::sync::Arc;

use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::resizable::{ResizableState, h_resizable, resizable_panel};
use gpui_component::select::{Select, SelectEvent, SelectState};
use gpui_component::{ActiveTheme, IndexPath, Selectable, Sizable, StyledExt, h_flex, v_flex};
use soquel_core::AppState;
use soquel_core::connectors::{KeyDetail, KeyEntry, KeyKind, KeyValue, KvDatabases};
use soquel_core::profiles::ConnectionProfile;

use crate::core::{self, Db};

/// Short badge label + colour per redis type (the webview's kv.ts map).
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
/// specials, wrap in `*...*`. Empty stays empty (the webview shows everything).
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

pub enum KvWorkspaceEvent {
  Close,
}

pub struct KvWorkspace {
  state: Arc<AppState>,
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
  _task: Task<()>,
}

impl EventEmitter<KvWorkspaceEvent> for KvWorkspace {}

impl KvWorkspace {
  pub fn new(
    state: Arc<AppState>,
    db: Db,
    profile: ConnectionProfile,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) -> Self {
    let server_version = db.server_version();
    let search = cx.new(|cx| InputState::new(window, cx).placeholder("search keys"));
    let string_draft = cx.new(|cx| InputState::new(window, cx).multi_line(true));
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
      _task: Task::ready(()),
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
    self._task = cx.spawn(async move |this, cx| {
      let mut cursor = start;
      let mut fresh: Vec<KeyEntry> = Vec::new();
      let mut done = false;
      for _ in 0..50 {
        let rx = core::kv_scan(&db, pattern.clone(), cursor.clone(), SCAN_COUNT);
        match rx.await {
          Ok(Ok(page)) => {
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
          Ok(Err(error)) => {
            let _ = this.update(cx, |this, cx| {
              this.scanning = false;
              this.status = format!("error: {error}").into();
              cx.notify();
            });
            return;
          }
          Err(_) => return,
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
    let rx = core::kv_databases(&self.db);
    let this = cx.entity();
    let db_select = self.db_select.clone();
    cx.spawn(async move |_, cx| {
      let Ok(Ok(databases)) = rx.await else {
        return;
      };
      let Some(handle) = cx.update(|cx| cx.active_window()) else {
        return;
      };
      let _ = cx.update_window(handle, move |_, window, cx| {
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
        this.update(cx, |this, cx| {
          this.db_indices = indices;
          this.databases = Some(databases);
          cx.notify();
        });
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
    let rx = core::kv_select_db(self.state.clone(), self.connection_id.clone(), db);
    self._task = cx.spawn(async move |this, cx| match rx.await {
      Ok(Ok(fresh)) => {
        let _ = this.update(cx, |this, cx| {
          this.db = fresh;
          this.selected_key = None;
          this.detail = None;
          this.scan(true, cx);
          this.load_databases(cx);
        });
      }
      Ok(Err(error)) => {
        let _ = this.update(cx, |this, cx| {
          this.status = format!("error: {error}").into();
          cx.notify();
        });
      }
      Err(_) => {}
    });
  }

  fn select_key(&mut self, key: String, window: &mut Window, cx: &mut Context<Self>) {
    self.selected_key = Some(key.clone());
    self.view = KvView::Key;
    self.detail = None;
    self.detail_loading = true;
    self.delete_armed = false;
    cx.notify();
    let rx = core::kv_key_detail(&self.db, key);
    let this = cx.entity();
    let string_draft = self.string_draft.clone();
    let ttl_input = self.ttl_input.clone();
    let _ = window;
    self._task = cx.spawn(async move |_, cx| {
      let result = rx.await;
      let Some(handle) = cx.update(|cx| cx.active_window()) else {
        return;
      };
      let _ = cx.update_window(handle, move |_, window, cx| {
        this.update(cx, |this, cx| {
          this.detail_loading = false;
          cx.notify();
        });
        match result {
          Ok(Ok(detail)) => {
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
            this.update(cx, |this, cx| {
              this.detail = Some(detail);
              cx.notify();
            });
          }
          Ok(Err(error)) => {
            this.update(cx, |this, cx| {
              this.status = format!("error: {error}").into();
              cx.notify();
            });
          }
          Err(_) => {}
        }
      });
    });
  }

  fn run_console(&mut self, _: &mut Window, cx: &mut Context<Self>) {
    let command = self.console_input.read(cx).value().trim().to_string();
    if command.is_empty() {
      return;
    }
    let rx = core::kv_run_command(&self.db, command.clone());
    let this = cx.entity();
    let input = self.console_input.clone();
    self._task = cx.spawn(async move |_, cx| {
      let result = rx.await;
      let Some(handle) = cx.update(|cx| cx.active_window()) else {
        return;
      };
      let _ = cx.update_window(handle, move |_, window, cx| {
        let entry = match result {
          Ok(Ok(lines)) => ConsoleEntry {
            command: command.clone(),
            lines,
            ok: true,
          },
          Ok(Err(error)) => ConsoleEntry {
            command: command.clone(),
            lines: vec![format!("{error}")],
            ok: false,
          },
          Err(_) => return,
        };
        input.update(cx, |input, cx| input.set_value("", window, cx));
        this.update(cx, |this, cx| {
          this.console_log.push(entry);
          // A write via the console shifts the keyspace.
          this.scan(true, cx);
          this.load_databases(cx);
          cx.notify();
        });
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
    let rx = core::kv_set_string(&self.db, key.clone(), value);
    self._task = self.after_write(rx, key, cx);
  }

  fn apply_ttl(&mut self, ttl_ms: Option<f64>, cx: &mut Context<Self>) {
    let Some(key) = self.selected_key.clone() else {
      return;
    };
    let rx = core::kv_set_ttl(&self.db, key.clone(), ttl_ms);
    self._task = self.after_write(rx, key, cx);
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
    let rx = core::kv_delete_key(&self.db, key);
    self._task = cx.spawn(async move |this, cx| {
      let result = rx.await;
      let _ = this.update(cx, |this, cx| {
        match result {
          Ok(Ok(())) => {
            this.selected_key = None;
            this.detail = None;
            this.delete_armed = false;
            this.scan(true, cx);
            this.load_databases(cx);
          }
          Ok(Err(error)) => this.status = format!("error: {error}").into(),
          Err(_) => {}
        }
        cx.notify();
      });
    });
  }

  /// A key write reloads its detail and re-scans (ttl/size may have moved).
  fn after_write(
    &self,
    rx: futures::channel::oneshot::Receiver<Result<(), soquel_core::error::Error>>,
    key: String,
    cx: &mut Context<Self>,
  ) -> Task<()> {
    cx.spawn(async move |this, cx| {
      let result = rx.await;
      let Some(handle) = cx.update(|cx| cx.active_window()) else {
        return;
      };
      let _ = cx.update_window(handle, move |_, window, cx| {
        let _ = this.update(cx, |this, cx| match result {
          Ok(Ok(())) => {
            this.scan(true, cx);
            this.load_databases(cx);
            this.select_key(key.clone(), window, cx);
          }
          Ok(Err(error)) => {
            this.status = format!("error: {error}").into();
            cx.notify();
          }
          Err(_) => {}
        });
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
    div()
      .px_1p5()
      .rounded(cx.theme().radius)
      .bg(color.opacity(0.12))
      .text_color(color)
      .text_xs()
      .font_family("IBM Plex Mono")
      .child(label)
  }

  fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
    let count = self.keys.len();
    v_flex()
      .size_full()
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
              .child("keys"),
          )
          .child(
            div()
              .w(px(140.))
              .child(Select::new(&self.db_select).small()),
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
        v_flex()
          .id("kv-keys")
          .flex_1()
          .min_h_0()
          .overflow_y_scroll()
          .children(self.keys.clone().into_iter().map(|entry| {
            let key = entry.key.clone();
            let selected = self.selected_key.as_deref() == Some(entry.key.as_str());
            h_flex()
              .id(SharedString::from(format!("key-{}", entry.key)))
              .px_2()
              .py_1()
              .gap_2()
              .items_center()
              .cursor_default()
              .text_xs()
              .font_family("IBM Plex Mono")
              .when(selected, |row| row.bg(cx.theme().accent))
              .hover(|row| row.bg(cx.theme().accent.opacity(0.5)))
              .on_click(cx.listener(move |this, _, window, cx| {
                this.select_key(key.clone(), window, cx);
              }))
              .child(self.type_badge(entry.kind, false, cx))
              .child(div().flex_1().min_w_0().truncate().child(entry.key.clone()))
              .when_some(entry.ttl_ms, |row, ttl| {
                row.child(
                  div()
                    .text_color(cx.theme().muted_foreground)
                    .child(format_ttl(ttl)),
                )
              })
          })),
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
        .font_family("IBM Plex Mono")
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
            .font_family("IBM Plex Mono")
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
        .font_family("IBM Plex Mono")
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
        .font_family("IBM Plex Mono")
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
              .font_family("IBM Plex Mono")
              .text_sm()
              .child(detail.key.clone()),
          )
          .child(
            div()
              .text_color(cx.theme().muted_foreground)
              .text_xs()
              .font_family("IBM Plex Mono")
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
          .font_family("IBM Plex Mono")
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
          .font_family("IBM Plex Mono")
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
    let version = self
      .server_version
      .clone()
      .map(|v| format!("Redis {v}"))
      .unwrap_or_else(|| "Redis".to_string());
    let view = self.view;

    v_flex()
      .size_full()
      .bg(cx.theme().background)
      .child(
        h_flex()
          .px_4()
          .py_2()
          .justify_between()
          .items_center()
          .border_b_1()
          .border_color(cx.theme().border)
          .child(
            h_flex()
              .gap_2()
              .items_center()
              .child(div().font_semibold().text_sm().child(self.name.clone()))
              .child(
                div()
                  .text_xs()
                  .font_family("IBM Plex Mono")
                  .text_color(cx.theme().muted_foreground)
                  .child(version),
              ),
          )
          .child(
            Button::new("kv-disconnect")
              .ghost()
              .xsmall()
              .label("Disconnect")
              .on_click(cx.listener(|_, _, _, cx| cx.emit(KvWorkspaceEvent::Close))),
          ),
      )
      .when(!self.status.is_empty(), |this| {
        this.child(
          div()
            .px_4()
            .py_1()
            .text_xs()
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
                  .child(
                    h_flex()
                      .px_2()
                      .py_1()
                      .gap_1()
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
  use ::core::prelude::v1::test;

  use super::*;

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
    let Ok(coord) = std::env::var("SOQUEL_TEST_REDIS") else {
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
    let db = futures::executor::block_on(crate::core::connect_with(
      profile.clone(),
      soquel_core::credentials::Credentials::fixed(Some("soquel".to_string())),
    ))
    .expect("channel")
    .expect("connects to the compose redis");

    // Seed one key this test owns, and a couple more to browse.
    let prefix = format!("gpui_view_{}", std::process::id());
    for line in [
      format!("HSET {prefix}:h field1 v1 field2 v2"),
      format!("SET {prefix}:s hello"),
    ] {
      futures::executor::block_on(crate::core::kv_run_command(&db, line))
        .expect("channel")
        .expect("seed");
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
      let _ = futures::executor::block_on(crate::core::kv_run_command(
        &db_handle(&view, cx),
        format!("DEL {prefix}:{suffix}"),
      ));
    }
  }

  fn db_handle(view: &Entity<KvWorkspace>, cx: &mut gpui::VisualTestContext) -> Db {
    cx.update(|_, cx| view.read(cx).db.clone())
  }
}
