use rusqlite::OptionalExtension;

use crate::connectors::{
  ColumnInfo, ForeignKeyInfo, IndexInfo, Introspect, SchemaInfo, SchemaSnapshot, TableInfo,
  TableKind,
};
use crate::error::Error;

use super::{quote_ident, SqliteConnection};

#[async_trait::async_trait]
impl Introspect for SqliteConnection {
  async fn schema_snapshot(&self) -> Result<SchemaSnapshot, Error> {
    self
      .exec(|conn| {
        let mut stmt = conn.prepare(
          "SELECT name, type FROM sqlite_master
           WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite_%'
           ORDER BY name",
        )?;
        let listed: Vec<(String, String)> = stmt
          .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
          .collect::<Result<_, _>>()?;
        drop(stmt);

        let mut tables = Vec::new();
        for (name, table_type) in listed {
          let mut info = TableInfo {
            kind: if table_type == "view" {
              TableKind::View
            } else {
              TableKind::Table
            },
            // sqlite keeps no row estimate; mirror pg's "never analyzed".
            estimated_rows: -1.0,
            columns: Vec::new(),
            primary_key: Vec::new(),
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
            name,
          };
          load_columns(conn, &mut info)?;
          if info.kind == TableKind::Table {
            load_indexes(conn, &mut info)?;
            load_foreign_keys(conn, &mut info)?;
          }
          tables.push(info);
        }

        Ok(SchemaSnapshot {
          schemas: vec![SchemaInfo {
            name: "main".to_string(),
            tables,
          }],
        })
      })
      .await
  }

  async fn table_ddl(&self, _schema: &str, table: &str) -> Result<String, Error> {
    let table = table.to_string();
    self
      .exec(move |conn| {
        // The stored CREATE statements, table first, then its indexes/triggers.
        let mut stmt = conn.prepare(
          "SELECT sql FROM sqlite_master
           WHERE tbl_name = ?1 AND sql IS NOT NULL
           ORDER BY CASE type WHEN 'table' THEN 0 WHEN 'view' THEN 0 WHEN 'index' THEN 1 ELSE 2 END, name",
        )?;
        let pieces: Vec<String> = stmt
          .query_map([&table], |row| row.get(0))?
          .collect::<Result<_, _>>()?;
        if pieces.is_empty() {
          return Err(Error::NotFound {
            message: format!("table {table} not found"),
          });
        }
        Ok(
          pieces
            .iter()
            .map(|sql| format!("{};", sql.trim_end().trim_end_matches(';')))
            .collect::<Vec<_>>()
            .join("\n\n"),
        )
      })
      .await
  }
}

fn load_columns(conn: &rusqlite::Connection, info: &mut TableInfo) -> Result<(), Error> {
  let mut stmt = conn.prepare(&format!("PRAGMA table_info({})", quote_ident(&info.name)))?;
  // (name, type, notnull, dflt_value, pk position)
  let columns: Vec<(String, String, bool, Option<String>, i64)> = stmt
    .query_map([], |row| {
      Ok((
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
      ))
    })?
    .collect::<Result<_, _>>()?;

  let mut pk: Vec<(i64, String)> = Vec::new();
  for (name, data_type, notnull, default, pk_position) in columns {
    if pk_position > 0 {
      pk.push((pk_position, name.clone()));
    }
    info.columns.push(ColumnInfo {
      name,
      data_type: if data_type.is_empty() {
        "any".to_string()
      } else {
        data_type.to_lowercase()
      },
      nullable: !notnull,
      default,
    });
  }
  pk.sort_by_key(|(position, _)| *position);
  info.primary_key = pk.into_iter().map(|(_, name)| name).collect();
  Ok(())
}

fn load_indexes(conn: &rusqlite::Connection, info: &mut TableInfo) -> Result<(), Error> {
  let mut stmt = conn.prepare(&format!("PRAGMA index_list({})", quote_ident(&info.name)))?;
  // (name, unique, origin): origin 'pk' rows duplicate the primary key.
  let indexes: Vec<(String, bool, String)> = stmt
    .query_map([], |row| Ok((row.get(1)?, row.get(2)?, row.get(3)?)))?
    .collect::<Result<_, _>>()?;
  drop(stmt);

  for (name, unique, origin) in indexes {
    if origin == "pk" {
      continue;
    }
    // CREATE INDEX text when it exists; auto unique indexes get a synthesized one.
    let stored: Option<String> = conn
      .query_row(
        "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = ?1 AND sql IS NOT NULL",
        [&name],
        |row| row.get(0),
      )
      .optional()?;
    let definition = match stored {
      Some(sql) => sql,
      None => {
        let mut stmt = conn.prepare(&format!("PRAGMA index_info({})", quote_ident(&name)))?;
        let columns: Vec<String> = stmt
          .query_map([], |row| row.get(2))?
          .collect::<Result<_, _>>()?;
        format!(
          "UNIQUE INDEX {} ({})",
          quote_ident(&name),
          columns.join(", ")
        )
      }
    };
    info.indexes.push(IndexInfo {
      name,
      definition,
      unique,
    });
  }
  Ok(())
}

fn load_foreign_keys(conn: &rusqlite::Connection, info: &mut TableInfo) -> Result<(), Error> {
  let mut stmt = conn.prepare(&format!(
    "PRAGMA foreign_key_list({})",
    quote_ident(&info.name)
  ))?;
  // (id, referenced table, from, to): `to` is NULL for implicit-PK references.
  let rows: Vec<(i64, String, String, Option<String>)> = stmt
    .query_map([], |row| {
      Ok((row.get(0)?, row.get(2)?, row.get(3)?, row.get(4)?))
    })?
    .collect::<Result<_, _>>()?;

  for (id, referenced_table, from, to) in rows {
    let name = format!("fk_{}_{id}", info.name);
    match info.foreign_keys.iter_mut().find(|fk| fk.name == name) {
      Some(fk) => {
        fk.columns.push(from);
        if let Some(to) = to {
          fk.referenced_columns.push(to);
        }
      }
      None => info.foreign_keys.push(ForeignKeyInfo {
        name,
        columns: vec![from],
        referenced_schema: "main".to_string(),
        referenced_table,
        referenced_columns: to.into_iter().collect(),
      }),
    }
  }
  Ok(())
}
