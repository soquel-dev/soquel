use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::resizable::{ResizableState, h_resizable, resizable_panel, v_resizable};
use gpui_component::select::{Select, SelectEvent, SelectState};
use gpui_component::table::{DataTable, TableState};
use gpui_component::{
  ActiveTheme, Disableable, Icon, IconName, IndexPath, Sizable, StyledExt, TitleBar, h_flex, v_flex,
};
use soquel_core::connectors::{ColumnFilter, FilterOp, SchemaSnapshot};

use crate::actions::{FocusEditor, RefreshSchema, RunQuery, ToggleThemeMode};
use crate::completion::{SchemaEntries, SqlCompletionProvider};
use crate::core::{self, Db};
use crate::filters::{filter_label, op_label, op_needs_value, ops_for_kind};
use crate::grid::RowsDelegate;
use crate::theme;

pub struct Workspace {
  editor: Entity<InputState>,
  table: Entity<TableState<RowsDelegate>>,
  splits: (Entity<ResizableState>, Entity<ResizableState>),
  db: Option<Db>,
  snapshot: Option<SchemaSnapshot>,
  server_version: Option<String>,
  browsing: Option<(String, String)>,
  running: bool,
  filter_open: bool,
  filter_column: Entity<SelectState<Vec<String>>>,
  filter_op: Entity<SelectState<Vec<String>>>,
  filter_ops: Vec<FilterOp>,
  filter_value: Entity<InputState>,
  schema_entries: SchemaEntries,
  _connect_task: Task<()>,
  _work_task: Task<()>,
}

