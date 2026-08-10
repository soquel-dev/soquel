use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::list::ListItem;
use gpui_component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_component::notification::Notification;
use gpui_component::resizable::{ResizableState, h_resizable, resizable_panel, v_resizable};
use gpui_component::select::{Select, SelectEvent, SelectState};
use gpui_component::table::{DataTable, TableEvent, TableState};
use gpui_component::text::TextView;
use gpui_component::tree::{TreeItem, TreeState, tree};
use gpui_component::{
  ActiveTheme, Disableable, Icon, IconName, IndexPath, Root, Selectable, Sizable, StyledExt,
  TitleBar, WindowExt, h_flex, v_flex,
};
use soquel_core::connectors::{ColumnFilter, FilterOp, SchemaSnapshot, TableChanges, TableKind};
use soquel_core::export::ExportFormat;
use soquel_core::licence::tab_limit_override;

use crate::actions::{
  CancelCellEdit, FocusEditor, NewSqlTab, NextCell, NextTab, PrevCell, PrevTab, RefreshSchema,
  RunQuery, ToggleThemeMode,
};
use crate::completion::{SchemaEntries, SqlCompletionProvider};
use crate::core::{self, Db};
use crate::explain::{
  ExplainPlan, explain_sql, flatten_plan, format_ms, format_rows, hidden_by_collapse, parse_explain,
};
use crate::export::{EXPORT_FORMATS, format_extension, format_label};
use crate::filters::{filter_label, op_label, op_needs_value, ops_for_kind};
use crate::format::format_estimated_rows;
use crate::grid::RowsDelegate;
use crate::history::{HistoryEntry, filter_history, push_history};
use crate::icons::SoquelIcon;
use crate::staged::{build_table_changes, preview_sql};
use crate::tabs::{
  FREE_TABS, TabsState, WorkspaceTab, activate_sibling, close_tab, open_sql_tab, open_table_tab,
};
use crate::theme;

enum TabContent {
  Table {
    table: Entity<TableState<RowsDelegate>>,
    show_ddl: bool,
    ddl: Option<String>,
  },
  Sql {
    editor: Entity<InputState>,
    results: Entity<TableState<RowsDelegate>>,
    split: Entity<ResizableState>,
    session: Option<core::Session>,
    running: bool,
    explain: Option<(Vec<ExplainPlan>, String)>,
    explain_collapsed: HashSet<String>,
  },
}

pub struct Workspace {
  focus_handle: FocusHandle,
  tabs: TabsState,
  contents: HashMap<String, TabContent>,
  cell_editor: Entity<InputState>,
  provider: Rc<SqlCompletionProvider>,
  sidebar_split: Entity<ResizableState>,
  tree: Entity<TreeState>,
  tree_filter: Entity<InputState>,
  db: Option<Db>,
  snapshot: Option<SchemaSnapshot>,
  server_version: Option<String>,
  status: SharedString,
  filter_open: bool,
  filter_column: Entity<SelectState<Vec<String>>>,
  filter_op: Entity<SelectState<Vec<String>>>,
  filter_ops: Vec<FilterOp>,
  filter_value: Entity<InputState>,
  history: Vec<HistoryEntry>,
  history_search: Entity<InputState>,
  schema_entries: SchemaEntries,
  _connect_task: Task<()>,
  _work_task: Task<()>,
}

impl Workspace {
  pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
    let focus_handle = cx.focus_handle();
    window.focus(&focus_handle, cx);
    let (provider, schema_entries) = SqlCompletionProvider::new();
    let cell_editor = cx.new(|cx| InputState::new(window, cx));

    cx.subscribe_in(
      &cell_editor,
      window,
      |this, _, event: &InputEvent, _, cx| {
        if let InputEvent::PressEnter { .. } = event
          && let Some(table) = this.active_table()
        {
          table.update(cx, |table, cx| {
            table.delegate_mut().commit_edit(cx);
          });
        }
      },
    )
    .detach();

    let sidebar_split = cx.new(|_| ResizableState::default());
    let tree = cx.new(|cx| TreeState::new(cx));
    let tree_filter = cx.new(|cx| InputState::new(window, cx).placeholder("filter tables"));
    cx.subscribe_in(
      &tree_filter,
      window,
      |this, _, event: &InputEvent, _, cx| {
        if let InputEvent::Change = event {
          this.rebuild_tree(cx);
        }
      },
    )
    .detach();
    let filter_column = cx.new(|cx| SelectState::new(Vec::<String>::new(), None, window, cx));
    let filter_op = cx.new(|cx| SelectState::new(Vec::<String>::new(), None, window, cx));
    let filter_value = cx.new(|cx| InputState::new(window, cx).placeholder("value"));
    let history_search =
      cx.new(|cx| InputState::new(window, cx).placeholder("search query history..."));

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

    let handle = window.window_handle();
    let connect_task = cx.spawn(async move |this, cx| {
      let db = match core::connect_dev().await {
        Ok(Ok(db)) => db,
        Ok(Err(error)) => {
          let _ = this.update(cx, |this, cx| {
            this.status = format!("error: {error}").into();
            cx.notify();
          });
          return;
        }
        Err(_) => return,
      };

      let _ = this.update(cx, |this, cx| {
        this.db = Some(db.clone());
        this.server_version = db.server_version();
        this.status = "connected".into();
        cx.notify();
      });

      // Schema first: it fills the sidebar and the completion entries.
      if let Ok(Ok(snapshot)) = core::schema_snapshot(&db).await {
        let _ = this.update(cx, |this, cx| {
          this.schema_entries.fill(&snapshot);
          this.snapshot = Some(snapshot);
          this.rebuild_tree(cx);
          cx.notify();
        });
      }

      let target = std::env::var("SOQUEL_GPUI_TABLE").unwrap_or_else(|_| "app.events".into());
      let (schema, table) = target.split_once('.').unwrap_or(("public", &*target));
      let (schema, table) = (schema.to_string(), table.to_string());
      let _ = cx.update_window(handle, |_, window, app| {
        let _ = this.update(app, |this, cx| {
          this.open_table(schema, table, Vec::new(), window, cx);
        });
      });
    });

