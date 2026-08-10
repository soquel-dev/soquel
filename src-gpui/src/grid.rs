use gpui::{App, Context, IntoElement, SharedString, Task, Window, px};
use gpui_component::table::{Column, ColumnSort, TableDelegate, TableState};
use soquel_core::connectors::{ColumnFilter, QueryColumn, SortDirection, SortSpec};

use crate::core::{self, Db};

pub const PAGE_SIZE: u32 = 500;

pub struct RowsDelegate {
  /// Set while browsing a table: load_more pages it. Query results leave it None.
  pub browse: Option<(Db, String, String)>,
  pub columns: Vec<QueryColumn>,
  pub rows: Vec<Vec<Option<String>>>,
  pub sort: Option<SortSpec>,
  pub filters: Vec<ColumnFilter>,
  pub loading: bool,
  pub eof: bool,
  pub status: SharedString,
  _load_task: Task<()>,
}

impl RowsDelegate {
  pub fn new() -> Self {
    Self {
      browse: None,
      columns: Vec::new(),
      rows: Vec::new(),
      sort: None,
      filters: Vec::new(),
      loading: true,
      eof: false,
      status: "connecting...".into(),
      _load_task: Task::ready(()),
    }
  }

  fn request(&self, offset: u32) -> Option<(Db, soquel_core::connectors::TableRowsRequest)> {
    let (db, schema, name) = self.browse.clone()?;
    let mut request = core::page_request(&schema, &name, offset, PAGE_SIZE);
    request.sort = self.sort.clone();
    request.filters = self.filters.clone();
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
    let rx = core::fetch_rows(&db, request);

    self._load_task = cx.spawn(async move |view, cx| {
      let result = rx.await;
      cx.update(|cx| {
        let _ = view.update(cx, |view, cx| {
          {
            let delegate = view.delegate_mut();
            delegate.loading = false;
            match result {
              Ok(Ok(result)) => {
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
              Ok(Err(error)) => {
                delegate.eof = true;
                delegate.status = format!("error: {error}").into();
              }
              Err(_) => {
                delegate.eof = true;
                delegate.status = "error: fetch canceled".into();
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
    self.columns.len()
  }

  fn rows_count(&self, _: &App) -> usize {
    self.rows.len()
  }

  fn column(&self, col_ix: usize, _: &App) -> Column {
    let col = &self.columns[col_ix];
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
    _: &mut Context<TableState<Self>>,
  ) -> impl IntoElement {
    match &self.rows[row_ix][col_ix] {
      Some(value) => value.clone(),
      None => "NULL".to_string(),
    }
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
    let rx = core::fetch_rows(&db, request);

    self._load_task = cx.spawn(async move |view, cx| {
      let result = rx.await;
      cx.update(|cx| {
        let _ = view.update(cx, |view, cx| {
          let delegate = view.delegate_mut();
          delegate.loading = false;
          match result {
            Ok(Ok(result)) => {
              if let Some(statement) = result.statements.into_iter().next() {
                delegate.eof = (statement.rows.len() as u32) < PAGE_SIZE;
                delegate.rows.extend(statement.rows);
              } else {
                delegate.eof = true;
              }
              delegate.status =
                format!("{schema}.{name} - {} rows loaded", delegate.rows.len()).into();
            }
            Ok(Err(error)) => {
              delegate.eof = true;
              delegate.status = format!("error: {error}").into();
            }
            Err(_) => {
              delegate.eof = true;
              delegate.status = "error: fetch canceled".into();
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
    let Some(col) = self.columns.get(col_ix) else {
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