impl Workspace {
  pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
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
      TableState::new(RowsDelegate::new(), window, cx)
        .cell_selectable(true)
        .col_resizable(true)
    });
    cx.observe(&table, |_, _, cx| cx.notify()).detach();

    let splits = (
      cx.new(|_| ResizableState::default()),
      cx.new(|_| ResizableState::default()),
    );

    let filter_column = cx.new(|cx| SelectState::new(Vec::<String>::new(), None, window, cx));
    let filter_op = cx.new(|cx| SelectState::new(Vec::<String>::new(), None, window, cx));
    let filter_value = cx.new(|cx| InputState::new(window, cx).placeholder("value"));

    cx.subscribe_in(
      &filter_column,
      window,
      |this, _, event: &SelectEvent<Vec<String>>, window, cx| {
        let SelectEvent::Confirm(Some(column)) = event else {
          return;
        };
        this.sync_filter_ops(column.clone(), window, cx);
      },
    )
    .detach();

    cx.subscribe_in(
      &filter_value,
      window,
      |this, _, event: &InputEvent, _, cx| {
        if let InputEvent::PressEnter { .. } = event {
          this.apply_filter(cx);
        }
      },
    )
    .detach();

    let connect_task = cx.spawn(async move |this, cx| {
      let db = match core::connect_dev().await {
        Ok(Ok(db)) => db,
        Ok(Err(error)) => {
          let _ = this.update(cx, |this, cx| {
            this.set_status(format!("error: {error}"), cx);
          });
          return;
        }
        Err(_) => return,
      };

      let _ = this.update(cx, |this, cx| {
        this.db = Some(db.clone());
        this.server_version = db.server_version();
        cx.notify();
      });

      // Schema first: it fills the sidebar and the completion entries.
      if let Ok(Ok(snapshot)) = core::schema_snapshot(&db).await {
        let _ = this.update(cx, |this, cx| {
          this.schema_entries.fill(&snapshot);
          this.snapshot = Some(snapshot);
          cx.notify();
        });
      }

      let target = std::env::var("SOQUEL_GPUI_TABLE").unwrap_or_else(|_| "app.events".into());
      let (schema, table) = target.split_once('.').unwrap_or(("public", &*target));
      let _ = this.update(cx, |this, cx| {
        this.browse(schema.to_string(), table.to_string(), cx);
      });
    });

    Self {
      editor,
      table,
      splits,
      db: None,
      snapshot: None,
      server_version: None,
      browsing: None,
      running: false,
      filter_open: false,
      filter_column,
      filter_op,
      filter_ops: Vec::new(),
      filter_value,
      schema_entries,
      _connect_task: connect_task,
      _work_task: Task::ready(()),
    }
  }

  fn sync_filter_ops(&mut self, column: String, window: &mut Window, cx: &mut Context<Self>) {
    let kind = self
      .table
      .read(cx)
      .delegate()
      .columns
      .iter()
      .find(|col| col.name == column)
      .map(|col| col.kind)
      .unwrap_or_default();
    self.filter_ops = ops_for_kind(kind);
    let labels: Vec<String> = self
      .filter_ops
      .iter()
      .map(|op| op_label(*op).to_string())
      .collect();
    self.filter_op.update(cx, |state, cx| {
      state.set_items(labels, window, cx);
      state.set_selected_index(Some(IndexPath::default()), window, cx);
    });
  }

  fn toggle_filter_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
    self.filter_open = !self.filter_open;
    if self.filter_open {
      let names: Vec<String> = self
        .table
        .read(cx)
        .delegate()
        .columns
        .iter()
        .map(|col| col.name.clone())
        .collect();
      self.filter_column.update(cx, |state, cx| {
        state.set_items(names, window, cx);
      });
    }
    cx.notify();
  }

  fn apply_filter(&mut self, cx: &mut Context<Self>) {
    let Some(column) = self.filter_column.read(cx).selected_value().cloned() else {
      return;
    };
    let Some(op_text) = self.filter_op.read(cx).selected_value().cloned() else {
      return;
    };
    let Some(op) = self
      .filter_ops
      .iter()
      .copied()
      .find(|op| op_label(*op) == op_text)
    else {
      return;
    };
    let value = self.filter_value.read(cx).value().to_string();
    if op_needs_value(op) && value.trim().is_empty() {
      return;
    }
    let filter = ColumnFilter {
      column: column.clone(),
      op,
      value: op_needs_value(op).then_some(value),
    };
    self.table.update(cx, |table, cx| {
      let delegate = table.delegate_mut();
      delegate.filters.retain(|f| f.column != column);
      delegate.filters.push(filter);
      delegate.reload(cx);
    });
    cx.notify();
  }

  fn remove_filter(&mut self, column: &str, cx: &mut Context<Self>) {
    self.table.update(cx, |table, cx| {
      let delegate = table.delegate_mut();
      delegate.filters.retain(|f| f.column != column);
      delegate.reload(cx);
    });
    cx.notify();
  }

  fn clear_filters(&mut self, cx: &mut Context<Self>) {
    self.table.update(cx, |table, cx| {
      let delegate = table.delegate_mut();
      delegate.filters.clear();
      delegate.reload(cx);
    });
    cx.notify();
  }

  fn set_status(&mut self, status: impl Into<SharedString>, cx: &mut Context<Self>) {
    self.table.update(cx, |table, cx| {
      table.delegate_mut().status = status.into();
      cx.notify();
    });
  }

  /// Point the grid at a table and reload; the delegate pages, sorts and filters.
  fn browse(&mut self, schema: String, name: String, cx: &mut Context<Self>) {
    let Some(db) = self.db.clone() else {
      return;
    };
    self.browsing = Some((schema.clone(), name.clone()));
    self.table.update(cx, |table, cx| {
      let delegate = table.delegate_mut();
      delegate.browse = Some((db, schema, name));
      // A new table means new columns: sort and filters do not carry over.
      delegate.sort = None;
      delegate.filters.clear();
      delegate.reload(cx);
    });
  }

  fn refresh_schema(&mut self, cx: &mut Context<Self>) {
    let Some(db) = self.db.clone() else {
      return;
    };
    self._work_task = cx.spawn(async move |this, cx| {
      if let Ok(Ok(snapshot)) = core::schema_snapshot(&db).await {
        let _ = this.update(cx, |this, cx| {
          this.schema_entries.fill(&snapshot);
          this.snapshot = Some(snapshot);
          cx.notify();
        });
      }
    });
  }

  fn run(&mut self, cx: &mut Context<Self>) {
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
    self.set_status("running...", cx);

    let rx = core::run_query(&db, sql);
    let table = self.table.clone();
    self._work_task = cx.spawn(async move |this, cx| {
      let result = rx.await;
      let _ = this.update(cx, |this, cx| {
        this.running = false;
        this.browsing = None;
        cx.notify();
      });
      table.update(cx, |table, cx| {
        {
          let delegate = table.delegate_mut();
          // Query results replace the browse view; no paging on them.
          delegate.browse = None;
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

  fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
    let mut rows = Vec::new();
    if let Some(snapshot) = &self.snapshot {
      for schema in &snapshot.schemas {
        for table in &schema.tables {
          let schema = schema.name.clone();
          let name = table.name.clone();
          let selected = self.browsing.as_ref() == Some(&(schema.clone(), name.clone()));
          let label = format!("{schema}.{name}");
          rows.push(
            div()
              .id(SharedString::from(format!("t-{schema}-{name}")))
              .px_2()
              .py_1()
              .rounded(cx.theme().radius)
              .text_sm()
              .cursor_default()
              .when(selected, |this| {
                this
                  .bg(cx.theme().accent)
                  .text_color(cx.theme().accent_foreground)
              })
              .hover(|this| this.bg(cx.theme().accent))
              .on_click(cx.listener(move |this, _, _, cx| {
                this.browse(schema.clone(), name.clone(), cx);
              }))
              .child(label),
          );
        }
      }
    }
    v_flex()
      .size_full()
      .bg(cx.theme().sidebar)
      .child(
        h_flex()
          .px_3()
          .py_2()
          .justify_between()
          .items_center()
          .child(
            div()
              .text_xs()
              .font_semibold()
              .text_color(cx.theme().muted_foreground)
              .child("TABLES"),
          )
          .child(
            Button::new("refresh-schema")
              .ghost()
              .xsmall()
              .label("refresh")
              .on_click(cx.listener(|this, _, _, cx| this.refresh_schema(cx))),
          ),
      )
      .child(
        v_flex()
          .id("tables")
          .flex_1()
          .min_h_0()
          .overflow_y_scroll()
          .px_2()
          .gap_px()
          .children(rows),
      )
  }

  fn render_status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
    let status = self.table.read(cx).delegate().status.clone();
    let connection = match (&self.db, &self.server_version) {
      (Some(_), Some(version)) => format!("PostgreSQL {version}"),
      (Some(_), None) => "connected".to_string(),
      (None, _) => "connecting...".to_string(),
    };
    h_flex()
      .px_3()
      .py_1()
      .justify_between()
      .bg(cx.theme().secondary)
      .border_t_1()
      .border_color(cx.theme().border)
      .text_xs()
      .text_color(cx.theme().muted_foreground)
      .child(div().child(status))
      .child(div().child(connection))
  }

  fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
    let filter_count = self.table.read(cx).delegate().filters.len();
    h_flex()
      .px_2()
      .py_1()
      .gap_2()
      .border_b_1()
      .border_color(cx.theme().border)
      .child(
        Button::new("run")
          .primary()
          .xsmall()
          .label(if self.running { "Running..." } else { "Run" })
          .disabled(self.running || self.db.is_none())
          .on_click(cx.listener(|this, _, _, cx| this.run(cx))),
      )
      .child(
        Button::new("toggle-filter")
          .ghost()
          .xsmall()
          .label(if filter_count > 0 {
            format!("Filter ({filter_count})")
          } else {
            "Filter".to_string()
          })
          .disabled(self.browsing.is_none())
          .on_click(cx.listener(|this, _, window, cx| this.toggle_filter_row(window, cx))),
      )
  }

  fn render_filter_row(&self, cx: &mut Context<Self>) -> impl IntoElement {
    h_flex()
      .px_2()
      .py_1()
      .gap_2()
      .border_b_1()
      .border_color(cx.theme().border)
      .child(div().w(px(200.)).child(Select::new(&self.filter_column)))
      .child(div().w(px(140.)).child(Select::new(&self.filter_op)))
      .child(div().w(px(220.)).child(Input::new(&self.filter_value)))
      .child(
        Button::new("apply-filter")
          .primary()
          .xsmall()
          .label("Add")
          .on_click(cx.listener(|this, _, _, cx| this.apply_filter(cx))),
      )
  }

  fn render_filter_chips(&self, cx: &mut Context<Self>) -> impl IntoElement {
    let filters = self.table.read(cx).delegate().filters.clone();
    h_flex()
      .px_2()
      .py_1()
      .gap_2()
      .flex_wrap()
      .border_b_1()
      .border_color(cx.theme().border)
      .children(filters.iter().map(|filter| {
        let column = filter.column.clone();
        h_flex()
          .gap_1()
          .px_2()
          .py_0p5()
          .rounded(cx.theme().radius)
          .border_1()
          .border_color(cx.theme().border)
          .text_xs()
          .font_family("IBM Plex Mono")
          .child(filter_label(filter))
          .child(
            Button::new(SharedString::from(format!("rm-{column}")))
              .ghost()
              .xsmall()
              .icon(Icon::new(IconName::Close))
              .on_click(cx.listener(move |this, _, _, cx| {
                this.remove_filter(&column, cx);
              })),
          )
      }))
      .child(
        Button::new("clear-filters")
          .ghost()
          .xsmall()
          .label("clear all")
          .on_click(cx.listener(|this, _, _, cx| this.clear_filters(cx))),
      )
  }
}

impl Render for Workspace {
  fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    v_flex()
      .size_full()
      .bg(cx.theme().background)
      .on_action(cx.listener(|this, _: &RunQuery, _, cx| this.run(cx)))
      .on_action(cx.listener(|this, _: &RefreshSchema, _, cx| this.refresh_schema(cx)))
      .on_action(cx.listener(|_, _: &ToggleThemeMode, window, cx| {
        theme::toggle(window, cx);
      }))
      .on_action(cx.listener(|this, _: &FocusEditor, window, cx| {
        this
          .editor
          .update(cx, |editor, cx| editor.focus(window, cx));
      }))
      .child(
        TitleBar::new().child(h_flex().child("soquel")).child(
          h_flex().flex_1().justify_end().pr_2().child(
            Button::new("toggle-theme")
              .ghost()
              .xsmall()
              .icon(Icon::new(if cx.theme().mode.is_dark() {
                IconName::Sun
              } else {
                IconName::Moon
              }))
              .on_click(cx.listener(|_, _, window, cx| theme::toggle(window, cx))),
          ),
        ),
      )
      .child(
        h_flex().flex_1().min_h_0().child(
          h_resizable("sidebar-main")
            .with_state(&self.splits.0)
            .child(
              resizable_panel()
                .size(px(220.))
                .size_range(px(140.)..px(480.))
                .child(self.render_sidebar(cx).into_any_element()),
            )
            .child(
              resizable_panel().child(
                v_flex()
                  .size_full()
                  .child(self.render_toolbar(cx))
                  .when(self.filter_open, |this| {
                    this.child(self.render_filter_row(cx))
                  })
                  .when(!self.table.read(cx).delegate().filters.is_empty(), |this| {
                    this.child(self.render_filter_chips(cx))
                  })
                  .child(
                    v_flex().flex_1().min_h_0().child(
                      v_resizable("editor-results")
                        .with_state(&self.splits.1)
                        .child(
                          resizable_panel()
                            .size(px(180.))
                            .size_range(px(60.)..px(600.))
                            .child(
                              v_flex()
                                .size_full()
                                .p_1()
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
                  ),
              ),
            ),
        ),
      )
      .child(self.render_status_bar(cx))
  }
}
