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
  ActiveTheme, Disableable, Icon, IconName, IndexPath, Selectable, Sizable, StyledExt, WindowExt,
  h_flex, v_flex,
};
use soquel_core::connectors::{ColumnFilter, FilterOp, SchemaSnapshot, TableChanges, TableKind};
use soquel_core::export::ExportFormat;
use soquel_core::licence::tab_limit_override;
use soquel_core::profiles::ConnectionProfile;

use crate::actions::{
  CancelCellEdit, FocusEditor, NewSqlTab, NextCell, NextTab, PrevCell, PrevTab, RefreshSchema,
  RunQuery, ToggleThemeMode,
};
use crate::completion::{SchemaEntries, SqlCompletionProvider};
use crate::core::{self, Db};
use crate::explain::{
  ExplainPlan, explain_sql, format_ms, format_rows, parse_explain, visible_plan_nodes,
};
use crate::export::{EXPORT_FORMATS, format_extension, format_label};
use crate::filters::{filter_label, op_label, op_needs_value, ops_for_kind};
use crate::format::format_estimated_rows;
use crate::grid::RowsDelegate;
use crate::history::{HistoryEntry, filter_history, push_history};
use crate::icons::SoquelIcon;
use crate::staged::{build_table_changes, preview_sql};
use crate::tabs::{
  FREE_TABS, TabsState, WorkspaceTab, activate_sibling, close_tab, effective_tab_limit,
  open_sql_tab, open_table_tab,
};
use crate::theme;

