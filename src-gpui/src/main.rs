#![recursion_limit = "256"]
mod actions;
mod app;
mod cell_editing;
mod command_approval;
mod completion;
mod connections;
mod core;
mod explain;
mod export;
mod filters;
mod format;
mod grid;
mod history;
mod host_key;
mod icons;
mod staged;
mod tabs;
#[cfg(test)]
mod test_support;
mod theme;
mod tunnels;
mod workspace;

use gpui::*;
use gpui_component::{Root, TitleBar};

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
            let state = crate::core::init_state().expect("app state loads");
            let view = cx.new(|cx| crate::app::App::new(state, window, cx));
            cx.new(|cx| Root::new(view, window, cx))
          },
        )
        .expect("failed to open window");
      })
      .detach();
    });
}
