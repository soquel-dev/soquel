use gpui::{App, Context, IntoElement, SharedString, Task, Window, px};
use gpui_component::table::{Column, TableDelegate, TableState};
use soquel_core::connectors::QueryColumn;

use crate::core::{self, Db};

pub const PAGE_SIZE: u32 = 500;

pub struct RowsDelegate {
  pub schema: String,
  pub table: String,
  pub db: Option<Db>,
  pub columns: Vec<QueryColumn>,
  pub rows: Vec<Vec<Option<String>>>,
  pub loading: bool,
  pub eof: bool,
  pub status: SharedString,
  _load_task: Task<()>,
}

impl RowsDelegate {
  pub fn new(schema: String, table: String) -> Self {
    Self {
      schema,
      table,
      db: None,
      columns: Vec::new(),
      rows: Vec::new(),
      loading: true,
      eof: false,
      status: "connecting...".into(),
      _load_task: Task::ready(()),
    }
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
    Column::new(col.name.clone(), col.name.clone()).width(px(width))
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
    !self.eof && !self.loading && self.db.is_some()
  }

  fn load_more_threshold(&self) -> usize {
    100
  }

  fn load_more(&mut self, _: &mut Window, cx: &mut Context<TableState<Self>>) {
    let Some(db) = self.db.clone() else {
      return;
    };
    self.loading = true;
    let request = core::page_request(&self.schema, &self.table, self.rows.len() as u32, PAGE_SIZE);
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
              delegate.status = format!(
                "{}.{} - {} rows loaded",
                delegate.schema,
                delegate.table,
                delegate.rows.len()
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
          cx.notify();
        });
      });
    });
  }
}
