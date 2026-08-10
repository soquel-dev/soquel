mod completion;
mod core;
mod grid;

use gpui::*;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputState};
use gpui_component::resizable::{ResizableState, resizable_panel, v_resizable};
use gpui_component::table::{DataTable, TableState};
use gpui_component::{ActiveTheme, Disableable, Root, Sizable, TitleBar, h_flex, v_flex};

use crate::completion::{SchemaEntries, SqlCompletionProvider};
use crate::core::Db;
use crate::grid::{PAGE_SIZE, RowsDelegate};

actions!(soquel, [RunQuery]);

pub struct Workspace {
  editor: Entity<InputState>,
  table: Entity<TableState<RowsDelegate>>,
  split: Entity<ResizableState>,
  db: Option<Db>,
  running: bool,
  _schema_entries: SchemaEntries,
  _connect_task: Task<()>,
  _run_task: Task<()>,
}

impl Workspace {
  fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
    let target = std::env::var("SOQUEL_GPUI_TABLE").unwrap_or_else(|_| "app.events".into());
    let (schema, table_name) = target.split_once('.').unwrap_or(("public", &target));

    let (provider, schema_entries) = SqlCompletionProvider::new();
    let editor = cx.new(|cx| {
      let mut state = InputState::new(window, cx)
        .code_editor("sql")
        .line_number(true)
        .searchable(true)
        .placeholder("select * from app.events limit 100  --  ctrl+enter runs");
      state.lsp.completion_provider = Some(provider);
      state
    });

    let table = cx.new(|cx| {
      TableState::new(
        RowsDelegate::new(schema.to_string(), table_name.to_string()),
        window,
        cx,
      )
      .cell_selectable(true)
      .col_resizable(true)
    });

    cx.observe(&table, |_, _, cx| cx.notify()).detach();

    let split = cx.new(|_| ResizableState::default());

    let connect_task = {
      let table = table.clone();
      cx.spawn(async move |this, cx| {
        let connected = core::connect_dev().await;
        let db = match connected {
          Ok(Ok(db)) => db,
          Ok(Err(error)) => {
            table.update(cx, |table, cx| {
              table.delegate_mut().loading = false;
              table.delegate_mut().status = format!("error: {error}").into();
              cx.notify();
            });
            return;
          }
          Err(_) => return,
        };

        let _ = this.update(cx, |this, _| this.db = Some(db.clone()));

        let request = {
          let (schema, name) = table.read_with(cx, |table, _| {
            let d = table.delegate();
            (d.schema.clone(), d.table.clone())
          });
          core::page_request(&schema, &name, 0, PAGE_SIZE)
        };
        let first_page = core::fetch_rows(&db, request).await;

        table.update(cx, |table, cx| {
          {
            let delegate = table.delegate_mut();
            delegate.db = Some(db.clone());
            delegate.loading = false;
            match first_page {
              Ok(Ok(result)) => {
                if let Some(statement) = result.statements.into_iter().next() {
                  delegate.columns = statement.columns;
                  delegate.eof = (statement.rows.len() as u32) < PAGE_SIZE;
                  delegate.rows = statement.rows;
                }
                delegate.status = format!(
                  "{}.{} - {} rows - first page {:.0} ms",
                  delegate.schema,
                  delegate.table,
                  delegate.rows.len(),
                  result.duration_ms
                )
                .into();
              }
              Ok(Err(error)) => {
                delegate.status = format!("error: {error}").into();
              }
              Err(_) => {
                delegate.status = "error: fetch canceled".into();
              }
            }
          }
          // Columns land after creation: the state only re-reads them on refresh.
          table.refresh(cx);
          cx.notify();
        });

        if let Ok(Ok(snapshot)) = core::schema_snapshot(&db).await {
          let _ = this.update(cx, |this, _| this._schema_entries.fill(&snapshot));
        }
      })
    };

