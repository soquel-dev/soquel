use gpui::{App, KeyBinding, actions};

actions!(
  soquel,
  [
    RunQuery,
    ToggleThemeMode,
    RefreshSchema,
    FocusEditor,
    CancelCellEdit,
    NextCell,
    PrevCell,
    NextTab,
    PrevTab,
    NewSqlTab
  ]
);

pub fn init(cx: &mut App) {
  cx.bind_keys([
    // After gpui_component::init so it shadows the editor's own secondary-enter
    // (which would insert a newline in multi-line mode).
    KeyBinding::new("secondary-enter", RunQuery, Some("Input")),
    KeyBinding::new("secondary-e", FocusEditor, None),
    KeyBinding::new("secondary-r", RefreshSchema, None),
    KeyBinding::new("ctrl-tab", NextTab, None),
    KeyBinding::new("ctrl-shift-tab", PrevTab, None),
    KeyBinding::new("secondary-t", NewSqlTab, None),
    // Scoped to the cell editor wrapper so the sql editor's own keys survive.
    KeyBinding::new("escape", CancelCellEdit, Some("CellEditor > Input")),
    KeyBinding::new("tab", NextCell, Some("CellEditor > Input")),
    KeyBinding::new("shift-tab", PrevCell, Some("CellEditor > Input")),
  ]);
}
