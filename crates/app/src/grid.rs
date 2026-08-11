use gpui::prelude::FluentBuilder;
use gpui::{
  App, Context, Entity, InteractiveElement, IntoElement, ParentElement, SharedString, Styled, Task,
  Window, div, px,
};
use gpui_component::input::{Input, InputState};
use gpui_component::table::{Column, ColumnSort, TableDelegate, TableState};
use gpui_component::{ActiveTheme, Sizable};
use soquel_core::connectors::{ColumnFilter, ForeignKeyInfo, QueryColumn, SortDirection, SortSpec};

use crate::cell_editing::{
  CellPosition, editor_mode, editor_value_valid, initial_editor_value, next_editable_position,
  staged_value,
};
use crate::core::{self, Db};
use crate::staged::StagedChanges;

pub const PAGE_SIZE: u32 = 500;

enum RowSlot {
  Insert(usize),
  Data(usize),
}

pub struct RowsDelegate {
  /// Set while browsing a table: load_more pages it. Query results leave it None.
  pub browse: Option<(Db, String, String)>,
  pub columns: Vec<QueryColumn>,
  pub rows: Vec<Vec<Option<String>>>,
  pub sort: Option<SortSpec>,
  pub filters: Vec<ColumnFilter>,
  /// Row identity for editing: primary key, or ctid once the rescue is enabled.
  pub key_columns: Vec<String>,
  pub foreign_keys: Vec<ForeignKeyInfo>,
  /// Tables only: views and matviews are read-only whatever their keys.
  pub can_ever_edit: bool,
  pub ctid_mode: bool,
  pub include_xmin: bool,
  pub staged: StagedChanges,
  /// (data row index or rows.len()+insert index, display column index).
  pub editing: Option<(usize, usize)>,
  pub editor: Entity<InputState>,
  pub loading: bool,
  pub eof: bool,
  pub status: SharedString,
  _load_task: Task<()>,
}

impl RowsDelegate {
  pub fn new(editor: Entity<InputState>) -> Self {
    Self {
      browse: None,
      columns: Vec::new(),
      rows: Vec::new(),
      sort: None,
      filters: Vec::new(),
      key_columns: Vec::new(),
      foreign_keys: Vec::new(),
      can_ever_edit: false,
      ctid_mode: false,
      include_xmin: false,
      staged: StagedChanges::default(),
      editing: None,
      editor,
      loading: true,
      eof: false,
      status: "connecting...".into(),
      _load_task: Task::ready(()),
    }
  }

  /// System columns (ctid key, xmin guard) are fetched for keying but never displayed.
  pub fn hidden_lead(&self) -> usize {
    self
      .columns
      .iter()
      .take_while(|col| col.name == "ctid" || col.name == "xmin")
      .count()
  }

  pub fn display_columns(&self) -> &[QueryColumn] {
    &self.columns[self.hidden_lead()..]
  }

  pub fn fk_for(&self, column: &str) -> Option<&ForeignKeyInfo> {
    self
      .foreign_keys
      .iter()
      .find(|fk| fk.columns.iter().any(|c| c == column))
  }

  /// The displayed value at a display-column position, staged edits included.
  pub fn display_value(&self, row_ix: usize, display_col: usize) -> Option<String> {
    if display_col >= self.display_columns().len() {
      return None;
    }
    let real_col = display_col + self.hidden_lead();
    self.cell_value(row_ix, real_col)
  }

  pub fn editable(&self) -> bool {
    self.can_ever_edit && !self.key_columns.is_empty()
  }

  /// Key columns for building changes; xmin rides along as the optimistic-lock guard.
  pub fn change_keys(&self) -> Vec<String> {
    let mut keys = self.key_columns.clone();
    if self.include_xmin && self.columns.iter().any(|col| col.name == "xmin") {
      keys.push("xmin".to_string());
    }
    keys
  }

  /// Inserts render at the top: nothing shifts under them while paging.
  fn slot(&self, row_ix: usize) -> RowSlot {
    if row_ix < self.staged.inserts.len() {
      RowSlot::Insert(row_ix)
    } else {
      RowSlot::Data(row_ix - self.staged.inserts.len())
    }
  }

  fn cell_value(&self, row_ix: usize, real_col: usize) -> Option<String> {
    let column = &self.columns[real_col];
    match self.slot(row_ix) {
      RowSlot::Insert(insert_ix) => self.staged.inserts[insert_ix]
        .get(&column.name)
        .cloned()
        .flatten(),
      RowSlot::Data(data_ix) => {
        if let Some(edit) = self
          .staged
          .edits
          .get(&data_ix)
          .and_then(|cells| cells.get(&column.name))
        {
          return edit.clone();
        }
        self.rows[data_ix][real_col].clone()
      }
    }
  }

