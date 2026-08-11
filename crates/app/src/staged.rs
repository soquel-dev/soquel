use std::collections::{BTreeMap, BTreeSet};

use soquel_core::connectors::{
  CellValue, QueryColumn, RowDelete, RowInsert, RowUpdate, TableChanges,
};

/// Cell values: Some = provided, None = NULL; an absent insert key = column DEFAULT.
#[derive(Debug, Clone, Default)]
pub struct StagedChanges {
  pub edits: BTreeMap<usize, BTreeMap<String, Option<String>>>,
  pub deletes: BTreeSet<usize>,
  pub inserts: Vec<BTreeMap<String, Option<String>>>,
}

impl StagedChanges {
  pub fn clear(&mut self) {
    self.edits.clear();
    self.deletes.clear();
    self.inserts.clear();
  }

  /// Operations, not touched cells: a deleted row collapses its edits.
  pub fn count(&self) -> usize {
    let edits = self
      .edits
      .keys()
      .filter(|row| !self.deletes.contains(row))
      .count();
    edits + self.deletes.len() + self.inserts.len()
  }

  pub fn is_empty(&self) -> bool {
    self.count() == 0
  }
}

pub fn build_table_changes(
  staged: &StagedChanges,
  rows: &[Vec<Option<String>>],
  columns: &[QueryColumn],
  key_columns: &[String],
  schema: &str,
  table: &str,
) -> TableChanges {
  let index_of: BTreeMap<&str, usize> = columns
    .iter()
    .enumerate()
    .map(|(index, column)| (column.name.as_str(), index))
    .collect();
  // Keys always carry the ORIGINAL row values, even when the key column itself was edited.
  let key_of = |row: usize| -> Vec<CellValue> {
    key_columns
      .iter()
      .map(|column| CellValue {
        column: column.clone(),
        value: index_of
          .get(column.as_str())
          .and_then(|ix| rows.get(row).and_then(|r| r.get(*ix)).cloned())
          .flatten(),
      })
      .collect()
  };

  TableChanges {
    schema: schema.to_string(),
    table: table.to_string(),
    updates: staged
      .edits
      .iter()
      .filter(|(row, _)| !staged.deletes.contains(row))
      .map(|(row, cells)| RowUpdate {
        key: key_of(*row),
        set: cells
          .iter()
          .map(|(column, value)| CellValue {
            column: column.clone(),
            value: value.clone(),
          })
          .collect(),
      })
      .collect(),
    inserts: staged
      .inserts
      .iter()
      .map(|values| RowInsert {
        values: values
          .iter()
          .map(|(column, value)| CellValue {
            column: column.clone(),
            value: value.clone(),
          })
          .collect(),
      })
      .collect(),
    deletes: staged
      .deletes
      .iter()
      .map(|row| RowDelete { key: key_of(*row) })
      .collect(),
  }
}

fn literal(value: &Option<String>) -> String {
  match value {
    None => "NULL".to_string(),
    Some(value) => format!("'{}'", value.replace('\'', "''")),
  }
}

fn ident(name: &str) -> String {
  format!("\"{}\"", name.replace('"', "\"\""))
}

/// Display only: execution binds every value as a parameter.
pub fn preview_sql(changes: &TableChanges) -> Vec<String> {
  let target = format!("{}.{}", ident(&changes.schema), ident(&changes.table));
  let where_of = |key: &[CellValue]| -> String {
    key
      .iter()
      .map(|cell| match &cell.value {
        None => format!("{} IS NULL", ident(&cell.column)),
        Some(_) => format!("{} = {}", ident(&cell.column), literal(&cell.value)),
      })
      .collect::<Vec<_>>()
      .join(" AND ")
  };

  changes
    .updates
    .iter()
    .map(|update| {
      let set = update
        .set
        .iter()
        .map(|cell| format!("{} = {}", ident(&cell.column), literal(&cell.value)))
        .collect::<Vec<_>>()
        .join(", ");
      format!("UPDATE {target} SET {set} WHERE {};", where_of(&update.key))
    })
    .chain(
      changes
        .deletes
        .iter()
        .map(|remove| format!("DELETE FROM {target} WHERE {};", where_of(&remove.key))),
    )
    .chain(changes.inserts.iter().map(|insert| {
      if insert.values.is_empty() {
        format!("INSERT INTO {target} DEFAULT VALUES;")
      } else {
        let columns = insert
          .values
          .iter()
          .map(|cell| ident(&cell.column))
          .collect::<Vec<_>>()
          .join(", ");
        let values = insert
          .values
          .iter()
          .map(|cell| literal(&cell.value))
          .collect::<Vec<_>>()
          .join(", ");
        format!("INSERT INTO {target} ({columns}) VALUES ({values});")
      }
    }))
    .collect()
}

