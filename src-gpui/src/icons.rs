use std::borrow::Cow;

use gpui::{AssetSource, IntoElement, Result, SharedString};
use gpui_component::{IconNamed, icon_named};

// Lucide icons the shipped gpui-component set is missing.
icon_named!(SoquelIcon, "assets/icons");

impl gpui::RenderOnce for SoquelIcon {
  fn render(self, _: &mut gpui::Window, _: &mut gpui::App) -> impl IntoElement {
    gpui_component::Icon::new(self)
  }
}

#[derive(rust_embed::RustEmbed)]
#[folder = "assets"]
#[include = "icons/**/*.svg"]
struct Embedded;

/// Our icons first, gpui-component's shipped set as the fallback.
pub struct Assets;

impl AssetSource for Assets {
  fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
    if let Some(file) = Embedded::get(path) {
      return Ok(Some(file.data));
    }
    gpui_component_assets::Assets.load(path)
  }

  fn list(&self, path: &str) -> Result<Vec<SharedString>> {
    let mut paths: Vec<SharedString> = Embedded::iter()
      .filter(|p| p.starts_with(path))
      .map(Into::into)
      .collect();
    paths.extend(gpui_component_assets::Assets.list(path)?);
    Ok(paths)
  }
}