enum TabContent {
  Table {
    table: Entity<TableState<RowsDelegate>>,
    show_ddl: bool,
    ddl: Option<String>,
    inspector_split: Entity<ResizableState>,
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
  profile: ConnectionProfile,
  /// Reads the installed licence per tab-open to lift the free-tier cap; an
  /// activation writes the file, so the cap follows without reactive plumbing.
  data_dir: std::path::PathBuf,
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
  pub fn new(
    db: Db,
    profile: ConnectionProfile,
    data_dir: std::path::PathBuf,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) -> Self {
    Self::build(Some(db), profile, data_dir, window, cx)
  }

  /// Tab-lifecycle tests need no live database behind the workspace.
  #[cfg(test)]
  pub fn test_new(window: &mut Window, cx: &mut Context<Self>) -> Self {
    let profile = ConnectionProfile {
      id: "test".to_string(),
      name: "test".to_string(),
      env: soquel_core::profiles::Env::Dev,
      group: None,
      agent_access: Default::default(),
      credential: Default::default(),
      params: soquel_core::profiles::ConnectorParams::Sqlite {
        path: String::new(),
      },
    };
    // No licence file under an empty dir: the free tier, unless the env override.
    Self::build(None, profile, std::path::PathBuf::new(), window, cx)
  }

  fn build(
    db: Option<Db>,
    profile: ConnectionProfile,
    data_dir: std::path::PathBuf,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) -> Self {
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
    let server_version = db.as_ref().and_then(|db| db.server_version());
    let connect_task = {
      let db = db.clone();
      cx.spawn(async move |this, cx| {
        let Some(db) = db else {
          return;
        };
        // Schema first: it fills the sidebar and the completion entries.
        if let Ok(snapshot) = core::schema_snapshot(&db, cx).await {
          let _ = this.update(cx, |this, cx| {
            this.schema_entries.fill(&snapshot);
            this.snapshot = Some(snapshot);
            this.rebuild_tree(cx);
            cx.notify();
          });
        }

        // Dev convenience only: SOQUEL_GPUI_TABLE opens a tab on arrival.
        if let Ok(target) = std::env::var("SOQUEL_GPUI_TABLE")
          && let Some((schema, table)) = target.split_once('.')
        {
          let (schema, table) = (schema.to_string(), table.to_string());
          let _ = cx.update_window(handle, |_, window, app| {
            let _ = this.update(app, |this, cx| {
              this.open_table(schema, table, Vec::new(), window, cx);
            });
          });
        }
      })
    };

    Self {
      focus_handle,
      profile,
      data_dir,
      tabs: TabsState::default(),
      contents: HashMap::new(),
      cell_editor,
      provider,
      sidebar_split,
      tree,
      tree_filter,
      db,
      snapshot: None,
      server_version,
      status: SharedString::default(),
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
    let status = soquel_core::licence::read(&soquel_core::licence::path(&self.data_dir));
    effective_tab_limit(&status, tab_limit_override())
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
      let foreign_keys = info
        .map(|info| info.foreign_keys.clone())
        .unwrap_or_default();

      table.update(cx, |table, cx| {
        let delegate = table.delegate_mut();
        delegate.browse = Some((db, schema, name));
        delegate.filters = filters;
        delegate.can_ever_edit = can_ever_edit;
        delegate.key_columns = primary_key;
        delegate.foreign_keys = foreign_keys;
        delegate.include_xmin = can_ever_edit;
        delegate.reload(cx);
      });
      self.contents.insert(
        id,
        TabContent::Table {
          table,
          show_ddl: false,
          ddl: None,
          inspector_split: cx.new(|_| ResizableState::default()),
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

  pub(crate) fn focus_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
    if let Some((_, editor, _)) = self.active_sql() {
      editor.update(cx, |editor, cx| editor.focus(window, cx));
    }
  }

  pub(crate) fn open_sql(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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

  /// Test cleanup; the real close path is ops::disconnect, which drops the
  /// connection's orphaned sessions itself.
  #[cfg(test)]
  pub fn close_sessions(&mut self) {
    for (_, content) in self.contents.drain() {
      if let TabContent::Sql {
        session: Some(session),
        ..
      } = content
      {
        core::close_session(session);
      }
    }
  }

  fn activate(&mut self, id: String, cx: &mut Context<Self>) {
    // The close button lives inside the tab: its click also lands here with
    // the id that just closed. Activating a ghost would blank the content.
    if self.tabs.tabs.iter().any(|tab| tab.id() == id) {
      self.tabs.active_id = Some(id);
      cx.notify();
    }
  }

  pub(crate) fn cycle(&mut self, direction: i32, cx: &mut Context<Self>) {
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
    let this = cx.entity().downgrade();
    window.open_dialog(cx, move |dialog, window, cx| {
      let dialog = crate::dialogs::styled(dialog, window, cx);
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
                .debug_selector(|| "cancel-apply".into())
                .on_click(|_, window, cx| window.close_dialog(cx)),
            )
            .child(
              Button::new("confirm-apply")
                .primary()
                .label("Apply")
                .debug_selector(|| "confirm-apply".into())
                .on_click(move |_, window, cx| {
                  window.close_dialog(cx);
                  let changes = changes.clone();
                  let db = db.clone();
                  this
                    .update(cx, |this, cx| this.run_apply(db, changes, cx))
                    .ok();
                }),
            ),
        )
    });
  }

  fn run_apply(&mut self, db: Db, changes: TableChanges, cx: &mut Context<Self>) {
    let Some(table) = self.active_table() else {
      return;
    };
    let task = core::apply_changes(&db, changes, cx);
    self._work_task = cx.spawn(async move |this, cx| {
      let result = task.await;
      match result {
        Ok(applied) => {
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
        Err(error) => {
          // Staging is kept: the transaction rolled back server-side.
          let _ = this.update(cx, |_, cx| {
            crate::status::toast_error(&error, cx);
          });
        }
      }
    });
  }

  pub(crate) fn refresh_schema(&mut self, cx: &mut Context<Self>) {
    let Some(db) = self.db.clone() else {
      return;
    };
    self._work_task = cx.spawn(async move |this, cx| {
      if let Ok(snapshot) = core::schema_snapshot(&db, cx).await {
        let _ = this.update(cx, |this, cx| {
          this.schema_entries.fill(&snapshot);
          this.snapshot = Some(snapshot);
          this.rebuild_tree(cx);
          cx.notify();
        });
      }
    });
  }

  pub(crate) fn run(&mut self, cx: &mut Context<Self>) {
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
        None => match core::open_session(&db, cx).await {
          Ok(session) => {
            let _ = this.update(cx, |this, _| {
              if let Some(TabContent::Sql { session: slot, .. }) = this.contents.get_mut(&id) {
                *slot = Some(session.clone());
              }
            });
            session
          }
          Err(error) => {
            let _ = this.update(cx, |this, cx| {
              if let Some(TabContent::Sql { running, .. }) = this.contents.get_mut(&id) {
                *running = false;
              }
              cx.notify();
            });
            results.update(cx, |table, cx| {
              table.delegate_mut().status = crate::status::error(&error);
              cx.notify();
            });
            return;
          }
        },
      };
      let result = core::run_session_query(&session, sql.clone(), cx).await;
      let at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default();
      let (ok, duration_ms) = match &result {
        Ok(result) => (true, result.duration_ms),
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
          if let Ok(result) = &result
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
            Ok(result) => {
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
            Err(error) => {
              delegate.status = crate::status::error(&error);
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
      ..
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
    let task = core::table_ddl(&db, schema, name, cx);
    self._work_task = cx.spawn(async move |this, cx| {
      let result = task.await;
      let _ = this.update(cx, |this, cx| {
        if let Some(TabContent::Table { ddl, .. }) = this.contents.get_mut(&id) {
          *ddl = Some(match result {
            Ok(sql) => sql,
            Err(error) => format!("-- error: {error}"),
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
    let this = cx.entity().downgrade();
    let search = self.history_search.clone();
    window.open_dialog(cx, move |dialog, window, cx| {
      let dialog = crate::dialogs::styled(dialog, window, cx);
      let this_entity = this.clone();
      let query = search.read(cx).value().to_string();
      let Ok(entries) = this_entity.read_with(cx, |workspace, _| {
        filter_history(&workspace.history, &query)
      }) else {
        return dialog;
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
                .debug_selector(move || format!("history-{ix}"))
                .px_2()
                .py_1()
                .gap_2()
                .rounded(cx.theme().radius)
                .cursor_default()
                .hover(|s| s.bg(cx.theme().accent))
                .on_click(move |_, window, app| {
                  let sql = sql.clone();
                  window.close_dialog(app);
                  this
                    .update(app, |this, cx| this.load_history_entry(sql, window, cx))
                    .ok();
                })
                .child(
                  div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_sm()
                    .font_family(crate::theme::mono(cx))
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

  fn render_explain(&self, id: &str, cx: &mut Context<Self>) -> AnyElement {
    let Some(TabContent::Sql {
      explain: Some((plans, _)),
      explain_collapsed: collapsed,
      ..
    }) = self.contents.get(id)
    else {
      return div().into_any_element();
    };
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

    let row_count = visible_plan_nodes(plans, collapsed).len();
    let tab_id = id.to_string();
    let rows = cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
      let Some(TabContent::Sql {
        explain: Some((plans, _)),
        explain_collapsed: collapsed,
        ..
      }) = this.contents.get(&tab_id)
      else {
        return Vec::new();
      };
      let nodes = visible_plan_nodes(plans, collapsed);
      nodes[range]
        .iter()
        .map(|&(plan, node)| {
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
          h_flex()
            .px_3()
            .py_0p5()
            .gap_2()
            .items_center()
            .font_family(crate::theme::mono(cx))
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
            )
        })
        .collect::<Vec<_>>()
    });

    v_flex()
      .size_full()
      .child(
        h_flex()
          .px_3()
          .py_1()
          .gap_3()
          .border_b_1()
          .border_color(cx.theme().border)
          .font_family(crate::theme::mono(cx))
          .text_xs()
          .text_color(cx.theme().muted_foreground)
          .children(header.into_iter().map(|part| div().child(part))),
      )
      .child(
        uniform_list("explain-rows", row_count, rows)
          .flex_1()
          .min_h_0()
          .py_1(),
      )
      .into_any_element()
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
        self.status = crate::status::error(&error);
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
          let (mut progress, done) = core::export_rows(&db, request, format, path, cx);
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
                    Ok(summary) => format!(
                      "exported {:.0} rows - {:.0} ms",
                      summary.rows, summary.duration_ms
                    )
                    .into(),
                    Err(error) => crate::status::error(&error),
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
              Err(error) => crate::status::error(&error),
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

  /// FK hop: open the referenced table filtered to this row's key values.
  fn hop(&mut self, row_ix: usize, column: String, window: &mut Window, cx: &mut Context<Self>) {
    let Some(table) = self.active_table() else {
      return;
    };
    let Some((fk, filters)) = ({
      let state = table.read(cx);
      let delegate = state.delegate();
      delegate.fk_for(&column).map(|fk| {
        let filters: Vec<ColumnFilter> = fk
          .columns
          .iter()
          .zip(&fk.referenced_columns)
          .map(|(local, referenced)| {
            let display_ix = delegate
              .display_columns()
              .iter()
              .position(|c| &c.name == local);
            let value = display_ix.and_then(|ix| delegate.display_value(row_ix, ix));
            match value {
              None => ColumnFilter {
                column: referenced.clone(),
                op: FilterOp::IsNull,
                value: None,
              },
              Some(value) => ColumnFilter {
                column: referenced.clone(),
                op: FilterOp::Eq,
                value: Some(value),
              },
            }
          })
          .collect();
        (fk.clone(), filters)
      })
    }) else {
      return;
    };
    self.open_table(
      fk.referenced_schema.clone(),
      fk.referenced_table.clone(),
      filters,
      window,
      cx,
    );
  }

  fn render_inspector(
    &self,
    table: &Entity<TableState<RowsDelegate>>,
    row_ix: usize,
    col_ix: usize,
    cx: &mut Context<Self>,
  ) -> impl IntoElement {
    let state = table.read(cx);
    let delegate = state.delegate();
    let column = delegate.display_columns().get(col_ix).cloned();
    let value = delegate.display_value(row_ix, col_ix);
    let can_hop = column
      .as_ref()
      .is_some_and(|column| delegate.fk_for(&column.name).is_some())
      && value.is_some();
    let grid = table.clone();
    let column_name = column.as_ref().map(|c| c.name.clone()).unwrap_or_default();
    let data_type = column
      .as_ref()
      .and_then(|c| c.data_type.clone())
      .unwrap_or_default();
    let is_json = column.is_some_and(|c| c.kind == soquel_core::connectors::ColumnKind::Json);

    let pretty = value.as_ref().map(|value| {
      if is_json {
        serde_json::from_str::<serde_json::Value>(value)
          .and_then(|parsed| serde_json::to_string_pretty(&parsed))
          .unwrap_or_else(|_| value.clone())
      } else {
        value.clone()
      }
    });
    let copy_value = value.clone();
    let hop_column = column_name.clone();

    v_flex()
      .size_full()
      .border_l_1()
      .border_color(cx.theme().border)
      .child(
        h_flex()
          .px_3()
          .py_1()
          .gap_2()
          .items_center()
          .border_b_1()
          .border_color(cx.theme().border)
          .child(
            div()
              .min_w_0()
              .truncate()
              .text_sm()
              .font_semibold()
              .font_family(crate::theme::mono(cx))
              .child(column_name),
          )
          .child(
            div()
              .text_xs()
              .text_color(cx.theme().muted_foreground)
              .child(data_type),
          )
          .child(div().flex_1())
          .when(value.is_some(), |this| {
            this.child(
              Button::new("inspector-copy")
                .ghost()
                .xsmall()
                .label("Copy")
                .on_click(cx.listener(move |this, _, _, cx| {
                  if let Some(value) = &copy_value {
                    cx.write_to_clipboard(ClipboardItem::new_string(value.clone()));
                    this.status = "copied".into();
                    cx.notify();
                  }
                })),
            )
          })
          .child({
            let grid = table.clone();
            Button::new("inspector-row")
              .ghost()
              .xsmall()
              .label("Row")
              .dropdown_menu(move |menu, window, cx| {
                let Some(data) = grid.read(cx).delegate().row_menu_data(row_ix) else {
                  return menu;
                };
                crate::grid::build_row_menu(data, grid.clone(), row_ix, menu, window)
              })
          })
          .when(can_hop, |this| {
            this.child(
              Button::new("inspector-hop")
                .ghost()
                .xsmall()
                .label("Open ref")
                .on_click(cx.listener(move |this, _, window, cx| {
                  this.hop(row_ix, hop_column.clone(), window, cx);
                })),
            )
          })
          .child(
            Button::new("inspector-close")
              .ghost()
              .xsmall()
              .icon(Icon::new(IconName::Close))
              .on_click(cx.listener(move |_, _, _, cx| {
                grid.update(cx, |table, cx| table.clear_selection(cx));
              })),
          ),
      )
      .child(match pretty {
        None => v_flex()
          .flex_1()
          .p_3()
          .text_sm()
          .italic()
          .text_color(cx.theme().muted_foreground)
          .child("NULL")
          .into_any_element(),
        Some(text) if is_json => v_flex()
          .id("inspector-value")
          .flex_1()
          .min_h_0()
          .overflow_y_scroll()
          .p_2()
          .text_sm()
          .child(TextView::markdown(
            "inspector-json",
            format!("```json\n{text}\n```"),
          ))
          .into_any_element(),
        Some(text) => v_flex()
          .id("inspector-value")
          .flex_1()
          .min_h_0()
          .overflow_y_scroll()
          .p_3()
          .text_sm()
          .font_family(crate::theme::mono(cx))
          .child(text)
          .into_any_element(),
      })
  }

  fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
    let workspace = cx.entity();
    v_flex()
      .size_full()
      .child(
        h_flex()
          .px_2()
          .pt_2()
          .pb_1()
          .gap_1()
          .items_center()
          .child(div().flex_1().child(Input::new(&self.tree_filter).small()))
          .child(
            Button::new("refresh-schema")
              .ghost()
              .xsmall()
              .icon(Icon::new(SoquelIcon::RefreshCw))
              .tooltip("Refresh schema")
              .on_click(cx.listener(|this, _, _, cx| this.refresh_schema(cx))),
          ),
      )
      .child(
        v_flex().flex_1().min_h_0().px_1().child(
          tree(&self.tree, {
            let workspace = workspace.clone();
            move |ix, entry, selected, _, cx| {
              let item = entry.item();
              if entry.is_folder() {
                return ListItem::new(ix).rounded(cx.theme().radius).child(
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
                .rounded(cx.theme().radius)
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
            }
          })
          .context_menu({
            let workspace = workspace.clone();
            move |_, entry, menu, window, _| {
              let item = entry.item();
              let refresh = window
                .listener_for(&workspace, |this: &mut Workspace, _: &ClickEvent, _, cx| {
                  this.refresh_schema(cx)
                });
              if entry.is_folder() {
                let schema = item
                  .id
                  .strip_prefix("schema:")
                  .unwrap_or(&item.id)
                  .to_string();
                return menu
                  .item(
                    PopupMenuItem::new("Copy schema name").on_click(move |_, _, cx| {
                      cx.write_to_clipboard(ClipboardItem::new_string(schema.clone()));
                    }),
                  )
                  .separator()
                  .item(PopupMenuItem::new("Refresh schema").on_click(refresh));
              }
              let mut parts = item.id.split('\t');
              let schema = parts.next().unwrap_or_default().to_string();
              let name = parts.next().unwrap_or_default().to_string();
              let qualified = format!("{schema}.{name}");
              let open = window.listener_for(&workspace, {
                let (schema, name) = (schema.clone(), name.clone());
                move |this: &mut Workspace, _: &ClickEvent, window, cx| {
                  this.open_table(schema.clone(), name.clone(), Vec::new(), window, cx);
                }
              });
              menu
                .item(PopupMenuItem::new("Open").on_click(open))
                .item(PopupMenuItem::new("Copy name").on_click(move |_, _, cx| {
                  cx.write_to_clipboard(ClipboardItem::new_string(qualified.clone()));
                }))
                .separator()
                .item(PopupMenuItem::new("Refresh schema").on_click(refresh))
            }
          })
          .size_full(),
        ),
      )
  }

  fn render_tab_strip(&self, cx: &mut Context<Self>) -> impl IntoElement {
    let active = self.tabs.active_id.clone();
    h_flex()
      .px_2()
      // Fixed height: with no tab open, the strip must not shrink.
      .h(px(34.))
      .flex_none()
      .gap_1()
      .items_center()
      .border_b_1()
      .border_color(cx.theme().border)
      .children(self.tabs.tabs.iter().map(|tab| {
        let id = tab.id().to_string();
        let close_id = id.clone();
        let selected = active.as_deref() == Some(tab.id());
        h_flex()
          .id(SharedString::from(format!("tab-{id}")))
          .px_2()
          .py_0p5()
          .gap_1()
          .items_center()
          .rounded(cx.theme().radius)
          .text_sm()
          .cursor_default()
          .when(selected, |this| {
            this
              .bg(cx.theme().accent)
              .text_color(cx.theme().accent_foreground)
          })
          .when(!selected, |this| {
            this
              .text_color(cx.theme().muted_foreground)
              .hover(|this| this.bg(cx.theme().accent.opacity(0.5)))
          })
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
        crate::ui::chip(filter_label(filter), cx).gap_1().child(
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
        inspector_split,
      }) => {
        let table = table.clone();
        let show_ddl = *show_ddl;
        let ddl = ddl.clone();
        let inspector_split = inspector_split.clone();
        let selected_cell = table.read(cx).selected_cell();
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
              .child(match selected_cell {
                Some((row_ix, col_ix)) => v_flex().flex_1().min_h_0().child(
                  h_resizable("grid-inspector")
                    .with_state(&inspector_split)
                    .child(
                      resizable_panel().child(
                        v_flex()
                          .size_full()
                          .p_1()
                          .child(DataTable::new(&table).stripe(true)),
                      ),
                    )
                    .child(
                      resizable_panel()
                        .size(px(320.))
                        .size_range(px(200.)..px(640.))
                        .child(
                          self
                            .render_inspector(&table, row_ix, col_ix, cx)
                            .into_any_element(),
                        ),
                    ),
                ),
                None => v_flex()
                  .flex_1()
                  .min_h_0()
                  .p_1()
                  .child(DataTable::new(&table).stripe(true)),
              })
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
                  Some(_) => v_flex()
                    .size_full()
                    .child(self.render_explain(&id, cx))
                    .into_any_element(),
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

  /// The active grid's line for the app footer, or the view's own status.
  pub(crate) fn footer_status(&self, cx: &App) -> SharedString {
    self
      .active_grid()
      .map(|grid| grid.read(cx).delegate().status.clone())
      .unwrap_or_else(|| self.status.clone())
  }

  pub(crate) fn footer_connection(&self) -> String {
    match &self.server_version {
      Some(version) => {
        let (engine, version) =
          crate::connections::server_badge(self.profile.params.kind(), version);
        format!("{} - {engine} {version}", self.profile.name)
      }
      None => self.profile.name.clone(),
    }
  }
}

impl Render for Workspace {
  fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    v_flex()
      .size_full()
      .track_focus(&self.focus_handle)
      .bg(theme::canvas(cx))
      .on_action(cx.listener(|this, _: &RunQuery, _, cx| this.run(cx)))
      .on_action(cx.listener(|this, _: &RefreshSchema, _, cx| this.refresh_schema(cx)))
      .on_action(cx.listener(|this, _: &NextTab, _, cx| this.cycle(1, cx)))
      .on_action(cx.listener(|this, _: &PrevTab, _, cx| this.cycle(-1, cx)))
      .on_action(cx.listener(|this, _: &NewSqlTab, window, cx| this.open_sql(window, cx)))
      .on_action(cx.listener(|_, _: &ToggleThemeMode, window, cx| {
        theme::toggle(window, cx);
      }))
      .on_action(cx.listener(|this, _: &FocusEditor, window, cx| this.focus_editor(window, cx)))
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
                  .bg(theme::panel(cx))
                  .border_l_1()
                  .border_color(cx.theme().border)
                  .child(self.render_tab_strip(cx))
                  .child(self.render_active_content(cx)),
              ),
            ),
        ),
      )
  }
}

#[cfg(test)]
mod tests {
  // The parent's `use gpui::*` puts gpui's `test` macro in scope, which would
  // make the generated `#[test]` expand itself forever. Shadow it back.
  use ::core::prelude::v1::test;
  use gpui::TestAppContext;
  use soquel_core::connectors::{ColumnKind, QueryColumn};
  use soquel_core::credentials::Credentials;
  use soquel_core::profiles::{ConnectorParams, Env};

  use super::*;
  use crate::test_support::{shell_window, wait_until};

  fn column(name: &str, data_type: &str, kind: ColumnKind) -> QueryColumn {
    QueryColumn {
      name: name.to_string(),
      data_type: Some(data_type.to_string()),
      kind,
    }
  }

  /// A table tab whose grid is seeded in memory: no database behind it, so
  /// reloads are no-ops and everything settles deterministically.
  fn open_seeded_people(
    this: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
  ) -> Entity<TableState<RowsDelegate>> {
    this.tabs =
      open_table_tab(&this.tabs, "main", "people", Vec::new(), usize::MAX).expect("under the cap");
    let id = this.tabs.active_id.clone().expect("just opened");
    let table = this.new_grid(window, cx);
    table.update(cx, |table, _| {
      let delegate = table.delegate_mut();
      delegate.columns = vec![
        column("id", "integer", ColumnKind::Number),
        column("name", "text", ColumnKind::Text),
      ];
      delegate.rows = vec![
        vec![Some("1".into()), Some("Ada".into())],
        vec![Some("2".into()), Some("Alan".into())],
      ];
      delegate.can_ever_edit = true;
      delegate.key_columns = vec!["id".to_string()];
      delegate.loading = false;
      delegate.eof = true;
    });
    this.contents.insert(
      id,
      TabContent::Table {
        table: table.clone(),
        show_ddl: false,
        ddl: None,
        inspector_split: cx.new(|_| ResizableState::default()),
      },
    );
    table
  }

  fn sqlite_profile(path: &std::path::Path) -> ConnectionProfile {
    ConnectionProfile {
      id: "ws-test".to_string(),
      name: "ws test".to_string(),
      env: Env::Dev,
      group: None,
      agent_access: Default::default(),
      credential: Default::default(),
      params: ConnectorParams::Sqlite {
        path: path.to_string_lossy().into_owned(),
      },
    }
  }

  #[gpui::test]
  fn activating_a_ghost_tab_is_ignored(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let cx = cx.add_empty_window();
    let workspace = cx.update(|window, cx| cx.new(|cx| Workspace::test_new(window, cx)));
    cx.update(|window, cx| {
      workspace.update(cx, |this, cx| {
        this.open_sql(window, cx);
        let real = this.tabs.active_id.clone();
        // The close button's click also bubbles to the tab with the closed id.
        this.activate("ghost".to_string(), cx);
        assert_eq!(this.tabs.active_id, real);
      });
    });
  }

  #[gpui::test]
  fn closing_a_tab_drops_its_content_and_picks_the_neighbor(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let cx = cx.add_empty_window();
    let workspace = cx.update(|window, cx| cx.new(|cx| Workspace::test_new(window, cx)));
    cx.update(|window, cx| {
      workspace.update(cx, |this, cx| {
        this.open_sql(window, cx);
        let first = this.tabs.active_id.clone().unwrap();
        this.open_sql(window, cx);
        let second = this.tabs.active_id.clone().unwrap();

        this.close(&second, cx);
        assert!(!this.contents.contains_key(&second));
        assert_eq!(this.tabs.active_id.as_deref(), Some(first.as_str()));
      });
    });
  }

  #[gpui::test]
  fn sql_tabs_number_past_closed_ones(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let cx = cx.add_empty_window();
    let workspace = cx.update(|window, cx| cx.new(|cx| Workspace::test_new(window, cx)));
    cx.update(|window, cx| {
      workspace.update(cx, |this, cx| {
        this.open_sql(window, cx);
        let first = this.tabs.active_id.clone().unwrap();
        this.open_sql(window, cx);
        this.close(&first, cx);
        // The model is covered in tabs.rs; this asserts the workspace routes
        // through it rather than numbering on its own.
        this.open_sql(window, cx);
        let titles: Vec<String> = this.tabs.tabs.iter().map(|tab| tab.title()).collect();
        assert_eq!(titles, vec!["sql 2", "sql 3"]);
      });
    });
  }

  #[gpui::test]
  fn the_filter_row_offers_the_grids_columns_and_ops_follow_the_kind(cx: &mut TestAppContext) {
    let (workspace, cx) = shell_window(cx, Workspace::test_new);
    cx.update(|window, cx| {
      workspace.update(cx, |this, cx| {
        open_seeded_people(this, window, cx);
        this.toggle_filter_row(window, cx);
        assert!(this.filter_open);
        // The picker got the grid's columns, in column order.
        this.filter_column.update(cx, |state, cx| {
          state.set_selected_index(Some(IndexPath::new(1)), window, cx);
        });
        assert_eq!(
          this.filter_column.read(cx).selected_value(),
          Some(&"name".to_string())
        );

        this.sync_filter_ops("id".to_string(), window, cx);
        assert_eq!(this.filter_ops, ops_for_kind(ColumnKind::Number));
        // The first op is preselected so Add works right away.
        assert_eq!(
          this.filter_op.read(cx).selected_value(),
          Some(&"=".to_string())
        );
      });
    });
  }

  #[gpui::test]
  fn apply_filter_requires_a_value_and_replaces_per_column(cx: &mut TestAppContext) {
    let (workspace, cx) = shell_window(cx, Workspace::test_new);
    cx.update(|window, cx| {
      workspace.update(cx, |this, cx| {
        let table = open_seeded_people(this, window, cx);
        this.toggle_filter_row(window, cx);
        this.filter_column.update(cx, |state, cx| {
          state.set_selected_index(Some(IndexPath::new(1)), window, cx);
        });
        this.sync_filter_ops("name".to_string(), window, cx);

        // Text ops: [=, !=, contains, starts with, is null, is not null].
        this.filter_op.update(cx, |state, cx| {
          state.set_selected_index(Some(IndexPath::new(2)), window, cx);
        });
        this
          .filter_value
          .update(cx, |state, cx| state.set_value("da", window, cx));
        this.apply_filter(cx);
        let filters = table.read(cx).delegate().filters.clone();
        assert_eq!(filters.len(), 1);
        assert_eq!(filters[0].op, FilterOp::Contains);
        assert_eq!(filters[0].value.as_deref(), Some("da"));

        // The same column again replaces instead of stacking.
        this.filter_op.update(cx, |state, cx| {
          state.set_selected_index(Some(IndexPath::new(0)), window, cx);
        });
        this
          .filter_value
          .update(cx, |state, cx| state.set_value("Ada", window, cx));
        this.apply_filter(cx);
        let filters = table.read(cx).delegate().filters.clone();
        assert_eq!(filters.len(), 1);
        assert_eq!(filters[0].op, FilterOp::Eq);

        // A blank value on an op that takes one applies nothing.
        this
          .filter_value
          .update(cx, |state, cx| state.set_value("  ", window, cx));
        this.apply_filter(cx);
        assert_eq!(
          table.read(cx).delegate().filters[0].value.as_deref(),
          Some("Ada")
        );

        // Nullness ops carry no value at all.
        this.filter_op.update(cx, |state, cx| {
          state.set_selected_index(Some(IndexPath::new(4)), window, cx);
        });
        this.apply_filter(cx);
        let filters = table.read(cx).delegate().filters.clone();
        assert_eq!(filters[0].op, FilterOp::IsNull);
        assert_eq!(filters[0].value, None);
      });
    });
  }

  #[gpui::test]
  fn removing_filters_reaches_the_grid_one_column_or_all(cx: &mut TestAppContext) {
    let (workspace, cx) = shell_window(cx, Workspace::test_new);
    cx.update(|window, cx| {
      workspace.update(cx, |this, cx| {
        let table = open_seeded_people(this, window, cx);
        table.update(cx, |table, _| {
          table.delegate_mut().filters = vec![
            ColumnFilter {
              column: "id".into(),
              op: FilterOp::Eq,
              value: Some("1".into()),
            },
            ColumnFilter {
              column: "name".into(),
              op: FilterOp::Contains,
              value: Some("a".into()),
            },
          ];
        });
        this.remove_filter("name", cx);
        assert_eq!(table.read(cx).delegate().filters.len(), 1);
        assert_eq!(table.read(cx).delegate().filters[0].column, "id");
        this.clear_filters(cx);
        assert!(table.read(cx).delegate().filters.is_empty());
      });
    });
  }

  #[gpui::test]
  fn enable_ctid_rekeys_a_keyless_grid(cx: &mut TestAppContext) {
    let (workspace, cx) = shell_window(cx, Workspace::test_new);
    cx.update(|window, cx| {
      workspace.update(cx, |this, cx| {
        let table = open_seeded_people(this, window, cx);
        table.update(cx, |table, _| table.delegate_mut().key_columns = Vec::new());
        assert!(!table.read(cx).delegate().editable());
        this.enable_ctid(cx);
        let state = table.read(cx);
        let delegate = state.delegate();
        assert!(delegate.ctid_mode);
        assert_eq!(delegate.key_columns, vec!["ctid".to_string()]);
        assert!(delegate.editable());
      });
    });
  }

  #[gpui::test]
  fn export_copy_writes_the_visible_grid_and_skips_hidden_keys(cx: &mut TestAppContext) {
    let (workspace, cx) = shell_window(cx, Workspace::test_new);
    cx.update(|window, cx| {
      workspace.update(cx, |this, cx| {
        let table = open_seeded_people(this, window, cx);
        // The driver prepends system columns; exports must not leak them.
        table.update(cx, |table, _| {
          let delegate = table.delegate_mut();
          delegate
            .columns
            .insert(0, column("xmin", "xid", ColumnKind::Other));
          for (ix, row) in delegate.rows.iter_mut().enumerate() {
            row.insert(0, Some(format!("77{ix}")));
          }
          delegate.include_xmin = true;
        });
        this.export_copy(ExportFormat::Csv, cx);
        assert_eq!(this.status.to_string(), "copied 2 rows as CSV");
      });
    });
    let text = cx
      .read_from_clipboard()
      .and_then(|item| item.text())
      .expect("the export landed in the clipboard");
    assert!(text.contains("Ada") && text.contains("Alan"));
    assert!(!text.contains("xmin") && !text.contains("770"));
  }

  #[gpui::test]
  fn the_free_tier_refuses_a_third_tab(cx: &mut TestAppContext) {
    let (workspace, cx) = shell_window(cx, Workspace::test_new);
    cx.update(|window, cx| {
      workspace.update(cx, |this, cx| {
        this.open_sql(window, cx);
        this.open_sql(window, cx);
        this.open_sql(window, cx);
        assert_eq!(this.tabs.tabs.len(), FREE_TABS);
        assert_eq!(this.contents.len(), FREE_TABS);
      });
    });
  }

  #[gpui::test]
  fn apply_staged_previews_the_sql_and_cancel_keeps_the_staging(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ws.db");
    std::fs::File::create(&path).unwrap();
    // A real handle for the browse slot; nothing is fetched through it here.
    let db = crate::core::connect_with_blocking(sqlite_profile(&path), Credentials::fixed(None))
      .expect("opens the sqlite file");
    let (workspace, cx) = shell_window(cx, Workspace::test_new);
    cx.update(|window, cx| {
      workspace.update(cx, |this, cx| {
        let table = open_seeded_people(this, window, cx);
        table.update(cx, |table, _| {
          let delegate = table.delegate_mut();
          delegate.browse = Some((db.clone(), "main".to_string(), "people".to_string()));
          delegate.staged.edits.insert(
            0,
            [("name".to_string(), Some("Ada II".to_string()))]
              .into_iter()
              .collect(),
          );
        });
        this.apply_staged(window, cx);
      });
    });
    cx.run_until_parked();
    let bounds = cx
      .debug_bounds("cancel-apply")
      .expect("the preview dialog is open");
    cx.simulate_click(bounds.center(), Modifiers::none());
    cx.run_until_parked();
    assert!(
      cx.debug_bounds("cancel-apply").is_none(),
      "the dialog closed"
    );
    cx.update(|_, cx| {
      let this = workspace.read(cx);
      let table = this.active_table().expect("a table tab");
      assert_eq!(table.read(cx).delegate().staged.count(), 1);
    });
  }

  #[gpui::test]
  fn the_history_dialog_loads_an_entry_into_the_editor(cx: &mut TestAppContext) {
    let (workspace, cx) = shell_window(cx, Workspace::test_new);
    cx.update(|window, cx| {
      workspace.update(cx, |this, cx| {
        this.open_sql(window, cx);
        this.history = vec![
          HistoryEntry {
            sql: "select 2".to_string(),
            at_ms: 2,
            duration_ms: 1.,
            ok: true,
          },
          HistoryEntry {
            sql: "select 1".to_string(),
            at_ms: 1,
            duration_ms: 1.,
            ok: false,
          },
        ];
        this.open_history(window, cx);
      });
    });
    cx.run_until_parked();
    let bounds = cx.debug_bounds("history-1").expect("two history rows");
    cx.simulate_click(bounds.center(), Modifiers::none());
    cx.run_until_parked();
    assert!(cx.debug_bounds("history-1").is_none(), "the dialog closed");
    cx.update(|_, cx| {
      let this = workspace.read(cx);
      let (_, editor, _) = this.active_sql().expect("a sql tab");
      assert_eq!(editor.read(cx).value().to_string(), "select 1");
    });
  }

  /// The table surface end to end on a real sqlite file: the schema fills the
  /// sidebar, a filter reloads through the database, staged edits apply
  /// through the preview dialog, an fk hops, the ddl fetches. Gated like the
  /// other sqlite flow.
  #[gpui::test]
  async fn integration_workspace_sqlite_table_flow(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    if soquel_core::integration_env("SOQUEL_TEST_SQLITE").is_none() {
      return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("flow.db");
    std::fs::File::create(&path).unwrap();
    let profile = sqlite_profile(&path);
    let db = crate::core::connect_with(profile.clone(), Credentials::fixed(None), cx)
      .await
      .expect("opens the sqlite file");
    let session = crate::core::open_session(&db, cx).await.expect("session");
    for sql in [
      "create table orgs (id integer primary key, name text)",
      "create table people (id integer primary key, name text, org_id integer references orgs(id))",
      "insert into orgs values (1, 'Umbrella'), (2, 'Acme')",
      "insert into people values (1, 'Ada', 2), (2, 'Alan', 2), (3, 'Grace', 1)",
    ] {
      crate::core::run_session_query(&session, sql.to_string(), cx)
        .await
        .expect("seed");
    }
    crate::core::close_session(session);

    let data_dir = dir.path().to_path_buf();
    let (workspace, cx) = shell_window(cx, |window, cx| {
      Workspace::new(db, profile, data_dir, window, cx)
    });
    wait_until(cx, "the schema snapshot", |cx| {
      workspace.read_with(cx, |this, _| this.snapshot.is_some())
    });

    // The sidebar's open: rows arrive, editing is keyed on the pk.
    cx.update(|window, cx| {
      workspace.update(cx, |this, cx| {
        this.open_table(
          "main".to_string(),
          "people".to_string(),
          Vec::new(),
          window,
          cx,
        );
      });
    });
    wait_until(cx, "the people rows", |cx| {
      workspace.read_with(cx, |this, cx| {
        this
          .active_table()
          .is_some_and(|table| table.read(cx).delegate().rows.len() == 3)
      })
    });
    workspace.read_with(cx, |this, cx| {
      assert!(this.active_table().unwrap().read(cx).delegate().editable());
    });

    // A filter reloads through the database.
    cx.update(|window, cx| {
      workspace.update(cx, |this, cx| {
        this.toggle_filter_row(window, cx);
        this.filter_column.update(cx, |state, cx| {
          state.set_selected_index(Some(IndexPath::new(1)), window, cx);
        });
        this.sync_filter_ops("name".to_string(), window, cx);
        this.filter_op.update(cx, |state, cx| {
          // Text ops: "starts with" sits fourth.
          state.set_selected_index(Some(IndexPath::new(3)), window, cx);
        });
        this
          .filter_value
          .update(cx, |state, cx| state.set_value("A", window, cx));
        this.apply_filter(cx);
      });
    });
    wait_until(cx, "the filtered rows", |cx| {
      workspace.read_with(cx, |this, cx| {
        this
          .active_table()
          .is_some_and(|table| table.read(cx).delegate().rows.len() == 2)
      })
    });
    cx.update(|_, cx| workspace.update(cx, |this, cx| this.clear_filters(cx)));
    wait_until(cx, "the filters cleared", |cx| {
      workspace.read_with(cx, |this, cx| {
        this
          .active_table()
          .is_some_and(|table| table.read(cx).delegate().rows.len() == 3)
      })
    });

    // Stage an edit, apply it through the preview dialog, reread.
    cx.update(|window, cx| {
      workspace.update(cx, |this, cx| {
        let table = this.active_table().expect("a table tab");
        table.update(cx, |table, _| {
          table.delegate_mut().staged.edits.insert(
            0,
            [("name".to_string(), Some("Ada II".to_string()))]
              .into_iter()
              .collect(),
          );
        });
        this.apply_staged(window, cx);
      });
    });
    cx.run_until_parked();
    let bounds = cx
      .debug_bounds("confirm-apply")
      .expect("the preview dialog");
    cx.simulate_click(bounds.center(), Modifiers::none());
    wait_until(cx, "the apply lands", |cx| {
      workspace.read_with(cx, |this, _| this.status.starts_with("applied: 1 updated"))
    });
    wait_until(cx, "the reread", |cx| {
      workspace.read_with(cx, |this, cx| {
        this.active_table().is_some_and(|table| {
          table
            .read(cx)
            .delegate()
            .rows
            .first()
            .is_some_and(|row| row[1] == Some("Ada II".to_string()))
        })
      })
    });

    // The fk hop opens the referenced table filtered to the row's key.
    cx.update(|window, cx| {
      workspace.update(cx, |this, cx| this.hop(0, "org_id".to_string(), window, cx));
    });
    wait_until(cx, "the referenced org", |cx| {
      workspace.read_with(cx, |this, cx| {
        this
          .active_tab()
          .is_some_and(|tab| tab.title() == "main.orgs")
          && this.active_table().is_some_and(|table| {
            let state = table.read(cx);
            let delegate = state.delegate();
            delegate.rows.len() == 1 && delegate.rows[0][1] == Some("Acme".to_string())
          })
      })
    });

    // The ddl view fetches once, on first toggle.
    cx.update(|_, cx| workspace.update(cx, |this, cx| this.toggle_ddl(true, cx)));
    wait_until(cx, "the ddl", |cx| {
      workspace.read_with(cx, |this, _| {
        let Some(id) = this.tabs.active_id.as_deref() else {
          return false;
        };
        matches!(
          this.contents.get(id),
          Some(TabContent::Table { ddl: Some(ddl), .. }) if ddl.to_lowercase().contains("create table")
        )
      })
    });
  }

  /// The sql surface end to end on a real sqlite file: run fills the grid and
  /// the history, a rerun collapses, a failure lands in both, export copies.
  #[gpui::test]
  async fn integration_workspace_sqlite_sql_flow(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    if soquel_core::integration_env("SOQUEL_TEST_SQLITE").is_none() {
      return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("flow.db");
    std::fs::File::create(&path).unwrap();
    let profile = sqlite_profile(&path);
    let db = crate::core::connect_with(profile.clone(), Credentials::fixed(None), cx)
      .await
      .expect("opens the sqlite file");
    let session = crate::core::open_session(&db, cx).await.expect("session");
    for sql in [
      "create table people (id integer primary key, name text)",
      "insert into people values (1, 'Ada'), (2, 'Alan')",
    ] {
      crate::core::run_session_query(&session, sql.to_string(), cx)
        .await
        .expect("seed");
    }
    crate::core::close_session(session);

    let data_dir = dir.path().to_path_buf();
    let (workspace, cx) = shell_window(cx, |window, cx| {
      Workspace::new(db, profile, data_dir, window, cx)
    });
    wait_until(cx, "the schema snapshot", |cx| {
      workspace.read_with(cx, |this, _| this.snapshot.is_some())
    });

    cx.update(|window, cx| {
      workspace.update(cx, |this, cx| {
        this.open_sql(window, cx);
        let (_, editor, _) = this.active_sql().expect("a sql tab");
        editor.update(cx, |editor, cx| {
          editor.set_value("select name from people order by id", window, cx);
        });
        this.run(cx);
      });
    });
    wait_until(cx, "the results", |cx| {
      workspace.read_with(cx, |this, cx| {
        this.history.len() == 1
          && this
            .active_sql()
            .is_some_and(|(_, _, results)| results.read(cx).delegate().rows.len() == 2)
      })
    });
    workspace.read_with(cx, |this, cx| {
      assert!(this.history[0].ok);
      let (id, _, results) = this.active_sql().expect("a sql tab");
      // One pinned session per tab from the first run on.
      assert!(matches!(
        this.contents.get(&id),
        Some(TabContent::Sql {
          session: Some(_),
          ..
        })
      ));
      assert_eq!(
        results.read(cx).delegate().rows[0][0],
        Some("Ada".to_string())
      );
    });

    // A consecutive rerun collapses into one history entry.
    cx.update(|_, cx| workspace.update(cx, |this, cx| this.run(cx)));
    wait_until(cx, "the rerun settles", |cx| {
      workspace.read_with(cx, |this, _| {
        let Some(id) = this.tabs.active_id.as_deref() else {
          return false;
        };
        matches!(
          this.contents.get(id),
          Some(TabContent::Sql { running: false, .. })
        )
      })
    });
    workspace.read_with(cx, |this, _| assert_eq!(this.history.len(), 1));

    // Export what the grid holds.
    cx.update(|_, cx| workspace.update(cx, |this, cx| this.export_copy(ExportFormat::Csv, cx)));
    let text = cx
      .read_from_clipboard()
      .and_then(|item| item.text())
      .expect("copied");
    assert!(text.contains("Ada"));
    workspace.read_with(cx, |this, _| {
      assert_eq!(this.status.to_string(), "copied 2 rows as CSV");
    });

    // A failing statement lands in the grid status and the history.
    cx.update(|window, cx| {
      workspace.update(cx, |this, cx| {
        let (_, editor, _) = this.active_sql().expect("a sql tab");
        editor.update(cx, |editor, cx| {
          editor.set_value("select * from missing", window, cx);
        });
        this.run(cx);
      });
    });
    wait_until(cx, "the failure lands", |cx| {
      workspace.read_with(cx, |this, _| this.history.len() == 2 && !this.history[0].ok)
    });
    workspace.read_with(cx, |this, cx| {
      let (_, _, results) = this.active_sql().expect("a sql tab");
      assert!(results.read(cx).delegate().status.starts_with("error:"));
    });

    cx.update(|_, cx| workspace.update(cx, |this, _| this.close_sessions()));
    cx.run_until_parked();
  }
}