#[cfg(test)]
mod tests {
  use soquel_core::connectors::ColumnKind;

  use super::*;

  fn columns() -> Vec<QueryColumn> {
    vec![
      QueryColumn {
        name: "id".into(),
        data_type: Some("int4".into()),
        kind: ColumnKind::Number,
      },
      QueryColumn {
        name: "name".into(),
        data_type: Some("text".into()),
        kind: ColumnKind::Text,
      },
    ]
  }

  fn rows() -> Vec<Vec<Option<String>>> {
    vec![
      vec![Some("1".into()), Some("Ada".into())],
      vec![Some("2".into()), Some("Alan".into())],
    ]
  }

  #[test]
  fn keys_updates_on_original_values_even_when_the_key_column_was_edited() {
    let mut staged = StagedChanges::default();
    staged.edits.insert(
      0,
      BTreeMap::from([
        ("id".to_string(), Some("10".to_string())),
        ("name".to_string(), Some("Ada II".to_string())),
      ]),
    );
    let changes = build_table_changes(
      &staged,
      &rows(),
      &columns(),
      &["id".into()],
      "app",
      "customers",
    );
    assert_eq!(changes.updates.len(), 1);
    assert_eq!(changes.updates[0].key[0].column, "id");
    assert_eq!(changes.updates[0].key[0].value, Some("1".to_string()));
    assert_eq!(changes.updates[0].set.len(), 2);
  }

  #[test]
  fn drops_the_edit_when_the_row_is_also_deleted() {
    let mut staged = StagedChanges::default();
    staged.edits.insert(
      1,
      BTreeMap::from([("name".to_string(), Some("x".to_string()))]),
    );
    staged.deletes.insert(1);
    let changes = build_table_changes(
      &staged,
      &rows(),
      &columns(),
      &["id".into()],
      "app",
      "customers",
    );
    assert!(changes.updates.is_empty());
    assert_eq!(changes.deletes.len(), 1);
    assert_eq!(changes.deletes[0].key[0].value, Some("2".to_string()));
  }

  #[test]
  fn keeps_only_provided_insert_columns_so_the_rest_take_default() {
    let mut staged = StagedChanges::default();
    staged.inserts.push(BTreeMap::from([
      ("name".to_string(), Some("New".to_string())),
      ("id".to_string(), None),
    ]));
    let changes = build_table_changes(
      &staged,
      &rows(),
      &columns(),
      &["id".into()],
      "app",
      "customers",
    );
    assert_eq!(changes.inserts.len(), 1);
    assert_eq!(changes.inserts[0].values.len(), 2);
  }

  #[test]
  fn counts_operations_not_touched_cells() {
    let mut staged = StagedChanges::default();
    staged.edits.insert(
      0,
      BTreeMap::from([("name".to_string(), Some("x".to_string()))]),
    );
    staged.edits.insert(
      1,
      BTreeMap::from([("name".to_string(), Some("y".to_string()))]),
    );
    staged.deletes.insert(1);
    staged.inserts.push(BTreeMap::new());
    // edit row 0, delete row 1 (its edit collapses), one insert
    assert_eq!(staged.count(), 3);
  }

  #[test]
  fn renders_quoted_display_statements() {
    let changes = TableChanges {
      schema: "app".into(),
      table: "customers".into(),
      updates: vec![RowUpdate {
        key: vec![CellValue {
          column: "id".into(),
          value: Some("1".into()),
        }],
        set: vec![CellValue {
          column: "name".into(),
          value: Some("O'Brien".into()),
        }],
      }],
      deletes: vec![RowDelete {
        key: vec![CellValue {
          column: "note".into(),
          value: None,
        }],
      }],
      inserts: vec![RowInsert { values: vec![] }],
    };
    assert_eq!(
      preview_sql(&changes),
      vec![
        "UPDATE \"app\".\"customers\" SET \"name\" = 'O''Brien' WHERE \"id\" = '1';",
        "DELETE FROM \"app\".\"customers\" WHERE \"note\" IS NULL;",
        "INSERT INTO \"app\".\"customers\" DEFAULT VALUES;",
      ]
    );
  }
}
