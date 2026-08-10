//! SQL builders for the browse/edit surfaces: `?` placeholders, double-quoted
//! idents, rowid standing in for ctid on PK-less tables.

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
  // WITHOUT ROWID tables always have a PK, so the rescue never reaches them.
  let projection = if request.include_ctid {
    "rowid AS \"ctid\", *"
  } else {
    "*"
  };
  let mut sql = format!("SELECT {projection} FROM {target}{where_clause}");

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
    // sqlite has no OFFSET without LIMIT; -1 means unlimited.
    None if request.offset > 0 => {
      sql.push_str(&format!(" LIMIT -1 OFFSET {}", request.offset));
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
        clauses.push(format!("{ident} LIKE ? ESCAPE '\\'"));
        params.push(format!("%{}%", escape_like(&value()?)));
      }
      FilterOp::StartsWith => {
        clauses.push(format!("{ident} LIKE ? ESCAPE '\\'"));
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
      format!("INSERT INTO {target} DEFAULT VALUES")
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

/// NULL-safe key comparison via `IS`; a ctid key maps back to rowid, absent
/// from the prepared list but valid on any rowid table.
fn key_clause(
  columns: &[String],
  key: &[CellValue],
  params: &mut Vec<Option<String>>,
) -> Result<String, Error> {
  let mut clauses = Vec::new();
  for cell in key {
    let ident = if cell.column == "ctid" {
      "rowid".to_string()
    } else if columns.iter().any(|name| name == &cell.column) {
      quote_ident(&cell.column)
    } else {
      return Err(Error::Unsupported {
        message: format!("unknown column {}", cell.column),
      });
    };
    params.push(cell.value.clone());
    clauses.push(format!("{ident} IS ?"));
  }
  Ok(clauses.join(" AND "))
}
