//! SQL builders for the browse/edit surfaces: `?` placeholders, backtick
//! idents, server-side coercion instead of explicit casts.

use crate::connectors::{
  CellValue, ColumnFilter, FilterOp, SortDirection, TableChanges, TableRowsRequest,
};
use crate::error::Error;

use super::quote_ident;

pub(super) const MAX_FETCH_ROWS: u32 = 5000;

pub(super) struct SelectPlan {
  pub sql: String,
  pub params: Vec<String>,
}

/// The prepared column list is the only source of column identity: filters and
/// sort must name one of these, so no frontend string reaches SQL unquoted.
pub(super) fn build_select(
  columns: &[String],
  request: &TableRowsRequest,
) -> Result<SelectPlan, Error> {
  let target = format!(
    "{}.{}",
    quote_ident(&request.schema),
    quote_ident(&request.table)
  );
  let (where_clause, params) = build_where(columns, &request.filters)?;
  let mut sql = format!("SELECT * FROM {target}{where_clause}");

  if let Some(sort) = &request.sort {
    if !columns.iter().any(|name| name == &sort.column) {
      return Err(Error::Unsupported {
        message: format!("unknown column {}", sort.column),
      });
    }
    let direction = match sort.direction {
      SortDirection::Asc => "ASC",
      SortDirection::Desc => "DESC",
    };
    sql.push_str(&format!(
      " ORDER BY {} {direction}",
      quote_ident(&sort.column)
    ));
  }

  match request.limit {
    Some(limit) => {
      sql.push_str(&format!(
        " LIMIT {} OFFSET {}",
        limit.min(MAX_FETCH_ROWS),
        request.offset
      ));
    }
    // No unlimited OFFSET without LIMIT in mysql; exports start at 0 anyway.
    None if request.offset > 0 => {
      sql.push_str(&format!(
        " LIMIT 18446744073709551615 OFFSET {}",
        request.offset
      ));
    }
    None => {}
  }

  Ok(SelectPlan { sql, params })
}

fn build_where(
  columns: &[String],
  filters: &[ColumnFilter],
) -> Result<(String, Vec<String>), Error> {
  let mut clauses = Vec::new();
  let mut params = Vec::new();
  for filter in filters {
    if !columns.iter().any(|name| name == &filter.column) {
      return Err(Error::Unsupported {
        message: format!("unknown column {}", filter.column),
      });
    }
    let ident = quote_ident(&filter.column);
    let value = || {
      filter.value.clone().ok_or_else(|| Error::Unsupported {
        message: format!("filter on {} needs a value", filter.column),
      })
    };
    match filter.op {
      FilterOp::Eq => {
        clauses.push(format!("{ident} = ?"));
        params.push(value()?);
      }
      FilterOp::Neq => {
        clauses.push(format!("{ident} <> ?"));
        params.push(value()?);
      }
      FilterOp::Lt => {
        clauses.push(format!("{ident} < ?"));
        params.push(value()?);
      }
      FilterOp::Lte => {
        clauses.push(format!("{ident} <= ?"));
        params.push(value()?);
      }
      FilterOp::Gt => {
        clauses.push(format!("{ident} > ?"));
        params.push(value()?);
      }
      FilterOp::Gte => {
        clauses.push(format!("{ident} >= ?"));
        params.push(value()?);
      }
      FilterOp::Contains => {
        clauses.push(format!("{ident} LIKE ?"));
        params.push(format!("%{}%", escape_like(&value()?)));
      }
      FilterOp::StartsWith => {
        clauses.push(format!("{ident} LIKE ?"));
        params.push(format!("{}%", escape_like(&value()?)));
      }
      FilterOp::IsNull => clauses.push(format!("{ident} IS NULL")),
      FilterOp::IsNotNull => clauses.push(format!("{ident} IS NOT NULL")),
    }
  }
  Ok((
    if clauses.is_empty() {
      String::new()
    } else {
      format!(" WHERE {}", clauses.join(" AND "))
    },
    params,
  ))
}