  pub fn start_edit(
    &mut self,
    row_ix: usize,
    display_col: usize,
    window: &mut Window,
    cx: &mut Context<TableState<Self>>,
  ) {
    if !self.editable() || display_col >= self.display_columns().len() {
      return;
    }
    if let RowSlot::Data(data_ix) = self.slot(row_ix)
      && self.staged.deletes.contains(&data_ix)
    {
      return;
    }
    let real_col = display_col + self.hidden_lead();
    let column = self.columns[real_col].clone();
    let mode = editor_mode(&column);
    let value = self.cell_value(row_ix, real_col);
    self.editing = Some((row_ix, display_col));
    self.editor.update(cx, |editor, cx| {
      editor.set_value(initial_editor_value(mode, value.as_deref()), window, cx);
      editor.focus(window, cx);
    });
    cx.notify();
  }

  /// Returns false when the value is invalid (json gate): the editor stays open.
  pub fn commit_edit(&mut self, cx: &mut Context<TableState<Self>>) -> bool {
    let Some((row_ix, display_col)) = self.editing else {
      return true;
    };
    let real_col = display_col + self.hidden_lead();
    let column = self.columns[real_col].clone();
    let mode = editor_mode(&column);
    let text = self.editor.read(cx).value().to_string();
    if !editor_value_valid(mode, &text) {
      return false;
    }
    let value = staged_value(mode, &text);

    match self.slot(row_ix) {
      RowSlot::Insert(insert_ix) => {
        self.staged.inserts[insert_ix].insert(column.name, value);
      }
      RowSlot::Data(data_ix) => {
        let original = &self.rows[data_ix][real_col];
        let cells = self.staged.edits.entry(data_ix).or_default();
        if *original == value {
          // Back to the original: the cell is no longer dirty.
          cells.remove(&column.name);
          if cells.is_empty() {
            self.staged.edits.remove(&data_ix);
          }
        } else {
          cells.insert(column.name, value);
        }
      }
    }
    self.editing = None;
    cx.notify();
    true
  }

  pub fn cancel_edit(&mut self, cx: &mut Context<TableState<Self>>) {
    self.editing = None;
    cx.notify();
  }

  /// Tab order across editable cells; commits the current one first.
  pub fn edit_neighbor(
    &mut self,
    direction: i32,
    window: &mut Window,
    cx: &mut Context<TableState<Self>>,
  ) {
    let Some((row_ix, display_col)) = self.editing else {
      return;
    };
    if !self.commit_edit(cx) {
      return;
    }
    let total_rows = self.rows.len() + self.staged.inserts.len();
    if let Some(next) = next_editable_position(
      CellPosition {
        row_index: row_ix,
        position: display_col,
      },
      direction,
      self.display_columns().len(),
      total_rows,
    ) {
      self.start_edit(next.row_index, next.position, window, cx);
    }
  }

  pub fn add_insert(&mut self, duplicate_of: Option<usize>, cx: &mut Context<TableState<Self>>) {
    let mut values = std::collections::BTreeMap::new();
    if let Some(RowSlot::Data(data_ix)) = duplicate_of.map(|row_ix| self.slot(row_ix)) {
      let hidden = self.hidden_lead();
      for (ix, column) in self.columns.iter().enumerate().skip(hidden) {
        values.insert(column.name.clone(), self.rows[data_ix][ix].clone());
      }
    }
    self.staged.inserts.push(values);
    cx.notify();
  }

  pub fn toggle_delete(&mut self, row_ix: usize, cx: &mut Context<TableState<Self>>) {
    match self.slot(row_ix) {
      RowSlot::Insert(insert_ix) => {
        self.staged.inserts.remove(insert_ix);
      }
      RowSlot::Data(data_ix) => {
        if !self.staged.deletes.remove(&data_ix) {
          self.staged.deletes.insert(data_ix);
        }
      }
    }
    self.editing = None;
    cx.notify();
  }

  pub fn discard(&mut self, cx: &mut Context<TableState<Self>>) {
    self.staged.clear();
    self.editing = None;
    cx.notify();
  }

