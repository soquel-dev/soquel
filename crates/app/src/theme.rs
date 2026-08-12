use gpui::{App, Hsla, SharedString, Window, WindowAppearance};
use gpui_component::{ActiveTheme, Theme, ThemeMode, ThemeRegistry};

/// The mono family from the theme; the app never hardcodes the font name.
pub fn mono(cx: &App) -> SharedString {
  cx.theme().mono_font_family.clone()
}

/// Screen background; panels and cards sit on it.
pub fn canvas(cx: &App) -> Hsla {
  if cx.theme().mode.is_dark() {
    cx.theme().background
  } else {
    cx.theme().secondary
  }
}

/// Elevated surface: cards, dialogs, sidebars.
pub fn panel(cx: &App) -> Hsla {
  if cx.theme().mode.is_dark() {
    cx.theme().popover
  } else {
    cx.theme().background
  }
}

pub fn init(cx: &mut App) {
  ThemeRegistry::global_mut(cx)
    .load_themes_from_str(include_str!("../themes/soquel.json"))
    .expect("themes/soquel.json is valid");
  let mode = match cx.window_appearance() {
    WindowAppearance::Dark | WindowAppearance::VibrantDark => ThemeMode::Dark,
    WindowAppearance::Light | WindowAppearance::VibrantLight => ThemeMode::Light,
  };
  apply(mode, None, cx);
}

pub fn toggle(window: &mut Window, cx: &mut App) {
  let mode = if cx.theme().mode.is_dark() {
    ThemeMode::Light
  } else {
    ThemeMode::Dark
  };
  apply(mode, Some(window), cx);
}

fn apply(mode: ThemeMode, window: Option<&mut Window>, cx: &mut App) {
  let name = match mode {
    ThemeMode::Dark => "Soquel Dark",
    ThemeMode::Light => "Soquel Light",
  };
  if let Some(config) = ThemeRegistry::global(cx).themes().get(name).cloned() {
    Theme::global_mut(cx).apply_config(&config);
  }
  Theme::change(mode, window, cx);
}
