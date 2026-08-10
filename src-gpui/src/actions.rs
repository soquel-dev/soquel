use gpui::{App, KeyBinding, actions};

actions!(
  soquel,
  [RunQuery, ToggleThemeMode, RefreshSchema, FocusEditor]
);

pub fn init(cx: &mut App) {
  cx.bind_keys([
    // After gpui_component::init so it shadows the editor's own secondary-enter
    // (which would insert a newline in multi-line mode).
    KeyBinding::new("secondary-enter", RunQuery, Some("Input")),
    KeyBinding::new("secondary-e", FocusEditor, None),
    KeyBinding::new("secondary-r", RefreshSchema, None),
  ]);
}