  fn request(&self, offset: u32) -> Option<(Db, soquel_core::connectors::TableRowsRequest)> {
    let (db, schema, name) = self.browse.clone()?;
    let mut request = core::page_request(&schema, &name, offset, PAGE_SIZE);
    request.sort = self.sort.clone();
    request.filters = self.filters.clone();
    request.include_ctid = self.ctid_mode;
    request.include_xmin = self.include_xmin;
    Some((db, request))
  }

  /// Refetch from the top with the current sort and filters, replacing the rows.
  pub fn reload(&mut self, cx: &mut Context<TableState<Self>>) {
    let Some((db, request)) = self.request(0) else {
      return;
    };
    let (schema, name) = {
      let (_, schema, name) = self.browse.as_ref().unwrap();
      (schema.clone(), name.clone())
    };
    self.loading = true;
    self.eof = false;
    self.staged.clear();
    self.editing = None;
    let task = core::fetch_rows(&db, request, cx);

    self._load_task = cx.spawn(async move |view, cx| {
      let result = task.await;
      cx.update(|cx| {
        let _ = view.update(cx, |view, cx| {
          {
            let delegate = view.delegate_mut();
            delegate.loading = false;
            match result {
              Ok(result) => {
                if let Some(statement) = result.statements.into_iter().next() {
                  delegate.eof = (statement.rows.len() as u32) < PAGE_SIZE;
                  delegate.columns = statement.columns;
                  delegate.rows = statement.rows;
                }
                delegate.status = format!(
                  "{schema}.{name} - {} rows - {:.0} ms",
                  delegate.rows.len(),
                  result.duration_ms
                )
                .into();
              }
              Err(error) => {
                delegate.eof = true;
                delegate.status = format!("error: {error}").into();
              }
            }
          }
          view.refresh(cx);
          cx.notify();
        });
      });
    });
  }
}

impl TableDelegate for RowsDelegate {
  fn columns_count(&self, _: &App) -> usize {
    self.display_columns().len()
  }

  fn rows_count(&self, _: &App) -> usize {
    self.rows.len() + self.staged.inserts.len()
  }

  fn column(&self, col_ix: usize, _: &App) -> Column {
    let col = &self.display_columns()[col_ix];
    let width = match col.data_type.as_deref() {
      Some("jsonb") | Some("json") | Some("text") => 320.,
      Some("timestamptz") | Some("timestamp") => 180.,
      _ if col_ix == 0 => 90.,
      _ => 160.,
    };
    let mut column = Column::new(col.name.clone(), col.name.clone()).width(px(width));
    // Server-side sort only exists while browsing a table.
    if self.browse.is_some() {
      column = match &self.sort {
        Some(sort) if sort.column == col.name => match sort.direction {
          SortDirection::Asc => column.sort(ColumnSort::Ascending),
          SortDirection::Desc => column.sort(ColumnSort::Descending),
        },
        _ => column.sortable(),
      };
    }
    column
  }

  fn render_td(
    &mut self,
    row_ix: usize,
    col_ix: usize,
    _: &mut Window,
    cx: &mut Context<TableState<Self>>,
  ) -> impl IntoElement {
    if self.editing == Some((row_ix, col_ix)) {
      return div()
        .key_context("CellEditor")
        .w_full()
        .child(Input::new(&self.editor).xsmall())
        .into_any_element();
    }

    let real_col = col_ix + self.hidden_lead();
    let slot = self.slot(row_ix);
    let is_insert = matches!(slot, RowSlot::Insert(_));
    let (deleted, dirty) = match slot {
      RowSlot::Insert(_) => (false, false),
      RowSlot::Data(data_ix) => (
        self.staged.deletes.contains(&data_ix),
        self
          .staged
          .edits
          .get(&data_ix)
          .is_some_and(|cells| cells.contains_key(&self.columns[real_col].name)),
      ),
    };
    let value = self.cell_value(row_ix, real_col);
    let missing_insert_value = is_insert && value.is_none();

    let text = match &value {
      Some(value) => value.clone(),
      None if missing_insert_value => "DEFAULT".to_string(),
      None => "NULL".to_string(),
    };

    div()
      .w_full()
      .when(deleted, |this| this.line_through().opacity(0.5))
      .when(dirty || is_insert, |this| this.text_color(cx.theme().blue))
      .when(value.is_none(), |this| this.opacity(0.6))
      .child(text)
      .into_any_element()
  }

  fn loading(&self, _: &App) -> bool {
    self.loading
  }

  fn has_more(&self, _: &App) -> bool {
    !self.eof && !self.loading && self.browse.is_some()
  }

  fn load_more_threshold(&self) -> usize {
    100
  }