    Self {
      editor,
      table,
      split,
      db: None,
      running: false,
      _schema_entries: schema_entries,
      _connect_task: connect_task,
      _run_task: Task::ready(()),
    }
  }

  fn run(&mut self, _: &mut Window, cx: &mut Context<Self>) {
    if self.running {
      return;
    }
    let Some(db) = self.db.clone() else {
      return;
    };
    let sql = self.editor.read(cx).value().to_string();
    if sql.trim().is_empty() {
      return;
    }

    self.running = true;
    self.table.update(cx, |table, cx| {
      table.delegate_mut().status = "running...".into();
      cx.notify();
    });

    let rx = core::run_query(&db, sql);
    let table = self.table.clone();
    self._run_task = cx.spawn(async move |this, cx| {
      let result = rx.await;
      let _ = this.update(cx, |this, _| this.running = false);
      table.update(cx, |table, cx| {
        {
          let delegate = table.delegate_mut();
          // Query results replace the browse view; no paging on them.
          delegate.db = None;
          delegate.eof = true;
          delegate.loading = false;
          match result {
            Ok(Ok(result)) => {
              let total_affected: f64 = result.statements.iter().map(|s| s.rows_affected).sum();
              let display = result
                .statements
                .into_iter()
                .rev()
                .find(|s| !s.columns.is_empty());
              match display {
                Some(statement) => {
                  delegate.status = format!(
                    "{} rows - {:.0} ms",
                    statement.rows.len(),
                    result.duration_ms
                  )
                  .into();
                  delegate.columns = statement.columns;
                  delegate.rows = statement.rows;
                }
                None => {
                  delegate.columns = Vec::new();
                  delegate.rows = Vec::new();
                  delegate.status = format!(
                    "{} rows affected - {:.0} ms",
                    total_affected, result.duration_ms
                  )
                  .into();
                }
              }
            }
            Ok(Err(error)) => {
              delegate.status = format!("error: {error}").into();
            }
            Err(_) => {
              delegate.status = "error: query canceled".into();
            }
          }
        }
        table.refresh(cx);
        cx.notify();
      });
    });
  }
}

impl Render for Workspace {
  fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    let status = self.table.read(cx).delegate().status.clone();

    v_flex()
      .size_full()
      .bg(cx.theme().background)
      .child(TitleBar::new().child(h_flex().child("soquel")))
      .child(
        h_flex()
          .px_3()
          .py_1()
          .gap_3()
          .border_b_1()
          .border_color(cx.theme().border)
          .child(
            Button::new("run")
              .primary()
              .xsmall()
              .label(if self.running { "Running..." } else { "Run" })
              .disabled(self.running || self.db.is_none())
              .on_click(cx.listener(|this, _, window, cx| this.run(window, cx))),
          )
          .child(
            div()
              .text_sm()
              .text_color(cx.theme().muted_foreground)
              .child(status),
          ),
      )
      .child(
        v_flex().flex_1().min_h_0().child(
          v_resizable("editor-results")
            .with_state(&self.split)
            .child(
              resizable_panel()
                .size(px(200.))
                .size_range(px(80.)..px(600.))
                .child(
                  v_flex()
                    .size_full()
                    .p_1()
                    .on_action(cx.listener(|this, _: &RunQuery, window, cx| this.run(window, cx)))
                    .child(Input::new(&self.editor).h_full()),
                ),
            )
            .child(
              resizable_panel().child(
                v_flex()
                  .size_full()
                  .p_1()
                  .child(DataTable::new(&self.table).stripe(true)),
              ),
            ),
        ),
      )
  }
}

fn main() {
  gpui_platform::application().run(move |cx| {
    gpui_component::init(cx);
    // Registered after init so it shadows the editor's own secondary-enter
    // (which would insert a newline in multi-line mode).
    cx.bind_keys([KeyBinding::new("secondary-enter", RunQuery, Some("Input"))]);

    cx.spawn(async move |cx| {
      cx.open_window(
        WindowOptions {
          titlebar: Some(TitleBar::title_bar_options()),
          ..Default::default()
        },
        |window, cx| {
          let view = cx.new(|cx| Workspace::new(window, cx));
          cx.new(|cx| Root::new(view, window, cx))
        },
      )
      .expect("failed to open window");
    })
    .detach();
  });
}
