//! SQL builders for the browse/edit surfaces: `$n` placeholders with
//! explicit casts, double-quoted idents.

use tokio_postgres::types::Type as PgType;

use crate::connectors::{
  CellValue, ColumnFilter, ColumnKind, FilterOp, QueryColumn, SortDirection, TableChanges,
  TableRowsRequest,
};
use crate::error::Error;

use super::{column_kind, quote_ident, type_name, PooledPg};

pub(super) const MAX_FETCH_ROWS: u32 = 5000;

pub(super) struct SelectPlan {
  pub sql: String,
  pub params: Vec<String>,
  pub columns: Vec<QueryColumn>,
}

/// Shared by the collected and streamed paths. The prepared column list is the
/// only source of column identity: filters and sort must name one of these, so
/// no frontend string reaches SQL unquoted.
pub(super) async fn plan_select(
  pg: &PooledPg,
  request: &TableRowsRequest,
) -> Result<SelectPlan, Error> {
  let base = format!(
    "SELECT * FROM {}.{}",
    quote_ident(&request.schema),
    quote_ident(&request.table)
  );
  let prepared = pg.client.prepare(&base).await?;
  let columns: Vec<(String, PgType)> = prepared
    .columns()
    .iter()
    .map(|c| (c.name().to_string(), c.type_().clone()))
    .collect();

  let (where_clause, params) = build_where(&columns, &request.filters)?;

  // ::text keeps the server's canonical formatting (arrays, timestamps, bytea)
  // while the extended protocol carries the filter parameters.
  let mut projection = columns
    .iter()
    .map(|(name, _)| {
      let ident = quote_ident(name);
      format!("{ident}::text AS {ident}")
    })
    .collect::<Vec<_>>()
    .join(", ");
  if request.include_xmin {
    projection = format!("\"xmin\"::text AS \"xmin\", {projection}");
  }
  if request.include_ctid {
    projection = format!("\"ctid\"::text AS \"ctid\", {projection}");
  }
  let mut sql = format!(
    "SELECT {projection} FROM {}.{}{where_clause}",
    quote_ident(&request.schema),
    quote_ident(&request.table)
  );
  if let Some(sort) = &request.sort {
    if !columns.iter().any(|(name, _)| name == &sort.column) {
      return Err(Error::Unsupported {
        message: format!("unknown column {}", sort.column),
      });
    }
    let direction = match sort.direction {
      SortDirection::Asc => "ASC",
      SortDirection::Desc => "DESC",
    };
    // Qualified: a bare name would resolve to the ::text output alias and sort
    // lexicographically.
    sql.push_str(&format!(
      " ORDER BY {}.{}.{} {direction}",
      quote_ident(&request.schema),
      quote_ident(&request.table),
      quote_ident(&sort.column)
    ));
  }
  if let Some(limit) = request.limit {
    sql.push_str(&format!(" LIMIT {}", limit.min(MAX_FETCH_ROWS)));
  }
  sql.push_str(&format!(" OFFSET {}", request.offset));

  let mut result_columns: Vec<QueryColumn> = columns
    .iter()
    .map(|(name, ty)| QueryColumn {
      name: name.clone(),
      data_type: Some(type_name(ty)),
      kind: column_kind(ty),
    })
    .collect();
  if request.include_ctid {
    result_columns.insert(
      0,
      QueryColumn {
        name: "ctid".to_string(),
        data_type: Some("tid".to_string()),
        kind: ColumnKind::Other,
      },
    );
  }
  if request.include_xmin {
    let position = usize::from(request.include_ctid);
    result_columns.insert(
      position,
      QueryColumn {
        name: "xmin".to_string(),
        data_type: Some("xid".to_string()),
        kind: ColumnKind::Other,
      },
    );
  }
  Ok(SelectPlan {
    sql,
    params,
    columns: result_columns,
  })
}