// LIKE metacharacters are data here, never wildcards.
fn escape_like(value: &str) -> String {
  value
    .replace('\\', "\\\\")
    .replace('%', "\\%")
    .replace('_', "\\_")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ChangeKind {
  Update,
  Insert,
  Delete,
}

#[derive(Debug)]
pub(super) struct ChangeStatement {
  pub sql: String,
  pub params: Vec<Option<String>>,
  pub kind: ChangeKind,
}

/// Fixed order: updates, then deletes, then inserts (mirrors postgres).
pub(super) fn build_change_statements(
  columns: &[String],
  changes: &TableChanges,
) -> Result<Vec<ChangeStatement>, Error> {
  let target = format!(
    "{}.{}",
    quote_ident(&changes.schema),
    quote_ident(&changes.table)
  );
  let known = |cell: &CellValue| {
    if columns.iter().any(|name| name == &cell.column) {
      Ok(())
    } else {
      Err(Error::Unsupported {
        message: format!("unknown column {}", cell.column),
      })
    }
  };
  let mut statements = Vec::new();

  for update in &changes.updates {
    if update.set.is_empty() || update.key.is_empty() {
      return Err(Error::Unsupported {
        message: "an update needs at least one changed cell and a key".to_string(),
      });
    }
    let mut params = Vec::new();
    let mut sets = Vec::new();
    for cell in &update.set {
      known(cell)?;
      params.push(cell.value.clone());
      sets.push(format!("{} = ?", quote_ident(&cell.column)));
    }
    let key = key_clause(columns, &update.key, &mut params)?;
    statements.push(ChangeStatement {
      sql: format!("UPDATE {target} SET {} WHERE {key}", sets.join(", ")),
      params,
      kind: ChangeKind::Update,
    });
  }

  for delete in &changes.deletes {
    if delete.key.is_empty() {
      return Err(Error::Unsupported {
        message: "a delete needs a key".to_string(),
      });
    }
    let mut params = Vec::new();
    let key = key_clause(columns, &delete.key, &mut params)?;
    statements.push(ChangeStatement {
      sql: format!("DELETE FROM {target} WHERE {key}"),
      params,
      kind: ChangeKind::Delete,
    });
  }

  for insert in &changes.inserts {
    let mut params = Vec::new();
    let statement = if insert.values.is_empty() {
      // Every column takes its DEFAULT: mysql's DEFAULT VALUES spelling.
      format!("INSERT INTO {target} () VALUES ()")
    } else {
      let mut names = Vec::new();
      let mut values = Vec::new();
      for cell in &insert.values {
        known(cell)?;
        params.push(cell.value.clone());
        names.push(quote_ident(&cell.column));
        values.push("?".to_string());
      }
      format!(
        "INSERT INTO {target} ({}) VALUES ({})",
        names.join(", "),
        values.join(", ")
      )
    };
    statements.push(ChangeStatement {
      sql: statement,
      params,
      kind: ChangeKind::Insert,
    });
  }

  Ok(statements)
}

/// NULL-safe key comparison via `<=>`, mysql's IS NOT DISTINCT FROM.
fn key_clause(
  columns: &[String],
  key: &[CellValue],
  params: &mut Vec<Option<String>>,
) -> Result<String, Error> {
  let mut clauses = Vec::new();
  for cell in key {
    if !columns.iter().any(|name| name == &cell.column) {
      return Err(Error::Unsupported {
        message: format!("unknown column {}", cell.column),
      });
    }
    params.push(cell.value.clone());
    clauses.push(format!("{} <=> ?", quote_ident(&cell.column)));
  }
  Ok(clauses.join(" AND "))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::connectors::{RowDelete, RowInsert, RowUpdate, SortSpec};

  fn cell(column: &str, value: Option<&str>) -> CellValue {
    CellValue {
      column: column.to_string(),
      value: value.map(str::to_string),
    }
  }

  fn request(filters: Vec<ColumnFilter>, sort: Option<SortSpec>) -> TableRowsRequest {
    TableRowsRequest {
      schema: "soquel_test".to_string(),
      table: "customers".to_string(),
      limit: Some(100),
      offset: 20,
      sort,
      filters,
      include_ctid: false,
      include_xmin: false,
    }
  }

  fn filter(column: &str, op: FilterOp, value: Option<&str>) -> ColumnFilter {
    ColumnFilter {
      column: column.to_string(),
      op,
      value: value.map(str::to_string),
    }
  }

  const COLUMNS: &[&str] = &["id", "name", "email"];

  fn columns() -> Vec<String> {
    COLUMNS.iter().map(|s| s.to_string()).collect()
  }

  #[test]
  fn build_select_covers_filters_sort_and_pagination() {
    let plan = build_select(
      &columns(),
      &request(
        vec![
          filter("name", FilterOp::Contains, Some("50%_\\off")),
          filter("email", FilterOp::IsNull, None),
        ],
        Some(SortSpec {
          column: "id".to_string(),
          direction: SortDirection::Desc,
        }),
      ),
    )
    .unwrap();
    assert_eq!(
      plan.sql,
      "SELECT * FROM `soquel_test`.`customers` WHERE `name` LIKE ? AND `email` IS NULL \
       ORDER BY `id` DESC LIMIT 100 OFFSET 20"
    );
    // LIKE metacharacters arrive escaped inside the parameter.
    assert_eq!(plan.params, vec!["%50\\%\\_\\\\off%"]);
  }

  #[test]
  fn build_select_caps_the_limit_and_handles_no_limit() {
    let mut over = request(vec![], None);
    over.limit = Some(1_000_000);
    assert!(build_select(&columns(), &over)
      .unwrap()
      .sql
      .ends_with("LIMIT 5000 OFFSET 20"));

    let mut unlimited = request(vec![], None);
    unlimited.limit = None;
    unlimited.offset = 0;
    assert_eq!(
      build_select(&columns(), &unlimited).unwrap().sql,
      "SELECT * FROM `soquel_test`.`customers`"
    );
  }

  #[test]
  fn build_select_rejects_unknown_columns() {
    let bad_sort = request(
      vec![],
      Some(SortSpec {
        column: "nope".to_string(),
        direction: SortDirection::Asc,
      }),
    );
    assert!(matches!(
      build_select(&columns(), &bad_sort),
      Err(Error::Unsupported { .. })
    ));
    let bad_filter = request(vec![filter("nope", FilterOp::Eq, Some("x"))], None);
    assert!(matches!(
      build_select(&columns(), &bad_filter),
      Err(Error::Unsupported { .. })
    ));
  }

  #[test]
  fn change_statements_use_null_safe_keys_and_quote_hostile_idents() {
    let columns = vec!["id".to_string(), "evil`name".to_string()];
    let changes = TableChanges {
      schema: "s".to_string(),
      table: "t".to_string(),
      updates: vec![RowUpdate {
        key: vec![cell("id", Some("1")), cell("evil`name", None)],
        set: vec![cell("evil`name", Some("x"))],
      }],
      deletes: vec![RowDelete {
        key: vec![cell("id", Some("2"))],
      }],
      inserts: vec![
        RowInsert {
          values: vec![cell("evil`name", Some("y"))],
        },
        RowInsert { values: vec![] },
      ],
    };
    let statements = build_change_statements(&columns, &changes).unwrap();
    assert_eq!(
      statements[0].sql,
      "UPDATE `s`.`t` SET `evil``name` = ? WHERE `id` <=> ? AND `evil``name` <=> ?"
    );
    assert_eq!(
      statements[0].params,
      vec![Some("x".to_string()), Some("1".to_string()), None]
    );
    assert_eq!(statements[1].sql, "DELETE FROM `s`.`t` WHERE `id` <=> ?");
    assert_eq!(
      statements[2].sql,
      "INSERT INTO `s`.`t` (`evil``name`) VALUES (?)"
    );
    assert_eq!(statements[3].sql, "INSERT INTO `s`.`t` () VALUES ()");
  }

  #[test]
  fn change_statements_reject_unknown_columns_and_empty_shapes() {
    let columns = vec!["id".to_string()];
    let unknown = TableChanges {
      schema: "s".to_string(),
      table: "t".to_string(),
      updates: vec![RowUpdate {
        key: vec![cell("id", Some("1"))],
        set: vec![cell("nope", Some("x"))],
      }],
      deletes: vec![],
      inserts: vec![],
    };
    assert!(matches!(
      build_change_statements(&columns, &unknown),
      Err(Error::Unsupported { .. })
    ));

    let empty_key = TableChanges {
      schema: "s".to_string(),
      table: "t".to_string(),
      updates: vec![],
      deletes: vec![RowDelete { key: vec![] }],
      inserts: vec![],
    };
    assert!(matches!(
      build_change_statements(&columns, &empty_key),
      Err(Error::Unsupported { .. })
    ));
  }
}
