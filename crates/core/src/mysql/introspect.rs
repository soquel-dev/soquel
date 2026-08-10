use std::collections::HashMap;

use mysql_async::prelude::Queryable;

use crate::connectors::{
  ColumnInfo, ForeignKeyInfo, IndexInfo, Introspect, SchemaInfo, SchemaSnapshot, TableInfo,
  TableKind,
};
use crate::error::Error;

use super::{quote_ident, MysqlConnection};

// Databases play the schema role; the server's own ones stay out.
const USER_SCHEMAS: &str =
  "TABLE_SCHEMA NOT IN ('mysql', 'information_schema', 'performance_schema', 'sys')";

#[async_trait::async_trait]
impl Introspect for MysqlConnection {
  async fn schema_snapshot(&self) -> Result<SchemaSnapshot, Error> {
    let mut conn = self.pool.conn().await?;

    let tables: Vec<(String, String, String, Option<u64>)> = conn
      .query(format!(
        "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE, TABLE_ROWS
         FROM information_schema.TABLES
         WHERE {USER_SCHEMAS}
         ORDER BY TABLE_SCHEMA, TABLE_NAME"
      ))
      .await?;

    let mut order: Vec<(String, String)> = Vec::new();
    let mut map: HashMap<(String, String), TableInfo> = HashMap::new();
    for (schema, table, table_type, estimated) in tables {
      let key = (schema, table.clone());
      order.push(key.clone());
      map.insert(
        key,
        TableInfo {
          name: table,
          kind: if table_type == "VIEW" {
            TableKind::View
          } else {
            TableKind::Table
          },
          // Views carry NULL: mirror pg's "never analyzed" sentinel.
          estimated_rows: estimated.map_or(-1.0, |rows| rows as f64),
          columns: Vec::new(),
          primary_key: Vec::new(),
          indexes: Vec::new(),
          foreign_keys: Vec::new(),
        },
      );
    }

    let columns: Vec<(
      String,
      String,
      String,
      String,
      String,
      Option<String>,
      String,
    )> = conn
      .query(format!(
        "SELECT TABLE_SCHEMA, TABLE_NAME, COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE,
                COLUMN_DEFAULT, EXTRA
         FROM information_schema.COLUMNS
         WHERE {USER_SCHEMAS}
         ORDER BY TABLE_SCHEMA, TABLE_NAME, ORDINAL_POSITION"
      ))
      .await?;
    for (schema, table, name, column_type, nullable, default, extra) in columns {
      if let Some(info) = map.get_mut(&(schema, table)) {
        info.columns.push(ColumnInfo {
          name,
          data_type: column_type,
          nullable: nullable == "YES",
          default: if extra.contains("auto_increment") {
            Some("auto_increment".to_string())
          } else {
            default
          },
        });
      }
    }

    let primary_keys: Vec<(String, String, String)> = conn
      .query(format!(
        "SELECT TABLE_SCHEMA, TABLE_NAME, COLUMN_NAME
         FROM information_schema.KEY_COLUMN_USAGE
         WHERE CONSTRAINT_NAME = 'PRIMARY' AND {USER_SCHEMAS}
         ORDER BY TABLE_SCHEMA, TABLE_NAME, ORDINAL_POSITION"
      ))
      .await?;
    for (schema, table, column) in primary_keys {
      if let Some(info) = map.get_mut(&(schema, table)) {
        info.primary_key.push(column);
      }
    }

    let indexes: Vec<(String, String, String, i64, String)> = conn
      .query(format!(
        "SELECT TABLE_SCHEMA, TABLE_NAME, INDEX_NAME, MIN(NON_UNIQUE),
                GROUP_CONCAT(COLUMN_NAME ORDER BY SEQ_IN_INDEX SEPARATOR ', ')
         FROM information_schema.STATISTICS
         WHERE INDEX_NAME <> 'PRIMARY' AND {USER_SCHEMAS}
         GROUP BY TABLE_SCHEMA, TABLE_NAME, INDEX_NAME
         ORDER BY TABLE_SCHEMA, TABLE_NAME, INDEX_NAME"
      ))
      .await?;
    for (schema, table, name, non_unique, columns) in indexes {
      if let Some(info) = map.get_mut(&(schema, table)) {
        let unique = non_unique == 0;
        info.indexes.push(IndexInfo {
          definition: format!(
            "{}INDEX {} ({columns})",
            if unique { "UNIQUE " } else { "" },
            quote_ident(&name)
          ),
          name,
          unique,
        });
      }
    }

    let foreign_keys: Vec<(String, String, String, String, String, String)> = conn
      .query(format!(
        "SELECT TABLE_SCHEMA, TABLE_NAME, CONSTRAINT_NAME, COLUMN_NAME,
                REFERENCED_TABLE_SCHEMA, REFERENCED_TABLE_NAME
         FROM information_schema.KEY_COLUMN_USAGE
         WHERE REFERENCED_TABLE_NAME IS NOT NULL AND {USER_SCHEMAS}
         ORDER BY TABLE_SCHEMA, TABLE_NAME, CONSTRAINT_NAME, ORDINAL_POSITION"
      ))
      .await?;
    let referenced: Vec<(String, String, String, String)> = conn
      .query(format!(
        "SELECT TABLE_SCHEMA, TABLE_NAME, CONSTRAINT_NAME, REFERENCED_COLUMN_NAME
         FROM information_schema.KEY_COLUMN_USAGE
         WHERE REFERENCED_TABLE_NAME IS NOT NULL AND {USER_SCHEMAS}
         ORDER BY TABLE_SCHEMA, TABLE_NAME, CONSTRAINT_NAME, ORDINAL_POSITION"
      ))
      .await?;
    let mut referenced_by_key: HashMap<(String, String, String), Vec<String>> = HashMap::new();
    for (schema, table, constraint, column) in referenced {
      referenced_by_key
        .entry((schema, table, constraint))
        .or_default()
        .push(column);
    }
    for (schema, table, constraint, column, ref_schema, ref_table) in foreign_keys {
      let Some(info) = map.get_mut(&(schema.clone(), table.clone())) else {
        continue;
      };
      match info
        .foreign_keys
        .iter_mut()
        .find(|fk| fk.name == constraint)
      {
        Some(fk) => fk.columns.push(column),
        None => info.foreign_keys.push(ForeignKeyInfo {
          columns: vec![column],
          referenced_columns: referenced_by_key
            .remove(&(schema, table, constraint.clone()))
            .unwrap_or_default(),
          referenced_schema: ref_schema,
          referenced_table: ref_table,
          name: constraint,
        }),
      }
    }

    let mut schemas: Vec<SchemaInfo> = Vec::new();
    for key in order {
      let table = map.remove(&key).unwrap();
      match schemas.last_mut() {
        Some(schema) if schema.name == key.0 => schema.tables.push(table),
        _ => schemas.push(SchemaInfo {
          name: key.0,
          tables: vec![table],
        }),
      }
    }
    Ok(SchemaSnapshot { schemas })
  }

  async fn table_ddl(&self, schema: &str, table: &str) -> Result<String, Error> {
    let mut conn = self.pool.conn().await?;
    // Works for views too: the definition always sits in the second column.
    let row: Option<mysql_async::Row> = conn
      .query_first(format!(
        "SHOW CREATE TABLE {}.{}",
        quote_ident(schema),
        quote_ident(table)
      ))
      .await
      .map_err(|_| Error::NotFound {
        message: format!("table {schema}.{table} not found"),
      })?;
    let mut row = row.ok_or_else(|| Error::NotFound {
      message: format!("table {schema}.{table} not found"),
    })?;
    row.take::<String, _>(1).ok_or_else(|| Error::Database {
      message: "SHOW CREATE TABLE returned no definition".to_string(),
    })
  }
}