    Self {
      focus_handle,
      tabs: TabsState::default(),
      contents: HashMap::new(),
      cell_editor,
      provider,
      sidebar_split,
      tree,
      tree_filter,
      db: None,
      snapshot: None,
      server_version: None,
      status: "connecting...".into(),
      filter_open: false,
      filter_column,
      filter_op,
      filter_ops: Vec::new(),
      filter_value,
      history: Vec::new(),
      history_search,
      schema_entries,
      _connect_task: connect_task,
      _work_task: Task::ready(()),
    }
  }

  fn tab_limit(&self) -> usize {
    // A licence lifts the cap; the surface for it is not here yet.
    tab_limit_override().map_or(FREE_TABS, |limit| limit as usize)
  }

  fn active_tab(&self) -> Option<&WorkspaceTab> {
    let id = self.tabs.active_id.as_deref()?;
    self.tabs.tabs.iter().find(|tab| tab.id() == id)
  }

  /// The active tab's grid when it is a table tab; sql results are not it.
  fn active_table(&self) -> Option<Entity<TableState<RowsDelegate>>> {
    let id = self.tabs.active_id.as_deref()?;
    match self.contents.get(id)? {
      TabContent::Table { table, .. } => Some(table.clone()),
      TabContent::Sql { .. } => None,
    }
  }

  fn active_sql(&self) -> Option<(String, Entity<InputState>, Entity<TableState<RowsDelegate>>)> {
    let id = self.tabs.active_id.as_deref()?;
    match self.contents.get(id)? {
      TabContent::Sql {
        editor, results, ..
      } => Some((id.to_string(), editor.clone(), results.clone())),
      TabContent::Table { .. } => None,
    }
  }

  /// The delegate whose status line the status bar shows.
  fn active_grid(&self) -> Option<Entity<TableState<RowsDelegate>>> {
    let id = self.tabs.active_id.as_deref()?;
    match self.contents.get(id)? {
      TabContent::Table { table, .. } => Some(table.clone()),
      TabContent::Sql { results, .. } => Some(results.clone()),
    }
  }

  fn new_grid(
    &self,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) -> Entity<TableState<RowsDelegate>> {
    let cell_editor = self.cell_editor.clone();
    let table = cx.new(|cx| {
      TableState::new(RowsDelegate::new(cell_editor), window, cx)
        .cell_selectable(true)
        .col_resizable(true)
    });
    cx.observe(&table, |_, _, cx| cx.notify()).detach();
    cx.subscribe_in(
      &table,
      window,
      |this, entity, event: &TableEvent, window, cx| {
        if let TableEvent::DoubleClickedCell(row_ix, col_ix) = event {
          let (row_ix, col_ix) = (*row_ix, *col_ix);
          entity.update(cx, |table, cx| {
            table.delegate_mut().start_edit(row_ix, col_ix, window, cx);
          });
          let _ = this;
        }
      },
    )
    .detach();
    table
  }

  fn limit_refused(&mut self, window: &mut Window, cx: &mut Context<Self>) {
    window.push_notification(
      Notification::info(format!(
        "{FREE_TABS} tabs at a time on the free tier. Close one, or unlock the app to open as many as you like.",
      )),
      cx,
    );
  }

  fn open_table(
    &mut self,
    schema: String,
    name: String,
    filters: Vec<ColumnFilter>,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    let Some(db) = self.db.clone() else {
      return;
    };
    let before = self.tabs.tabs.len();
    let Some(next) = open_table_tab(
      &self.tabs,
      &schema,
      &name,
      filters.clone(),
      self.tab_limit(),
    ) else {
      self.limit_refused(window, cx);
      return;
    };
    let opened = next.tabs.len() > before;
    self.tabs = next;

    if opened {
      let id = self.tabs.active_id.clone().expect("just opened");
      let table = self.new_grid(window, cx);

      // Row identity comes from the snapshot: kind gates editing, the pk keys it.
      let info = self.snapshot.as_ref().and_then(|snapshot| {
        snapshot
          .schemas
          .iter()
          .find(|s| s.name == schema)
          .and_then(|s| s.tables.iter().find(|t| t.name == name))
      });
      let can_ever_edit = info.is_some_and(|info| info.kind == TableKind::Table);
      let primary_key = info
        .map(|info| info.primary_key.clone())
        .unwrap_or_default();

      table.update(cx, |table, cx| {
        let delegate = table.delegate_mut();
        delegate.browse = Some((db, schema, name));
        delegate.filters = filters;
        delegate.can_ever_edit = can_ever_edit;
        delegate.key_columns = primary_key;
        delegate.include_xmin = can_ever_edit;
        delegate.reload(cx);
      });
      self.contents.insert(
        id,
        TabContent::Table {
          table,
          show_ddl: false,
          ddl: None,
        },
      );
    } else if let Some(table) = self.active_table() {
      // Re-activated: the model kept the newest initial filters, apply them.
      let filters = match self.active_tab() {
        Some(WorkspaceTab::Table {
          initial_filters, ..
        }) => initial_filters.clone(),
        _ => Vec::new(),
      };
      if !filters.is_empty() {
        table.update(cx, |table, cx| {
          table.delegate_mut().filters = filters;
          table.delegate_mut().reload(cx);
        });
      }
    }
    cx.notify();
  }

  fn open_sql(&mut self, window: &mut Window, cx: &mut Context<Self>) {
    let Some(next) = open_sql_tab(&self.tabs, self.tab_limit()) else {
      self.limit_refused(window, cx);
      return;
    };
    self.tabs = next;
    let id = self.tabs.active_id.clone().expect("just opened");

    let provider = self.provider.clone();
    let editor = cx.new(|cx| {
      let mut state = InputState::new(window, cx)
        .code_editor("sql")
        .line_number(true)
        .searchable(true)
        .placeholder("select * from app.events limit 100  --  ctrl+enter runs");
      state.lsp.completion_provider = Some(provider);
      state
    });
    let results = self.new_grid(window, cx);
    results.update(cx, |table, cx| {
      table.delegate_mut().loading = false;
      table.delegate_mut().eof = true;
      table.delegate_mut().status = "run a query".into();
      cx.notify();
    });
    let split = cx.new(|_| ResizableState::default());
    self.contents.insert(
      id,
      TabContent::Sql {
        editor,
        results,
        split,
        session: None,
        running: false,
        explain: None,
        explain_collapsed: HashSet::new(),
      },
    );
    cx.notify();
  }

  fn close(&mut self, id: &str, cx: &mut Context<Self>) {
    self.tabs = close_tab(&self.tabs, id);
    // Frees the pinned client when its tab closes.
    if let Some(TabContent::Sql {
      session: Some(session),
      ..
    }) = self.contents.remove(id)
    {
      core::close_session(session);
    }
    cx.notify();
  }

  fn activate(&mut self, id: String, cx: &mut Context<Self>) {
    // The close button lives inside the tab: its click also lands here with
    // the id that just closed. Activating a ghost would blank the content.
    if self.tabs.tabs.iter().any(|tab| tab.id() == id) {
      self.tabs.active_id = Some(id);
      cx.notify();
    }
  }

  fn cycle(&mut self, direction: i32, cx: &mut Context<Self>) {
    self.tabs = activate_sibling(&self.tabs, direction);
    cx.notify();
  }

  fn sync_filter_ops(&mut self, column: String, window: &mut Window, cx: &mut Context<Self>) {
    let Some(table) = self.active_table() else {
      return;
    };
    let kind = table
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
    if self.filter_open
      && let Some(table) = self.active_table()
    {
      let names: Vec<String> = table
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
    let Some(table) = self.active_table() else {
      return;
    };
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
    table.update(cx, |table, cx| {
      let delegate = table.delegate_mut();
      delegate.filters.retain(|f| f.column != column);
      delegate.filters.push(filter);
      delegate.reload(cx);
    });
    cx.notify();
  }

  fn remove_filter(&mut self, column: &str, cx: &mut Context<Self>) {
    let Some(table) = self.active_table() else {
      return;
    };
    table.update(cx, |table, cx| {
      let delegate = table.delegate_mut();
      delegate.filters.retain(|f| f.column != column);
      delegate.reload(cx);
    });
    cx.notify();
  }

  fn clear_filters(&mut self, cx: &mut Context<Self>) {
    let Some(table) = self.active_table() else {
      return;
    };
    table.update(cx, |table, cx| {
      let delegate = table.delegate_mut();
      delegate.filters.clear();
      delegate.reload(cx);
    });
    cx.notify();
  }

  /// PK-less table rescue: ctid becomes the row identity, opt-in per table.
  fn enable_ctid(&mut self, cx: &mut Context<Self>) {
    let Some(table) = self.active_table() else {
      return;
    };
    table.update(cx, |table, cx| {
      let delegate = table.delegate_mut();
      delegate.ctid_mode = true;
      delegate.key_columns = vec!["ctid".to_string()];
      delegate.reload(cx);
    });
  }

  fn apply_staged(&mut self, window: &mut Window, cx: &mut Context<Self>) {
    let Some(table) = self.active_table() else {
      return;
    };
    let (changes, db) = {
      let table = table.read(cx);
      let delegate = table.delegate();
      let Some((db, schema, name)) = delegate.browse.clone() else {
        return;
      };
      if delegate.staged.is_empty() {
        return;
      }
      (
        build_table_changes(
          &delegate.staged,
          &delegate.rows,
          &delegate.columns,
          &delegate.change_keys(),
          &schema,
          &name,
        ),
        db,
      )
    };

    let statements = preview_sql(&changes);
    let this = cx.entity();
    window.open_dialog(cx, move |dialog, _, cx| {
      let changes = changes.clone();
      let db = db.clone();
      let this = this.clone();
      dialog
        .title(format!("Apply {} change(s)", statements.len()))
        .child(
          div()
            .max_h(px(360.))
            .p_2()
            .rounded(cx.theme().radius)
            .bg(cx.theme().muted)
            .text_sm()
            .child(TextView::markdown(
              "apply-preview",
              format!("```sql\n{}\n```", statements.join("\n")),
            )),
        )
        .footer(
          h_flex()
            .gap_2()
            .justify_end()
            .child(
              Button::new("cancel-apply")
                .label("Cancel")
                .on_click(|_, window, cx| window.close_dialog(cx)),
            )
            .child(
              Button::new("confirm-apply")
                .primary()
                .label("Apply")
                .on_click(move |_, window, cx| {
                  window.close_dialog(cx);
                  let changes = changes.clone();
                  let db = db.clone();
                  this.update(cx, |this, cx| this.run_apply(db, changes, cx));
                }),
            ),
        )
    });
  }

  fn run_apply(&mut self, db: Db, changes: TableChanges, cx: &mut Context<Self>) {
    let Some(table) = self.active_table() else {
      return;
    };
    let rx = core::apply_changes(&db, changes);
    self._work_task = cx.spawn(async move |this, cx| {
      let result = rx.await;
      match result {
        Ok(Ok(applied)) => {
          let _ = this.update(cx, |this, cx| {
            this.status = format!(
              "applied: {} updated, {} inserted, {} deleted",
              applied.updated, applied.inserted, applied.deleted
            )
            .into();
            cx.notify();
          });
          table.update(cx, |table, cx| {
            table.delegate_mut().reload(cx);
          });
        }
        Ok(Err(error)) => {
          // Staging is kept: the transaction rolled back server-side.
          table.update(cx, |table, cx| {
            table.delegate_mut().status = format!("error: {error}").into();
            cx.notify();
          });
        }
        Err(_) => {}
      }
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
          this.rebuild_tree(cx);
          cx.notify();
        });
      }
    });
  }

  fn run(&mut self, cx: &mut Context<Self>) {
    self.run_inner(None, cx);
  }

  /// EXPLAIN wraps a single statement: use the selection to explain one of many.
  fn run_explain(&mut self, analyze: bool, cx: &mut Context<Self>) {
    self.run_inner(Some(analyze), cx);
  }

  fn run_inner(&mut self, explain: Option<bool>, cx: &mut Context<Self>) {
    let Some(db) = self.db.clone() else {
      return;
    };
    let Some((id, editor, results)) = self.active_sql() else {
      return;
    };
    if let Some(TabContent::Sql { running: true, .. }) = self.contents.get(&id) {
      return;
    }
    // The selection runs alone when there is one; otherwise the whole editor.
    let sql = {
      let state = editor.read(cx);
      let value = state.value().to_string();
      let range = state.selected_range();
      let selected = value.get(range).unwrap_or("").trim().to_string();
      if selected.is_empty() {
        value.trim().to_string()
      } else {
        selected
      }
    };
    if sql.is_empty() {
      return;
    }
    let sql = match explain {
      Some(analyze) => explain_sql(
        soquel_core::profiles::ConnectorKind::Postgres,
        analyze,
        sql.trim_end_matches(';').trim_end(),
      ),
      None => sql,
    };

    if let Some(TabContent::Sql { running, .. }) = self.contents.get_mut(&id) {
      *running = true;
    }
    results.update(cx, |table, cx| {
      table.delegate_mut().status = "running...".into();
      cx.notify();
    });

    let session = match self.contents.get(&id) {
      Some(TabContent::Sql { session, .. }) => session.clone(),
      _ => None,
    };
    self._work_task = cx.spawn(async move |this, cx| {
      // One pinned session per editor tab: SET and transactions stick to it.
      let session = match session {
        Some(session) => session,
        None => match core::open_session(&db).await {
          Ok(Ok(session)) => {
            let _ = this.update(cx, |this, _| {
              if let Some(TabContent::Sql { session: slot, .. }) = this.contents.get_mut(&id) {
                *slot = Some(session.clone());
              }
            });
            session
          }
          _ => {
            let _ = this.update(cx, |this, cx| {
              if let Some(TabContent::Sql { running, .. }) = this.contents.get_mut(&id) {
                *running = false;
              }
              cx.notify();
            });
            results.update(cx, |table, cx| {
              table.delegate_mut().status = "error: could not open a session".into();
              cx.notify();
            });
            return;
          }
        },
      };
      let result = core::run_session_query(&session, sql.clone()).await;
      let at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default();
      let (ok, duration_ms) = match &result {
        Ok(Ok(result)) => (true, result.duration_ms),
        _ => (false, 0.),
      };
      let _ = this.update(cx, |this, cx| {
        if let Some(TabContent::Sql {
          running,
          explain,
          explain_collapsed,
          ..
        }) = this.contents.get_mut(&id)
        {
          *running = false;
          *explain = None;
          explain_collapsed.clear();
          if let Ok(Ok(result)) = &result
            && let Some(statement) = result
              .statements
              .iter()
              .rev()
              .find(|s| !s.columns.is_empty())
            && let Some(plans) = parse_explain(&statement.columns, &statement.rows)
          {
            let raw = statement
              .rows
              .iter()
              .map(|row| row.first().and_then(|c| c.as_deref()).unwrap_or(""))
              .collect::<Vec<_>>()
              .join("\n");
            *explain = Some((plans, raw));
          }
        }
        this.history = push_history(
          std::mem::take(&mut this.history),
          HistoryEntry {
            sql,
            at_ms,
            duration_ms,
            ok,
          },
        );
        cx.notify();
      });
      results.update(cx, |table, cx| {
        {
          let delegate = table.delegate_mut();
          // Query results never page.
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

  /// Ids carry the row's facts (schema, table, kind, estimate) so the render
  /// closure can draw without reaching back into the snapshot.
  fn rebuild_tree(&mut self, cx: &mut Context<Self>) {
    let needle = self.tree_filter.read(cx).value().trim().to_lowercase();
    let Some(snapshot) = &self.snapshot else {
      return;
    };
    let mut items = Vec::new();
    for schema in &snapshot.schemas {
      let tables: Vec<TreeItem> = schema
        .tables
        .iter()
        .filter(|table| needle.is_empty() || table.name.to_lowercase().contains(&needle))
        .map(|table| {
          let marker = match table.kind {
            TableKind::Table => "T",
            TableKind::View => "V",
            TableKind::MaterializedView => "M",
          };
          let id = format!(
            "{}\t{}\t{}\t{}",
            schema.name,
            table.name,
            marker,
            format_estimated_rows(table.estimated_rows)
          );
          TreeItem::new(id, table.name.clone())
        })
        .collect();
      if tables.is_empty() {
        continue;
      }
      items.push(
        TreeItem::new(format!("schema:{}", schema.name), schema.name.clone())
          .expanded(true)
          .children(tables),
      );
    }
    self.tree.update(cx, |state, cx| {
      state.set_items(items, cx);
    });
    cx.notify();
  }

  fn toggle_ddl(&mut self, show: bool, cx: &mut Context<Self>) {
    let Some(id) = self.tabs.active_id.clone() else {
      return;
    };
    let Some(TabContent::Table {
      show_ddl,
      ddl,
      table,
    }) = self.contents.get_mut(&id)
    else {
      return;
    };
    *show_ddl = show;
    let needs_fetch = show && ddl.is_none();
    let browse = table.read(cx).delegate().browse.clone();
    cx.notify();
    if !needs_fetch {
      return;
    }
    let Some((db, schema, name)) = browse else {
      return;
    };
    let rx = core::table_ddl(&db, schema, name);
    self._work_task = cx.spawn(async move |this, cx| {
      let result = rx.await;
      let _ = this.update(cx, |this, cx| {
        if let Some(TabContent::Table { ddl, .. }) = this.contents.get_mut(&id) {
          *ddl = Some(match result {
            Ok(Ok(sql)) => sql,
            Ok(Err(error)) => format!("-- error: {error}"),
            Err(_) => "-- error: fetch canceled".to_string(),
          });
        }
        cx.notify();
      });
    });
  }

  fn cancel_query(&mut self, cx: &mut Context<Self>) {
    let Some((id, _, results)) = self.active_sql() else {
      return;
    };
    if let Some(TabContent::Sql {
      session: Some(session),
      running: true,
      ..
    }) = self.contents.get(&id)
    {
      core::cancel_session(session);
      results.update(cx, |table, cx| {
        table.delegate_mut().status = "canceling...".into();
        cx.notify();
      });
    }
  }

  fn open_history(&mut self, window: &mut Window, cx: &mut Context<Self>) {
    self.history_search.update(cx, |state, cx| {
      state.set_value("", window, cx);
    });
    let this = cx.entity();
    let search = self.history_search.clone();
    window.open_dialog(cx, move |dialog, _, cx| {
      let this_entity = this.clone();
      let entries = {
        let workspace = this_entity.read(cx);
        let query = search.read(cx).value().to_string();
        filter_history(&workspace.history, &query)
      };
      dialog.title("Query history").child(
        v_flex().gap_2().child(Input::new(&search)).child(
          v_flex()
            .id("history-entries")
            .max_h(px(360.))
            .min_h(px(120.))
            .overflow_y_scroll()
            .gap_px()
            .children(entries.into_iter().enumerate().map(|(ix, entry)| {
              let this = this_entity.clone();
              let sql = entry.sql.clone();
              let label = if entry.ok {
                format!("{:.0} ms", entry.duration_ms)
              } else {
                "failed".to_string()
              };
              h_flex()
                .id(SharedString::from(format!("h-{ix}")))
                .px_2()
                .py_1()
                .gap_2()
                .rounded(cx.theme().radius)
                .cursor_default()
                .hover(|s| s.bg(cx.theme().accent))
                .on_click(move |_, window, app| {
                  let sql = sql.clone();
                  window.close_dialog(app);
                  this.update(app, |this, cx| this.load_history_entry(sql, window, cx));
                })
                .child(
                  div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_sm()
                    .font_family("IBM Plex Mono")
                    .child(entry.sql.clone()),
                )
                .child(
                  div()
                    .text_xs()
                    .text_color(if entry.ok {
                      cx.theme().muted_foreground
                    } else {
                      cx.theme().danger
                    })
                    .child(label),
                )
            })),
        ),
      )
    });
  }

  fn load_history_entry(&mut self, sql: String, window: &mut Window, cx: &mut Context<Self>) {
    if let Some((_, editor, _)) = self.active_sql() {
      editor.update(cx, |editor, cx| {
        editor.set_value(sql, window, cx);
        editor.focus(window, cx);
      });
    }
  }

  fn toggle_explain_node(&mut self, id: &str, cx: &mut Context<Self>) {
    let Some(tab_id) = self.tabs.active_id.clone() else {
      return;
    };
    if let Some(TabContent::Sql {
      explain_collapsed, ..
    }) = self.contents.get_mut(&tab_id)
    {
      if !explain_collapsed.remove(id) {
        explain_collapsed.insert(id.to_string());
      }
      cx.notify();
    }
  }

  fn render_explain(
    &self,
    plans: &[ExplainPlan],
    collapsed: &HashSet<String>,
    cx: &mut Context<Self>,
  ) -> impl IntoElement {
    let mut header: Vec<String> = Vec::new();
    for plan in plans {
      if let Some(ms) = plan.planning_ms {
        header.push(format!("planning {}", format_ms(ms)));
      }
      if let Some(ms) = plan.execution_ms {
        header.push(format!("execution {}", format_ms(ms)));
      }
      if !plan.analyzed {
        header.push(
          if plan.root.total_cost > 0. {
            "estimated costs only"
          } else {
            "query structure only"
          }
          .to_string(),
        );
      }
    }

    let mut rows = Vec::new();
    for plan in plans {
      for node in flatten_plan(&plan.root) {
        if hidden_by_collapse(&node.id, collapsed) {
          continue;
        }
        let heat_color = if node.heat >= 0.4 {
          cx.theme().red
        } else if node.heat >= 0.1 {
          cx.theme().yellow
        } else {
          cx.theme().muted_foreground
        };
        let timing = if !plan.analyzed {
          if node.total_cost > 0. {
            format!(
              "cost {:.*}",
              if node.total_cost >= 100. { 0 } else { 2 },
              node.total_cost
            )
          } else {
            String::new()
          }
        } else {
          node.inclusive_ms.map(format_ms).unwrap_or_default()
        };
        let rows_label = if node.plan_rows > 0. || node.actual_rows.is_some() {
          let mut label = format!("rows {}", format_rows(node.plan_rows));
          if let Some(actual) = node.actual_rows {
            label.push_str(&format!(" -> {}", format_rows(actual)));
          }
          Some(label)
        } else {
          None
        };
        let node_id = node.id.clone();
        let is_collapsed = collapsed.contains(&node.id);
        rows.push(
          h_flex()
            .px_3()
            .py_0p5()
            .gap_2()
            .items_center()
            .font_family("IBM Plex Mono")
            .text_xs()
            .pl(px(12. + node.depth as f32 * 16.))
            .child(if node.children.is_empty() {
              div().w_4().into_any_element()
            } else {
              Button::new(SharedString::from(format!("ex-{node_id}")))
                .ghost()
                .xsmall()
                .icon(Icon::new(if is_collapsed {
                  IconName::ChevronRight
                } else {
                  IconName::ChevronDown
                }))
                .on_click(cx.listener(move |this, _, _, cx| {
                  this.toggle_explain_node(&node_id, cx);
                }))
                .into_any_element()
            })
            .child(div().font_semibold().child(node.node_type.clone()))
            .when_some(node.target.clone(), |this, target| {
              this.child(div().text_color(cx.theme().muted_foreground).child(target))
            })
            .when_some(node.condition.clone(), |this, condition| {
              this.child(
                div()
                  .flex_1()
                  .min_w_0()
                  .truncate()
                  .text_color(cx.theme().muted_foreground)
                  .child(condition),
              )
            })
            .child(div().flex_1())
            .when(node.never_executed, |this| {
              this.child(
                div()
                  .text_color(cx.theme().muted_foreground)
                  .italic()
                  .child("never executed"),
              )
            })
            .when(!node.never_executed, |this| {
              this
                .when_some(rows_label.clone(), |this, label| {
                  this.child(
                    div()
                      .text_color(if node.estimate_off {
                        cx.theme().yellow
                      } else {
                        cx.theme().muted_foreground
                      })
                      .child(label),
                  )
                })
                .child(div().text_color(heat_color).child(timing.clone()))
            })
            .child(
              // Exclusive share of total: the folded-flamegraph signature.
              div()
                .w_10()
                .h_1()
                .rounded_full()
                .bg(cx.theme().muted)
                .child(div().h_full().rounded_full().bg(heat_color).w(relative(
                  (node.heat as f32).max(if node.heat > 0. { 0.04 } else { 0. }),
                ))),
            ),
        );
      }
    }

    v_flex()
      .size_full()
      .child(
        h_flex()
          .px_3()
          .py_1()
          .gap_3()
          .border_b_1()
          .border_color(cx.theme().border)
          .font_family("IBM Plex Mono")
          .text_xs()
          .text_color(cx.theme().muted_foreground)
          .children(header.into_iter().map(|part| div().child(part))),
      )
      .child(
        v_flex()
          .id("explain-rows")
          .flex_1()
          .min_h_0()
          .overflow_y_scroll()
          .py_1()
          .children(rows),
      )
  }

  /// The grid whose rows exports read: the table tab's, or the sql results.
  fn export_source(&self) -> Option<(Entity<TableState<RowsDelegate>>, String, String)> {
    let grid = self.active_grid()?;
    let (schema, name) = match self.active_tab()? {
      WorkspaceTab::Table { schema, table, .. } => (schema.clone(), table.clone()),
      WorkspaceTab::Sql { .. } => (String::new(), "results".to_string()),
    };
    Some((grid, schema, name))
  }

  fn export_copy(&mut self, format: ExportFormat, cx: &mut Context<Self>) {
    let Some((grid, _, name)) = self.export_source() else {
      return;
    };
    let kind = self
      .db
      .as_ref()
      .map(|db| db.kind())
      .unwrap_or(soquel_core::profiles::ConnectorKind::Postgres);
    let (columns, rows) = {
      let table = grid.read(cx);
      let delegate = table.delegate();
      let hidden = delegate.hidden_lead();
      let columns = delegate.display_columns().to_vec();
      let rows: Vec<Vec<Option<String>>> = delegate
        .rows
        .iter()
        .map(|row| row[hidden..].to_vec())
        .collect();
      (columns, rows)
    };
    if columns.is_empty() {
      return;
    }
    let count = rows.len();
    match soquel_core::export::format_statement(columns, &rows, format, kind, &name) {
      Ok(text) => {
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.status = format!("copied {count} rows as {}", format_label(format)).into();
      }
      Err(error) => {
        self.status = format!("error: {error}").into();
      }
    }
    cx.notify();
  }

  fn export_save(&mut self, format: ExportFormat, cx: &mut Context<Self>) {
    let Some((grid, schema, name)) = self.export_source() else {
      return;
    };
    let Some(db) = self.db.clone() else {
      return;
    };
    let base = if schema.is_empty() {
      name.clone()
    } else {
      format!("{schema}.{name}")
    };
    let suggested = format!("{base}.{}", format_extension(format));
    let home = std::env::var_os("HOME")
      .or_else(|| std::env::var_os("USERPROFILE"))
      .map(std::path::PathBuf::from)
      .unwrap_or_default();
    let picked = cx.prompt_for_new_path(&home, Some(&suggested));

    self._work_task = cx.spawn(async move |this, cx| {
      let Ok(Ok(Some(path))) = picked.await else {
        return;
      };
      let path = path.to_string_lossy().to_string();

      // A browsed table streams in full (current sort and filters); a result
      // set writes what the grid holds.
      let (browse, sort, filters, columns, rows, kind) = {
        grid.read_with(cx, |table, _| {
          let delegate = table.delegate();
          let hidden = delegate.hidden_lead();
          (
            delegate.browse.clone(),
            delegate.sort.clone(),
            delegate.filters.clone(),
            delegate.display_columns().to_vec(),
            delegate
              .rows
              .iter()
              .map(|row| row[hidden..].to_vec())
              .collect::<Vec<_>>(),
            db.kind(),
          )
        })
      };

      match browse {
        Some((db, schema, table_name)) => {
          let mut request = core::page_request(&schema, &table_name, 0, 0);
          request.limit = None;
          request.sort = sort;
          request.filters = filters;
          let (mut progress, done) = core::export_rows(&db, request, format, path);
          let _ = this.update(cx, |this, cx| {
            this.status = "exporting...".into();
            cx.notify();
          });
          use futures::{FutureExt, StreamExt};
          let mut done = done.fuse();
          loop {
            futures::select! {
              rows = progress.next() => {
                if let Some(rows) = rows {
                  let _ = this.update(cx, |this, cx| {
                    this.status = format!("exporting... {rows} rows").into();
                    cx.notify();
                  });
                }
              }
              result = done => {
                let _ = this.update(cx, |this, cx| {
                  this.status = match result {
                    Ok(Ok(summary)) => format!(
                      "exported {:.0} rows - {:.0} ms",
                      summary.rows, summary.duration_ms
                    )
                    .into(),
                    Ok(Err(error)) => format!("error: {error}").into(),
                    Err(_) => "error: export canceled".into(),
                  };
                  cx.notify();
                });
                return;
              }
            }
          }
        }
        None => {
          let count = rows.len();
          let result = cx
            .background_spawn(async move {
              soquel_core::export::export_statement(columns, &rows, format, kind, &name, &path)
            })
            .await;
          let _ = this.update(cx, |this, cx| {
            this.status = match result {
              Ok(()) => format!("exported {count} rows").into(),
              Err(error) => format!("error: {error}").into(),
            };
            cx.notify();
          });
        }
      }
    });
  }

  fn render_export_menu(&self, id: &str, cx: &mut Context<Self>) -> impl IntoElement {
    let workspace = cx.entity();
    Button::new(SharedString::from(format!("export-{id}")))
      .ghost()
      .xsmall()
      .label("Export")
      .dropdown_menu(move |mut menu, window, _| {
        menu = menu.label("Copy as");
        for format in EXPORT_FORMATS {
          menu = menu.item(
            PopupMenuItem::new(format_label(format)).on_click(window.listener_for(
              &workspace,
              move |this, _: &ClickEvent, _, cx| {
                this.export_copy(format, cx);
              },
            )),
          );
        }
        menu = menu.separator().label("Save as");
        for format in EXPORT_FORMATS {
          menu = menu.item(
            PopupMenuItem::new(format!("{}...", format_label(format))).on_click(
              window.listener_for(&workspace, move |this, _: &ClickEvent, _, cx| {
                this.export_save(format, cx);
              }),
            ),
          );
        }
        menu
      })
  }

  fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
    let workspace = cx.entity();
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
        div()
          .px_2()
          .pb_1()
          .child(Input::new(&self.tree_filter).xsmall()),
      )
      .child(
        v_flex().flex_1().min_h_0().px_1().child(
          tree(&self.tree, move |ix, entry, selected, _, cx| {
            let item = entry.item();
            if entry.is_folder() {
              return ListItem::new(ix).child(
                h_flex()
                  .gap_1()
                  .pl(px(4.))
                  .items_center()
                  .text_xs()
                  .text_color(cx.theme().muted_foreground)
                  .child(
                    Icon::new(if entry.is_expanded() {
                      IconName::ChevronDown
                    } else {
                      IconName::ChevronRight
                    })
                    .size_3(),
                  )
                  .child(item.label.clone()),
              );
            }
            let mut parts = item.id.split('\t');
            let schema = parts.next().unwrap_or_default().to_string();
            let name = parts.next().unwrap_or_default().to_string();
            let marker = parts.next().unwrap_or_default();
            let estimate = parts.next().unwrap_or_default().to_string();
            let icon = match marker {
              "V" => Icon::new(IconName::Eye),
              "M" => Icon::new(SoquelIcon::Layers),
              _ => Icon::new(SoquelIcon::Table2),
            };
            let workspace = workspace.clone();
            ListItem::new(ix)
              .selected(selected)
              .on_click(move |_, window, app| {
                let (schema, name) = (schema.clone(), name.clone());
                workspace.update(app, |this, cx| {
                  this.open_table(schema, name, Vec::new(), window, cx);
                });
              })
              .child(
                h_flex()
                  .pl(px(20.))
                  .pr_2()
                  .gap_2()
                  .items_center()
                  .text_sm()
                  .child(
                    div()
                      .text_color(cx.theme().muted_foreground)
                      .child(icon.size_3()),
                  )
                  .child(
                    div()
                      .flex_1()
                      .min_w_0()
                      .truncate()
                      .child(item.label.clone()),
                  )
                  .child(
                    div()
                      .text_xs()
                      .text_color(cx.theme().muted_foreground)
                      .child(estimate),
                  ),
              )
          })
          .size_full(),
        ),
      )
  }

  fn render_tab_strip(&self, cx: &mut Context<Self>) -> impl IntoElement {
    let active = self.tabs.active_id.clone();
    h_flex()
      .px_2()
      .pt_1()
      .gap_1()
      .border_b_1()
      .border_color(cx.theme().border)
      .children(self.tabs.tabs.iter().map(|tab| {
        let id = tab.id().to_string();
        let close_id = id.clone();
        let selected = active.as_deref() == Some(tab.id());
        h_flex()
          .id(SharedString::from(format!("tab-{id}")))
          .px_2()
          .py_1()
          .gap_1()
          .items_center()
          .rounded_t(cx.theme().radius)
          .text_sm()
          .cursor_default()
          .when(selected, |this| {
            this
              .bg(cx.theme().accent)
              .text_color(cx.theme().accent_foreground)
          })
          .when(!selected, |this| {
            this.text_color(cx.theme().muted_foreground)
          })
          .hover(|this| this.bg(cx.theme().accent))
          .on_click(cx.listener(move |this, _, _, cx| this.activate(id.clone(), cx)))
          .child(tab.title())
          .child(
            Button::new(SharedString::from(format!("close-{close_id}")))
              .ghost()
              .xsmall()
              .icon(Icon::new(IconName::Close))
              .on_click(cx.listener(move |this, _, _, cx| {
                this.close(&close_id.clone(), cx);
              })),
          )
      }))
      .child(
        Button::new("new-sql-tab")
          .ghost()
          .xsmall()
          .label("+ SQL")
          .disabled(self.db.is_none())
          .on_click(cx.listener(|this, _, window, cx| this.open_sql(window, cx))),
      )
  }

  fn render_table_toolbar(
    &self,
    table: &Entity<TableState<RowsDelegate>>,
    show_ddl: bool,
    ddl: Option<&String>,
    cx: &mut Context<Self>,
  ) -> impl IntoElement {
    let filter_count = table.read(cx).delegate().filters.len();
    let ddl_text = ddl.cloned();
    let editable = table.read(cx).delegate().editable();
    let can_ever_edit = table.read(cx).delegate().can_ever_edit;
    let selected_row = table.read(cx).selected_cell().map(|(row, _)| row);
    let pending = table.read(cx).delegate().staged.count();
    let grid = table.clone();
    let grid_delete = table.clone();
    let grid_discard = table.clone();

    h_flex()
      .px_2()
      .py_1()
      .gap_2()
      .border_b_1()
      .border_color(cx.theme().border)
      .child(
        Button::new("view-data")
          .ghost()
          .xsmall()
          .label("Data")
          .selected(!show_ddl)
          .on_click(cx.listener(|this, _, _, cx| this.toggle_ddl(false, cx))),
      )
      .child(
        Button::new("view-ddl")
          .ghost()
          .xsmall()
          .label("DDL")
          .selected(show_ddl)
          .on_click(cx.listener(|this, _, _, cx| this.toggle_ddl(true, cx))),
      )
      .when(!show_ddl, |this| {
        this.child(self.render_export_menu("table", cx))
      })
      .when(show_ddl, |this| {
        this.child(
          Button::new("copy-ddl")
            .ghost()
            .xsmall()
            .label("Copy")
            .disabled(ddl_text.is_none())
            .on_click(move |_, _, cx| {
              if let Some(sql) = &ddl_text {
                cx.write_to_clipboard(ClipboardItem::new_string(sql.clone()));
              }
            }),
        )
      })
      .when(!show_ddl, |this| {
        this.child(
          Button::new("toggle-filter")
            .ghost()
            .xsmall()
            .label(if filter_count > 0 {
              format!("Filter ({filter_count})")
            } else {
              "Filter".to_string()
            })
            .on_click(cx.listener(|this, _, window, cx| this.toggle_filter_row(window, cx))),
        )
      })
      .when(!show_ddl && editable, |this| {
        this
          .child(
            Button::new("add-row")
              .ghost()
              .xsmall()
              .label("+ Row")
              .on_click(cx.listener(move |_, _, _, cx| {
                grid.update(cx, |table, cx| {
                  table.delegate_mut().add_insert(None, cx);
                  // Inserts sit at the top: nothing shifts under them while paging.
                  table.scroll_to_row(0, cx);
                });
              })),
          )
          .child(
            Button::new("delete-row")
              .ghost()
              .xsmall()
              .label("Delete")
              .disabled(selected_row.is_none())
              .on_click(cx.listener(move |_, _, _, cx| {
                if let Some(row) = selected_row {
                  grid_delete.update(cx, |table, cx| {
                    table.delegate_mut().toggle_delete(row, cx);
                  });
                }
              })),
          )
          .when(pending > 0, |this| {
            this
              .child(
                Button::new("apply")
                  .primary()
                  .xsmall()
                  .label(format!("Apply ({pending})"))
                  .on_click(cx.listener(|this, _, window, cx| this.apply_staged(window, cx))),
              )
              .child(
                Button::new("discard")
                  .ghost()
                  .xsmall()
                  .label("Discard")
                  .on_click(cx.listener(move |_, _, _, cx| {
                    grid_discard.update(cx, |table, cx| {
                      table.delegate_mut().discard(cx);
                    });
                  })),
              )
          })
      })
      .when(!show_ddl && can_ever_edit && !editable, |this| {
        this
          .child(
            div()
              .text_xs()
              .text_color(cx.theme().muted_foreground)
              .child("no primary key - editing disabled"),
          )
          .child(
            Button::new("enable-ctid")
              .ghost()
              .xsmall()
              .label("edit via ctid")
              .on_click(cx.listener(|this, _, _, cx| this.enable_ctid(cx))),
          )
      })
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

  fn render_filter_chips(
    &self,
    table: &Entity<TableState<RowsDelegate>>,
    cx: &mut Context<Self>,
  ) -> impl IntoElement {
    let filters = table.read(cx).delegate().filters.clone();
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

  fn render_active_content(&self, cx: &mut Context<Self>) -> AnyElement {
    let Some(id) = self.tabs.active_id.clone() else {
      return v_flex()
        .flex_1()
        .items_center()
        .justify_center()
        .text_sm()
        .text_color(cx.theme().muted_foreground)
        .child("open a table from the sidebar, or a sql tab")
        .into_any_element();
    };
    match self.contents.get(&id) {
      Some(TabContent::Table {
        table,
        show_ddl,
        ddl,
      }) => {
        let table = table.clone();
        let show_ddl = *show_ddl;
        let ddl = ddl.clone();
        v_flex()
          .flex_1()
          .min_h_0()
          .child(self.render_table_toolbar(&table, show_ddl, ddl.as_ref(), cx))
          .when(!show_ddl, |this| {
            this
              .when(self.filter_open, |this| {
                this.child(self.render_filter_row(cx))
              })
              .when(!table.read(cx).delegate().filters.is_empty(), |this| {
                this.child(self.render_filter_chips(&table, cx))
              })
              .child(
                v_flex()
                  .flex_1()
                  .min_h_0()
                  .p_1()
                  .child(DataTable::new(&table).stripe(true)),
              )
          })
          .when(show_ddl, |this| {
            this.child(
              v_flex()
                .id("ddl-view")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .p_3()
                .text_sm()
                .child(TextView::markdown(
                  "ddl",
                  format!("```sql\n{}\n```", ddl.as_deref().unwrap_or("-- loading...")),
                )),
            )
          })
          .into_any_element()
      }
      Some(TabContent::Sql {
        editor,
        results,
        split,
        running,
        explain,
        explain_collapsed,
        ..
      }) => v_flex()
        .flex_1()
        .min_h_0()
        .child(
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
                .label(if *running { "Running..." } else { "Run" })
                .disabled(*running || self.db.is_none())
                .on_click(cx.listener(|this, _, _, cx| this.run(cx))),
            )
            .when(*running, |this| {
              this.child(
                Button::new("cancel-query")
                  .ghost()
                  .xsmall()
                  .label("Cancel")
                  .on_click(cx.listener(|this, _, _, cx| this.cancel_query(cx))),
              )
            })
            .child(
              Button::new("explain")
                .ghost()
                .xsmall()
                .label("Explain")
                .disabled(*running || self.db.is_none())
                .on_click(cx.listener(|this, _, _, cx| this.run_explain(false, cx))),
            )
            .child(
              Button::new("explain-analyze")
                .ghost()
                .xsmall()
                .label("Analyze")
                .disabled(*running || self.db.is_none())
                .on_click(cx.listener(|this, _, _, cx| this.run_explain(true, cx))),
            )
            .child(
              Button::new("history")
                .ghost()
                .xsmall()
                .label("History")
                .disabled(self.history.is_empty())
                .on_click(cx.listener(|this, _, window, cx| this.open_history(window, cx))),
            )
            .child(self.render_export_menu("sql", cx)),
        )
        .child(
          v_flex().flex_1().min_h_0().child(
            v_resizable(SharedString::from(format!("sql-split-{id}")))
              .with_state(split)
              .child(
                resizable_panel()
                  .size(px(180.))
                  .size_range(px(60.)..px(600.))
                  .child(
                    v_flex()
                      .size_full()
                      .p_1()
                      .child(Input::new(editor).h_full()),
                  ),
              )
              .child(
                resizable_panel().child(match explain {
                  Some((plans, _)) => {
                    let plans = plans.clone();
                    let collapsed = explain_collapsed.clone();
                    v_flex()
                      .size_full()
                      .child(self.render_explain(&plans, &collapsed, cx))
                      .into_any_element()
                  }
                  None => v_flex()
                    .size_full()
                    .p_1()
                    .child(DataTable::new(results).stripe(true))
                    .into_any_element(),
                }),
              ),
          ),
        )
        .into_any_element(),
      None => div().into_any_element(),
    }
  }

  fn render_status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
    let status = self
      .active_grid()
      .map(|grid| grid.read(cx).delegate().status.clone())
      .unwrap_or_else(|| self.status.clone());
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
}

impl Render for Workspace {
  fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    // Root does not render these itself: without them dialogs and toasts are silent no-ops.
    let dialog_layer = Root::render_dialog_layer(window, cx);
    let notification_layer = Root::render_notification_layer(window, cx);

    v_flex()
      .size_full()
      .track_focus(&self.focus_handle)
      .bg(cx.theme().background)
      .on_action(cx.listener(|this, _: &RunQuery, _, cx| this.run(cx)))
      .on_action(cx.listener(|this, _: &RefreshSchema, _, cx| this.refresh_schema(cx)))
      .on_action(cx.listener(|this, _: &NextTab, _, cx| this.cycle(1, cx)))
      .on_action(cx.listener(|this, _: &PrevTab, _, cx| this.cycle(-1, cx)))
      .on_action(cx.listener(|this, _: &NewSqlTab, window, cx| this.open_sql(window, cx)))
      .on_action(cx.listener(|_, _: &ToggleThemeMode, window, cx| {
        theme::toggle(window, cx);
      }))
      .on_action(cx.listener(|this, _: &FocusEditor, window, cx| {
        if let Some((_, editor, _)) = this.active_sql() {
          editor.update(cx, |editor, cx| editor.focus(window, cx));
        }
      }))
      .on_action(cx.listener(|this, _: &CancelCellEdit, _, cx| {
        if let Some(table) = this.active_table() {
          table.update(cx, |table, cx| {
            table.delegate_mut().cancel_edit(cx);
          });
        }
      }))
      .on_action(cx.listener(|this, _: &NextCell, window, cx| {
        if let Some(table) = this.active_table() {
          table.update(cx, |table, cx| {
            table.delegate_mut().edit_neighbor(1, window, cx);
          });
        }
      }))
      .on_action(cx.listener(|this, _: &PrevCell, window, cx| {
        if let Some(table) = this.active_table() {
          table.update(cx, |table, cx| {
            table.delegate_mut().edit_neighbor(-1, window, cx);
          });
        }
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
            .with_state(&self.sidebar_split)
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
                  .child(self.render_tab_strip(cx))
                  .child(self.render_active_content(cx)),
              ),
            ),
        ),
      )
      .child(self.render_status_bar(cx))
      .children(dialog_layer)
      .children(notification_layer)
  }
}