  fn load_more(&mut self, _: &mut Window, cx: &mut Context<TableState<Self>>) {
    let Some((db, request)) = self.request(self.rows.len() as u32) else {
      return;
    };
    let (schema, name) = {
      let (_, schema, name) = self.browse.as_ref().unwrap();
      (schema.clone(), name.clone())
    };
    self.loading = true;
    let task = core::fetch_rows(&db, request, cx);

    self._load_task = cx.spawn(async move |view, cx| {
      let result = task.await;
      cx.update(|cx| {
        let _ = view.update(cx, |view, cx| {
          let delegate = view.delegate_mut();
          delegate.loading = false;
          match result {
            Ok(result) => {
              if let Some(statement) = result.statements.into_iter().next() {
                delegate.eof = (statement.rows.len() as u32) < PAGE_SIZE;
                delegate.rows.extend(statement.rows);
              } else {
                delegate.eof = true;
              }
              delegate.status =
                format!("{schema}.{name} - {} rows loaded", delegate.rows.len()).into();
            }
            Err(error) => {
              delegate.eof = true;
              delegate.status = format!("error: {error}").into();
            }
          }
          cx.notify();
        });
      });
    });
  }

  fn perform_sort(
    &mut self,
    col_ix: usize,
    sort: ColumnSort,
    _: &mut Window,
    cx: &mut Context<TableState<Self>>,
  ) {
    if self.browse.is_none() {
      return;
    }
    let Some(col) = self.display_columns().get(col_ix) else {
      return;
    };
    self.sort = match sort {
      ColumnSort::Ascending => Some(SortSpec {
        column: col.name.clone(),
        direction: SortDirection::Asc,
      }),
      ColumnSort::Descending => Some(SortSpec {
        column: col.name.clone(),
        direction: SortDirection::Desc,
      }),
      ColumnSort::Default => None,
    };
    self.reload(cx);
  }
}

#[cfg(test)]
mod tests {
  use gpui::{AppContext, TestAppContext};
  use gpui_component::input::InputState;
  use soquel_core::connectors::{ColumnKind, ForeignKeyInfo};

  use super::*;

  fn column(name: &str, data_type: &str, kind: ColumnKind) -> QueryColumn {
    QueryColumn {
      name: name.to_string(),
      data_type: Some(data_type.to_string()),
      kind,
    }
  }

  /// Two data rows of (id, name), editable, keyed on id.
  fn seeded(editor: Entity<InputState>) -> RowsDelegate {
    let mut delegate = RowsDelegate::new(editor);
    delegate.columns = vec![
      column("id", "int4", ColumnKind::Number),
      column("name", "text", ColumnKind::Text),
    ];
    delegate.rows = vec![
      vec![Some("1".into()), Some("Ada".into())],
      vec![Some("2".into()), Some("Alan".into())],
    ];
    delegate.can_ever_edit = true;
    delegate.key_columns = vec!["id".to_string()];
    delegate.loading = false;
    delegate
  }

  #[gpui::test]
  fn inserts_sit_on_top_and_indexes_follow(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let cx = cx.add_empty_window();
    cx.update(|window, cx| {
      let editor = cx.new(|cx| InputState::new(window, cx));
      let table = cx.new(|cx| TableState::new(seeded(editor), window, cx));
      table.update(cx, |table, cx| {
        table.delegate_mut().add_insert(None, cx);
        let delegate = table.delegate();
        // The insert renders first; data rows shift below it.
        assert_eq!(delegate.rows.len() + delegate.staged.inserts.len(), 3);
        assert_eq!(delegate.display_value(0, 0), None); // DEFAULT
        assert_eq!(delegate.display_value(1, 0), Some("1".to_string()));
        assert_eq!(delegate.display_value(2, 1), Some("Alan".to_string()));

        // Deleting the insert row removes it outright.
        table.delegate_mut().toggle_delete(0, cx);
        let delegate = table.delegate();
        assert!(delegate.staged.inserts.is_empty());
        assert!(delegate.staged.deletes.is_empty());

        // Deleting a data row stages it; the delete uses the DATA index.
        table.delegate_mut().add_insert(None, cx);
        table.delegate_mut().toggle_delete(2, cx);
        let delegate = table.delegate();
        assert!(delegate.staged.deletes.contains(&1));
      });
    });
  }

