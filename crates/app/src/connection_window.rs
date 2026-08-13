//! The root view of a per-connection window: one workspace picked by connector
//! capability, wrapped in the shared chrome.

use std::rc::Rc;
use std::sync::Arc;

use gpui::*;
use gpui_component::{Icon, IconName, Root, TitleBar, h_flex, v_flex};
use soquel_core::AppState;
use soquel_core::profiles::ConnectionProfile;

use crate::actions::ToggleCommandPalette;
use crate::command_palette::{PaletteItem, PaletteSection};
use crate::core;
use crate::doc::DocWorkspace;
use crate::kv::KvWorkspace;
use crate::theme;
use crate::workspace::Workspace;

pub enum WorkspaceKind {
  Sql(Entity<Workspace>),
  Kv(Entity<KvWorkspace>),
  Doc(Entity<DocWorkspace>),
}

pub struct ConnectionWindow {
  state: Arc<AppState>,
  connection_name: SharedString,
  workspace: WorkspaceKind,
  focus_handle: FocusHandle,
}

impl ConnectionWindow {
  pub fn new(
    state: Arc<AppState>,
    db: core::Db,
    profile: ConnectionProfile,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) -> Self {
    let connection_name: SharedString = profile.name.clone().into();
    // The browsers branch on kind: keys -> redis, documents -> mongo,
    // everything else the SQL workspace.
    let caps = soquel_core::connectors::connector_for(db.kind()).capabilities();
    let workspace = if caps.contains(&soquel_core::connectors::Capability::KvBrowse) {
      WorkspaceKind::Kv(cx.new(|cx| KvWorkspace::new(state.clone(), db, profile, window, cx)))
    } else if caps.contains(&soquel_core::connectors::Capability::DocBrowse) {
      WorkspaceKind::Doc(cx.new(|cx| DocWorkspace::new(state.clone(), db, profile, window, cx)))
    } else {
      let data_dir = state.data_dir.clone();
      WorkspaceKind::Sql(cx.new(|cx| Workspace::new(db, profile, data_dir, window, cx)))
    };
    // No focus grab here: the workspaces focus themselves. The handle is only
    // the key fallback so cmd-k still lands once focus drifts.
    Self {
      state,
      connection_name,
      workspace,
      focus_handle: cx.focus_handle(),
    }
  }

  fn palette_items(&self, cx: &mut Context<Self>) -> Vec<PaletteItem> {
    let mut items = crate::chrome::app_palette_items(self.state.clone(), cx);
    if let WorkspaceKind::Sql(view) = &self.workspace {
      let run_view = view.downgrade();
      items.push(PaletteItem {
        label: "Run query".into(),
        hint: None,
        keywords: "run query execute".to_string(),
        icon: Icon::new(IconName::Play),
        section: PaletteSection::Actions,
        run: Rc::new(move |_, cx| {
          run_view.update(cx, |view, cx| view.run(cx)).ok();
        }),
      });
      let sql_view = view.downgrade();
      items.push(PaletteItem {
        label: "New SQL tab".into(),
        hint: None,
        keywords: "new sql tab".to_string(),
        icon: Icon::new(IconName::Plus),
        section: PaletteSection::Actions,
        run: Rc::new(move |window, cx| {
          sql_view
            .update(cx, |view, cx| view.open_sql(window, cx))
            .ok();
        }),
      });
      let refresh_view = view.downgrade();
      items.push(PaletteItem {
        label: "Refresh schema".into(),
        hint: None,
        keywords: "refresh schema reload".to_string(),
        icon: Icon::new(IconName::Replace),
        section: PaletteSection::Actions,
        run: Rc::new(move |_, cx| {
          refresh_view
            .update(cx, |view, cx| view.refresh_schema(cx))
            .ok();
        }),
      });
      let focus_view = view.downgrade();
      items.push(PaletteItem {
        label: "Focus editor".into(),
        hint: None,
        keywords: "focus editor sql".to_string(),
        icon: Icon::new(IconName::SquareTerminal),
        section: PaletteSection::Actions,
        run: Rc::new(move |window, cx| {
          focus_view
            .update(cx, |view, cx| view.focus_editor(window, cx))
            .ok();
        }),
      });
      let next_view = view.downgrade();
      items.push(PaletteItem {
        label: "Next tab".into(),
        hint: None,
        keywords: "next tab".to_string(),
        icon: Icon::new(IconName::ArrowRight),
        section: PaletteSection::Actions,
        run: Rc::new(move |_, cx| {
          next_view.update(cx, |view, cx| view.cycle(1, cx)).ok();
        }),
      });
      let prev_view = view.downgrade();
      items.push(PaletteItem {
        label: "Previous tab".into(),
        hint: None,
        keywords: "previous tab".to_string(),
        icon: Icon::new(IconName::ArrowLeft),
        section: PaletteSection::Actions,
        run: Rc::new(move |_, cx| {
          prev_view.update(cx, |view, cx| view.cycle(-1, cx)).ok();
        }),
      });
    }
    items.push(PaletteItem {
      label: "Show connections".into(),
      hint: None,
      keywords: "connections hub show back".to_string(),
      icon: Icon::new(IconName::ArrowLeft),
      section: PaletteSection::Actions,
      run: Rc::new(|_, cx| crate::windows::focus_or_open_hub(cx)),
    });
    items
  }

  fn open_command_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
    let items = self.palette_items(cx);
    crate::chrome::open_command_palette(items, window, cx);
  }
}

impl Render for ConnectionWindow {
  fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    // Root does not render these itself: without them dialogs and toasts are silent no-ops.
    let dialog_layer = Root::render_dialog_layer(window, cx);
    let notification_layer = Root::render_notification_layer(window, cx);
    let (connection, status) = match &self.workspace {
      WorkspaceKind::Sql(view) => {
        let view = view.read(cx);
        (Some(view.footer_connection()), view.footer_status(cx))
      }
      WorkspaceKind::Kv(view) => (
        Some(view.read(cx).footer_connection()),
        SharedString::default(),
      ),
      WorkspaceKind::Doc(view) => (
        Some(view.read(cx).footer_connection()),
        SharedString::default(),
      ),
    };

    v_flex()
      .size_full()
      .key_context("App")
      .track_focus(&self.focus_handle)
      .on_action(cx.listener(|this, _: &ToggleCommandPalette, window, cx| {
        this.open_command_palette(window, cx)
      }))
      .bg(theme::canvas(cx))
      .child(TitleBar::new().child(h_flex().child(self.connection_name.clone())))
      .child(div().flex_1().min_h_0().child(match &self.workspace {
        WorkspaceKind::Sql(view) => view.clone().into_any_element(),
        WorkspaceKind::Kv(view) => view.clone().into_any_element(),
        WorkspaceKind::Doc(view) => view.clone().into_any_element(),
      }))
      .child(crate::chrome::footer(
        self.state.clone(),
        connection,
        status,
        window,
        cx,
      ))
      .children(dialog_layer)
      .children(notification_layer)
  }
}
