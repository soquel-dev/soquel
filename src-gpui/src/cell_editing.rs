use soquel_core::connectors::{ColumnKind, QueryColumn};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorMode {
  Bool,
  Date,
  Json,
  Text,
}

/// Sentinel value: a real bool cell can never collide with it.
pub const NULL_OPTION: &str = "__null__";

pub fn editor_mode(column: &QueryColumn) -> EditorMode {
  if column.kind == ColumnKind::Bool {
    return EditorMode::Bool;
  }
  // Only plain dates: a datetime editor would drop timezone and microseconds.
  if column.data_type.as_deref() == Some("date") {
    return EditorMode::Date;
  }
  if column.kind == ColumnKind::Json {
    return EditorMode::Json;
  }
  EditorMode::Text
}

pub fn initial_editor_value(mode: EditorMode, initial: Option<&str>) -> String {
  if mode == EditorMode::Bool && initial.is_none() {
    return NULL_OPTION.to_string();
  }
  initial.unwrap_or("").to_string()
}

pub fn staged_value(mode: EditorMode, value: &str) -> Option<String> {
  if mode == EditorMode::Bool {
    return (value != NULL_OPTION).then(|| value.to_string());
  }
  // A cleared date reads as NULL: '' can never cast to date anyway.
  if mode == EditorMode::Date && value.is_empty() {
    return None;
  }
  Some(value.to_string())
}

/// Gate for staging/navigation: invalid JSON must never reach the staging area.
pub fn editor_value_valid(mode: EditorMode, value: &str) -> bool {
  if mode != EditorMode::Json || value.trim().is_empty() {
    return true;
  }
  serde_json::from_str::<serde_json::Value>(value).is_ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellPosition {
  pub row_index: usize,
  pub position: usize,
}

/// Tab-order neighbor among editable columns, wrapping across rows;
/// None when stepping past either end of the loaded rows.
pub fn next_editable_position(
  current: CellPosition,
  direction: i32,
  column_count: usize,
  row_count: usize,
) -> Option<CellPosition> {
  if column_count == 0 {
    return None;
  }
  let mut position = current.position as i64 + direction as i64;
  let mut row_index = current.row_index as i64;
  if position < 0 {
    position = column_count as i64 - 1;
    row_index -= 1;
  } else if position >= column_count as i64 {
    position = 0;
    row_index += 1;
  }
  if row_index < 0 || row_index >= row_count as i64 {
    return None;
  }
  Some(CellPosition {
    row_index: row_index as usize,
    position: position as usize,
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  fn column(kind: ColumnKind, data_type: Option<&str>) -> QueryColumn {
    QueryColumn {
      name: "c".into(),
      data_type: data_type.map(Into::into),
      kind,
    }
  }

  #[test]
  fn picks_the_editor_from_kind_and_exact_data_type() {
    assert_eq!(
      editor_mode(&column(ColumnKind::Bool, Some("bool"))),
      EditorMode::Bool
    );
    assert_eq!(
      editor_mode(&column(ColumnKind::DateTime, Some("date"))),
      EditorMode::Date
    );
    // Timestamps stay text: a datetime editor would mangle tz and precision.
    assert_eq!(
      editor_mode(&column(ColumnKind::DateTime, Some("timestamptz"))),
      EditorMode::Text
    );
    assert_eq!(
      editor_mode(&column(ColumnKind::Json, Some("jsonb"))),
      EditorMode::Json
    );
    assert_eq!(
      editor_mode(&column(ColumnKind::Number, Some("int4"))),
      EditorMode::Text
    );
    assert_eq!(
      editor_mode(&column(ColumnKind::Text, None)),
      EditorMode::Text
    );
  }

  #[test]
  fn maps_null_through_the_bool_sentinel() {
    assert_eq!(initial_editor_value(EditorMode::Bool, None), NULL_OPTION);
    assert_eq!(staged_value(EditorMode::Bool, NULL_OPTION), None);
    assert_eq!(initial_editor_value(EditorMode::Bool, Some("true")), "true");
    assert_eq!(
      staged_value(EditorMode::Bool, "false"),
      Some("false".to_string())
    );
  }

  #[test]
  fn reads_a_cleared_date_as_null_but_keeps_empty_text_as_empty_string() {
    assert_eq!(staged_value(EditorMode::Date, ""), None);
    assert_eq!(
      staged_value(EditorMode::Date, "2026-07-30"),
      Some("2026-07-30".to_string())
    );
    assert_eq!(staged_value(EditorMode::Text, ""), Some(String::new()));
    assert_eq!(initial_editor_value(EditorMode::Text, None), "");
  }

  #[test]
  fn gates_only_json_and_lets_empty_pass() {
    assert!(editor_value_valid(EditorMode::Json, "{\"a\": 1}"));
    assert!(!editor_value_valid(EditorMode::Json, "{a: 1}"));
    assert!(editor_value_valid(EditorMode::Json, "  "));
    assert!(editor_value_valid(EditorMode::Text, "{a: 1}"));
  }

  #[test]
  fn moves_within_a_row_and_wraps_across_rows() {
    let step = |row_index, position, direction| {
      next_editable_position(
        CellPosition {
          row_index,
          position,
        },
        direction,
        3,
        2,
      )
    };
    assert_eq!(
      step(0, 0, 1),
      Some(CellPosition {
        row_index: 0,
        position: 1
      })
    );
    assert_eq!(
      step(0, 2, 1),
      Some(CellPosition {
        row_index: 1,
        position: 0
      })
    );
    assert_eq!(
      step(1, 0, -1),
      Some(CellPosition {
        row_index: 0,
        position: 2
      })
    );
  }

  #[test]
  fn stops_past_either_end_and_on_empty_columns() {
    let step = |row_index, position, direction| {
      next_editable_position(
        CellPosition {
          row_index,
          position,
        },
        direction,
        3,
        2,
      )
    };
    assert_eq!(step(0, 0, -1), None);
    assert_eq!(step(1, 2, 1), None);
    assert_eq!(
      next_editable_position(
        CellPosition {
          row_index: 0,
          position: 0
        },
        1,
        0,
        2
      ),
      None
    );
  }
}