  #[gpui::test]
  fn duplicate_copies_the_data_row_not_the_insert_offset(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let cx = cx.add_empty_window();
    cx.update(|window, cx| {
      let editor = cx.new(|cx| InputState::new(window, cx));
      let table = cx.new(|cx| TableState::new(seeded(editor), window, cx));
      table.update(cx, |table, cx| {
        table.delegate_mut().add_insert(None, cx);
        // Display row 1 = data row 0 once an insert sits on top.
        table.delegate_mut().add_insert(Some(1), cx);
        let delegate = table.delegate();
        assert_eq!(
          delegate.staged.inserts[1].get("name"),
          Some(&Some("Ada".to_string()))
        );
      });
    });
  }

  #[gpui::test]
  fn commit_un_dirties_a_cell_returned_to_its_original(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let cx = cx.add_empty_window();
    cx.update(|window, cx| {
      let editor = cx.new(|cx| InputState::new(window, cx));
      let table = cx.new(|cx| TableState::new(seeded(editor.clone()), window, cx));
      table.update(cx, |table, cx| {
        table.delegate_mut().start_edit(0, 1, window, cx);
      });
      editor.update(cx, |editor, cx| editor.set_value("Ada II", window, cx));
      table.update(cx, |table, cx| {
        assert!(table.delegate_mut().commit_edit(cx));
        assert_eq!(
          table.delegate().staged.edits[&0].get("name"),
          Some(&Some("Ada II".to_string()))
        );
      });

      // Editing back to the original clears the dirty cell entirely.
      table.update(cx, |table, cx| {
        table.delegate_mut().start_edit(0, 1, window, cx);
      });
      editor.update(cx, |editor, cx| editor.set_value("Ada", window, cx));
      table.update(cx, |table, cx| {
        assert!(table.delegate_mut().commit_edit(cx));
        assert!(table.delegate().staged.edits.is_empty());
      });
    });
  }

  #[gpui::test]
  fn start_edit_refuses_deleted_rows_and_readonly_grids(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let cx = cx.add_empty_window();
    cx.update(|window, cx| {
      let editor = cx.new(|cx| InputState::new(window, cx));
      let table = cx.new(|cx| TableState::new(seeded(editor.clone()), window, cx));
      table.update(cx, |table, cx| {
        table.delegate_mut().toggle_delete(0, cx);
        table.delegate_mut().start_edit(0, 1, window, cx);
        assert!(table.delegate().editing.is_none());
      });

      let readonly = cx.new(|cx| {
        let mut delegate = seeded(editor);
        delegate.can_ever_edit = false;
        TableState::new(delegate, window, cx)
      });
      readonly.update(cx, |table, cx| {
        table.delegate_mut().start_edit(0, 1, window, cx);
        assert!(table.delegate().editing.is_none());
      });
    });
  }

  #[gpui::test]
  fn hidden_lead_and_xmin_keys(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let cx = cx.add_empty_window();
    cx.update(|window, cx| {
      let editor = cx.new(|cx| InputState::new(window, cx));
      let mut delegate = seeded(editor);
      // The driver prepends the system columns when asked for them.
      delegate
        .columns
        .insert(0, column("xmin", "xid", ColumnKind::Other));
      for (ix, row) in delegate.rows.iter_mut().enumerate() {
        row.insert(0, Some(format!("77{ix}")));
      }
      delegate.include_xmin = true;
      let table = cx.new(|cx| TableState::new(delegate, window, cx));
      let state = table.read(cx);
      let delegate = state.delegate();

      assert_eq!(delegate.hidden_lead(), 1);
      assert_eq!(delegate.display_columns().len(), 2);
      // Keys carry the pk plus the optimistic-lock guard.
      assert_eq!(
        delegate.change_keys(),
        vec!["id".to_string(), "xmin".to_string()]
      );
      // Display indexes skip the hidden lead.
      assert_eq!(delegate.display_value(0, 0), Some("1".to_string()));
    });
  }

  #[gpui::test]
  fn fk_lookup_matches_columns(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let cx = cx.add_empty_window();
    cx.update(|window, cx| {
      let editor = cx.new(|cx| InputState::new(window, cx));
      let mut delegate = seeded(editor);
      delegate.foreign_keys = vec![ForeignKeyInfo {
        name: "fk_org".into(),
        columns: vec!["id".into()],
        referenced_schema: "app".into(),
        referenced_table: "organizations".into(),
        referenced_columns: vec!["id".into()],
      }];
      let table = cx.new(|cx| TableState::new(delegate, window, cx));
      let state = table.read(cx);
      assert!(state.delegate().fk_for("id").is_some());
      assert!(state.delegate().fk_for("name").is_none());
    });
  }
}
