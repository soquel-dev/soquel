mod core;
mod grid;

use gpui::*;
use gpui_component::table::{DataTable, TableState};
use gpui_component::{ActiveTheme, Root, TitleBar, h_flex, v_flex};

use crate::grid::{PAGE_SIZE, RowsDelegate};

pub struct Workspace {
  table: Entity<TableState<RowsDelegate>>,
  _connect_task: Task<()>,
}

impl Workspace {
  fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
    let target = std::env::var("SOQUEL_GPUI_TABLE").unwrap_or_else(|_| "app.events".into());
    let (schema, table_name) = target.split_once('.').unwrap_or(("public", &target));

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

    let connect_task = {
      let table = table.clone();
      cx.spawn(async move |_, cx| {
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

        let request = {
          let (schema, name) = table.read_with(cx, |table, _| {
            let d = table.delegate();
            (d.schema.clone(), d.table.clone())
          });
          core::page_request(&schema, &name, 0, PAGE_SIZE)
        };
        let first_page = core::fetch_rows(&db, request).await;

        table.update(cx, |table, cx| {
          let delegate = table.delegate_mut();
          delegate.db = Some(db);
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
          cx.notify();
        });
      })
    };

    Self {
      table,
      _connect_task: connect_task,
    }
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
          .border_b_1()
          .border_color(cx.theme().border)
          .text_sm()
          .text_color(cx.theme().muted_foreground)
          .child(status),
      )
      .child(
        v_flex()
          .flex_1()
          .min_h_0()
          .p_2()
          .child(DataTable::new(&self.table).stripe(true)),
      )
  }
}

fn main() {
  gpui_platform::application().run(move |cx| {
    gpui_component::init(cx);

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