// Every parameter travels as text; build_where casts it to the column's type.
pub(super) fn bind_text(
  params: &[String],
) -> Vec<(&(dyn tokio_postgres::types::ToSql + Sync), PgType)> {
  params
    .iter()
    .map(|value| {
      (
        value as &(dyn tokio_postgres::types::ToSql + Sync),
        PgType::TEXT,
      )
    })
    .collect()
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

/// Fixed order: updates, then deletes, then inserts.
pub(super) fn build_change_statements(
  schema: &str,
  table: &str,
  columns: &[(String, PgType)],
  changes: &TableChanges,
) -> Result<Vec<ChangeStatement>, Error> {
  let target = format!("{}.{}", quote_ident(schema), quote_ident(table));
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
      let cast = change_cast(columns, &cell.column)?;
      params.push(cell.value.clone());
      sets.push(format!(
        "{} = ${}::{cast}",
        quote_ident(&cell.column),
        params.len()
      ));
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
        let cast = change_cast(columns, &cell.column)?;
        params.push(cell.value.clone());
        names.push(quote_ident(&cell.column));
        values.push(format!("${}::{cast}", params.len()));
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

// System columns are absent from the prepared list but valid as keys:
// ctid for PK-less tables, xmin as the optimistic-lock guard.
pub(super) fn change_cast(columns: &[(String, PgType)], column: &str) -> Result<String, Error> {
  if column == "ctid" {
    return Ok("tid".to_string());
  }
  if column == "xmin" {
    return Ok("xid".to_string());
  }
  columns
    .iter()
    .find(|(name, _)| name == column)
    .map(|(_, ty)| type_name(ty))
    .ok_or_else(|| Error::Unsupported {
      message: format!("unknown column {column}"),
    })
}

/// NULL-safe key comparison: PK values are never NULL, but ctid-less tables
/// may key on nullable columns.
pub(super) fn key_clause(
  columns: &[(String, PgType)],
  key: &[CellValue],
  params: &mut Vec<Option<String>>,
) -> Result<String, Error> {
  let mut clauses = Vec::new();
  for cell in key {
    let cast = change_cast(columns, &cell.column)?;
    params.push(cell.value.clone());
    clauses.push(format!(
      "{} IS NOT DISTINCT FROM ${}::{cast}",
      quote_ident(&cell.column),
      params.len()
    ));
  }
  Ok(clauses.join(" AND "))
}

/// AND-ed WHERE clause + bound parameter values, `$1`-numbered in order.
pub(super) fn build_where(
  columns: &[(String, PgType)],
  filters: &[ColumnFilter],
) -> Result<(String, Vec<String>), Error> {
  let mut clauses = Vec::new();
  let mut params: Vec<String> = Vec::new();
  for filter in filters {
    if !columns.iter().any(|(name, _)| name == &filter.column) {
      return Err(Error::Unsupported {
        message: format!("unknown column {}", filter.column),
      });
    }
    let ident = quote_ident(&filter.column);
    // Parameters are declared text on the wire: comparisons cast them to the
    // column's type so postgres compares values, not strings.
    let cast = columns
      .iter()
      .find(|(name, _)| name == &filter.column)
      .map(|(_, ty)| type_name(ty))
      .unwrap_or_default();
    let clause = match filter.op {
      FilterOp::IsNull => format!("{ident} IS NULL"),
      FilterOp::IsNotNull => format!("{ident} IS NOT NULL"),
      op => {
        let value = filter.value.clone().ok_or_else(|| Error::Unsupported {
          message: format!("filter on {} requires a value", filter.column),
        })?;
        let n = params.len() + 1;
        match op {
          FilterOp::Contains => {
            params.push(format!("%{}%", escape_like(&value)));
            format!("{ident}::text ILIKE ${n}")
          }
          FilterOp::StartsWith => {
            params.push(format!("{}%", escape_like(&value)));
            format!("{ident}::text ILIKE ${n}")
          }
          _ => {
            params.push(value);
            format!("{ident} {} ${n}::{cast}", comparison_operator(op))
          }
        }
      }
    };
    clauses.push(clause);
  }
  let clause = if clauses.is_empty() {
    String::new()
  } else {
    format!(" WHERE {}", clauses.join(" AND "))
  };
  Ok((clause, params))
}

pub(super) fn comparison_operator(op: FilterOp) -> &'static str {
  match op {
    FilterOp::Eq => "=",
    FilterOp::Neq => "<>",
    FilterOp::Lt => "<",
    FilterOp::Lte => "<=",
    FilterOp::Gt => ">",
    FilterOp::Gte => ">=",
    FilterOp::Contains | FilterOp::StartsWith | FilterOp::IsNull | FilterOp::IsNotNull => {
      unreachable!("handled before reaching the comparison branch")
    }
  }
}

// User values are literals: LIKE metacharacters must not act as wildcards.
pub(super) fn escape_like(value: &str) -> String {
  value
    .replace('\\', "\\\\")
    .replace('%', "\\%")
    .replace('_', "\\_")
}
