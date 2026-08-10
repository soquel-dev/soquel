mod actions;
mod cell_editing;
mod completion;
mod core;
mod explain;
mod export;
mod filters;
mod format;
mod grid;
mod history;
mod icons;
mod staged;
mod tabs;
mod theme;
mod workspace;

use gpui::*;
use gpui_component::{Root, TitleBar};

use crate::workspace::Workspace;

fn main() {
  // Without the asset source, every Icon (sort chevrons, titlebar, chips) is invisible.
  gpui_platform::application()
    .with_assets(crate::icons::Assets)
    .run(move |cx| {
      gpui_component::init(cx);
      theme::init(cx);
      actions::init(cx);

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
