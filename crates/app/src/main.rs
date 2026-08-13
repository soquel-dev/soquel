#![recursion_limit = "256"]
mod actions;
mod app;
mod cell_editing;
mod chrome;
mod command_approval;
mod command_palette;
mod completion;
mod connection_window;
mod connections;
mod core;
mod diagnostics;
mod dialogs;
mod doc;
mod explain;
mod export;
mod filters;
mod format;
mod grid;
mod history;
mod host_key;
mod icons;
mod kv;
mod licence;
mod mcp;
mod mcp_approval;
mod staged;
mod status;
mod tabs;
#[cfg(test)]
mod test_support;
mod theme;
mod transfer;
mod tunnels;
mod ui;
mod windows;
mod workspace;

fn main() {
  // Before anything logs: the keyring probe in init_state is the first line worth
  // capturing.
  crate::core::init_logging();
  // Without the asset source, every Icon (sort chevrons, titlebar, chips) is invisible.
  let app = gpui_platform::application().with_assets(crate::icons::Assets);
  // macOS dock reactivation with every window closed brings the hub back.
  app.on_reopen(crate::windows::reopen_hub_if_none);
  app.run(move |cx| {
    gpui_component::init(cx);
    theme::init(cx);
    actions::init(cx);

    let state = crate::core::init_state().expect("app state loads");
    crate::mcp::init_coordinator(state.clone(), cx);
    crate::windows::init(state, cx);
    crate::windows::open_hub_window(cx);
  });
}
