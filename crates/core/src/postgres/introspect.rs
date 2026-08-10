use std::collections::HashMap;

use crate::connectors::{
  ColumnInfo, ForeignKeyInfo, IndexInfo, Introspect, SchemaInfo, SchemaSnapshot, TableInfo,
  TableKind,
};
use crate::error::Error;

use super::{quote_ident, PostgresConnection};

// User schemas only: everything except pg_* and information_schema.
const USER_SCHEMAS: &str = "n.nspname !~ '^pg_' AND n.nspname <> 'information_schema'";

#[async_trait::async_trait]
impl Introspect for PostgresConnection {
  async fn schema_snapshot(&self) -> Result<SchemaSnapshot, Error> {
    let tables = format!(
      "SELECT n.nspname, c.relname, c.relkind::text, c.reltuples::float8
       FROM pg_class c
       JOIN pg_namespace n ON n.oid = c.relnamespace
       WHERE c.relkind IN ('r', 'p', 'v', 'm') AND {USER_SCHEMAS}
       ORDER BY n.nspname, c.relname"
    );
    let columns = format!(
      "SELECT n.nspname, c.relname, a.attname,
              format_type(a.atttypid, a.atttypmod),
              NOT a.attnotnull,
              pg_get_expr(d.adbin, d.adrelid)
       FROM pg_attribute a
       JOIN pg_class c ON c.oid = a.attrelid
       JOIN pg_namespace n ON n.oid = c.relnamespace
       LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum
       WHERE a.attnum > 0 AND NOT a.attisdropped
         AND c.relkind IN ('r', 'p', 'v', 'm') AND {USER_SCHEMAS}
       ORDER BY n.nspname, c.relname, a.attnum"
    );
    let primary_keys = format!(
      "SELECT n.nspname, c.relname, a.attname
       FROM pg_index i
       JOIN pg_class c ON c.oid = i.indrelid
       JOIN pg_namespace n ON n.oid = c.relnamespace
       JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum = ANY (i.indkey)
       WHERE i.indisprimary AND {USER_SCHEMAS}
       ORDER BY n.nspname, c.relname, array_position(i.indkey, a.attnum)"
    );
    let indexes = format!(
      "SELECT n.nspname, c.relname, ic.relname,
              pg_get_indexdef(i.indexrelid), i.indisunique
       FROM pg_index i
       JOIN pg_class c ON c.oid = i.indrelid
       JOIN pg_class ic ON ic.oid = i.indexrelid
       JOIN pg_namespace n ON n.oid = c.relnamespace
       WHERE NOT i.indisprimary AND {USER_SCHEMAS}
       ORDER BY n.nspname, c.relname, ic.relname"
    );
    let foreign_keys = format!(
      "SELECT n.nspname, c.relname, con.conname,
              (SELECT array_agg(a.attname ORDER BY x.ord)
               FROM unnest(con.conkey) WITH ORDINALITY AS x(attnum, ord)
               JOIN pg_attribute a ON a.attrelid = con.conrelid AND a.attnum = x.attnum),
              fn.nspname, fc.relname,
              (SELECT array_agg(a.attname ORDER BY x.ord)
               FROM unnest(con.confkey) WITH ORDINALITY AS x(attnum, ord)
               JOIN pg_attribute a ON a.attrelid = con.confrelid AND a.attnum = x.attnum)
       FROM pg_constraint con
       JOIN pg_class c ON c.oid = con.conrelid
       JOIN pg_namespace n ON n.oid = c.relnamespace
       JOIN pg_class fc ON fc.oid = con.confrelid
       JOIN pg_namespace fn ON fn.oid = fc.relnamespace
       WHERE con.contype = 'f' AND {USER_SCHEMAS}
       ORDER BY n.nspname, c.relname, con.conname"
    );

    let pg = self.checkout().await?;

    // (schema, table) -> TableInfo, insertion-ordered by the tables query.
    let mut order: Vec<(String, String)> = Vec::new();
    let mut map: HashMap<(String, String), TableInfo> = HashMap::new();

    for row in pg.client.query(&tables, &[]).await? {
      let key = (row.get::<_, String>(0), row.get::<_, String>(1));
      let kind = match row.get::<_, String>(2).as_str() {
        "v" => TableKind::View,
        "m" => TableKind::MaterializedView,
        _ => TableKind::Table,
      };
      order.push(key.clone());
      map.insert(
        key,
        TableInfo {
          name: order.last().unwrap().1.clone(),
          kind,
          estimated_rows: row.get(3),
          columns: Vec::new(),
          primary_key: Vec::new(),
          indexes: Vec::new(),
          foreign_keys: Vec::new(),
        },
      );
    }

    for row in pg.client.query(&columns, &[]).await? {
      if let Some(table) = map.get_mut(&(row.get(0), row.get(1))) {
        table.columns.push(ColumnInfo {
          name: row.get(2),
          data_type: row.get(3),
          nullable: row.get(4),
          default: row.get(5),
        });
      }
    }

    for row in pg.client.query(&primary_keys, &[]).await? {
      if let Some(table) = map.get_mut(&(row.get(0), row.get(1))) {
        table.primary_key.push(row.get(2));
      }
    }

    for row in pg.client.query(&indexes, &[]).await? {
      if let Some(table) = map.get_mut(&(row.get(0), row.get(1))) {
        table.indexes.push(IndexInfo {
          name: row.get(2),
          definition: row.get(3),
          unique: row.get(4),
        });
      }
    }

    for row in pg.client.query(&foreign_keys, &[]).await? {
      if let Some(table) = map.get_mut(&(row.get(0), row.get(1))) {
        table.foreign_keys.push(ForeignKeyInfo {
          name: row.get(2),
          columns: row.get(3),
          referenced_schema: row.get(4),
          referenced_table: row.get(5),
          referenced_columns: row.get(6),
        });
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

  /// pg_dump-style definition assembled from the catalog: `pg_get_*def` give
  /// exact clauses (FK actions, CHECKs, view bodies) the snapshot doesn't carry.
  async fn table_ddl(&self, schema: &str, table: &str) -> Result<String, Error> {
    let pg = self.checkout().await?;

    let rel = pg
      .client
      .query_opt(
        "SELECT c.oid, c.relkind::text
         FROM pg_class c
         JOIN pg_namespace n ON n.oid = c.relnamespace
         WHERE n.nspname = $1 AND c.relname = $2 AND c.relkind IN ('r', 'p', 'v', 'm')",
        &[&schema, &table],
      )
      .await?;
    let Some(rel) = rel else {
      return Err(Error::NotFound {
        message: format!("table {schema}.{table} not found"),
      });
    };
    let oid: u32 = rel.get(0);
    let relkind: String = rel.get(1);
    let target = format!("{}.{}", quote_ident(schema), quote_ident(table));

    if relkind == "v" || relkind == "m" {
      let definition: String = pg
        .client
        .query_one("SELECT pg_get_viewdef($1::oid, true)", &[&oid])
        .await?
        .get(0);
      let keyword = if relkind == "m" {
        "MATERIALIZED VIEW"
      } else {
        "VIEW"
      };
      return Ok(format!("CREATE {keyword} {target} AS\n{definition}"));
    }

    let columns = pg
      .client
      .query(
        // Non-default collation only: comparing against the type's own collation
        // keeps implicit ones (text -> "default") off the column line.
        "SELECT a.attname, format_type(a.atttypid, a.atttypmod), a.attnotnull,
                pg_get_expr(d.adbin, d.adrelid),
                a.attidentity::text, a.attgenerated::text,
                CASE WHEN a.attcollation <> t.typcollation THEN col.collname END
         FROM pg_attribute a
         JOIN pg_type t ON t.oid = a.atttypid
         LEFT JOIN pg_collation col ON col.oid = a.attcollation
         LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum
         WHERE a.attrelid = $1 AND a.attnum > 0 AND NOT a.attisdropped
         ORDER BY a.attnum",
        &[&oid],
      )
      .await?;
    let mut lines = Vec::new();
    for row in &columns {
      let name: String = row.get(0);
      let data_type: String = row.get(1);
      let not_null: bool = row.get(2);
      let default: Option<String> = row.get(3);
      let identity: String = row.get(4);
      let generated: String = row.get(5);
      let collation: Option<String> = row.get(6);
      let mut line = format!("  {} {data_type}", quote_ident(&name));
      if let Some(collation) = collation {
        line.push_str(&format!(" COLLATE {}", quote_ident(&collation)));
      }
      // Identity has no pg_attrdef row; a generated column's row holds the
      // expression, which must not surface as a plain DEFAULT.
      match (identity.as_str(), generated.as_str()) {
        ("a", _) => line.push_str(" GENERATED ALWAYS AS IDENTITY"),
        ("d", _) => line.push_str(" GENERATED BY DEFAULT AS IDENTITY"),
        (_, "s") => {
          let expr = default.as_deref().unwrap_or_default();
          line.push_str(&format!(" GENERATED ALWAYS AS ({expr}) STORED"));
        }
        // Virtual generated columns (postgres 18+).
        (_, "v") => {
          let expr = default.as_deref().unwrap_or_default();
          line.push_str(&format!(" GENERATED ALWAYS AS ({expr}) VIRTUAL"));
        }
        _ => {
          if let Some(default) = &default {
            line.push_str(&format!(" DEFAULT {default}"));
          }
        }
      }
      if not_null {
        line.push_str(" NOT NULL");
      }
      lines.push(line);
    }
    let mut ddl = format!("CREATE TABLE {target} (\n{}\n);", lines.join(",\n"));

    let constraints = pg
      .client
      .query(
        // contype 'n': postgres 18 catalogs NOT NULL as constraints; the column
        // lines already carry them.
        "SELECT conname, pg_get_constraintdef(oid, true)
         FROM pg_constraint
         WHERE conrelid = $1 AND contype <> 'n'
         ORDER BY CASE contype WHEN 'p' THEN 0 WHEN 'u' THEN 1 WHEN 'f' THEN 2 ELSE 3 END, conname",
        &[&oid],
      )
      .await?;
    for row in &constraints {
      let name: String = row.get(0);
      let definition: String = row.get(1);
      ddl.push_str(&format!(
        "\n\nALTER TABLE {target}\n  ADD CONSTRAINT {} {definition};",
        quote_ident(&name)
      ));
    }

    // Constraint-backed indexes already appear as constraints above.
    let indexes = pg
      .client
      .query(
        "SELECT pg_get_indexdef(i.indexrelid, 0, true)
         FROM pg_index i
         WHERE i.indrelid = $1
           AND NOT EXISTS (SELECT 1 FROM pg_constraint con WHERE con.conindid = i.indexrelid)
         ORDER BY 1",
        &[&oid],
      )
      .await?;
    for row in &indexes {
      let definition: String = row.get(0);
      ddl.push_str(&format!("\n\n{definition};"));
    }

    let comments = pg
      .client
      .query(
        "SELECT NULL::text, obj_description($1::oid, 'pg_class') WHERE obj_description($1::oid, 'pg_class') IS NOT NULL
         UNION ALL
         SELECT a.attname, col_description($1::oid, a.attnum)
         FROM pg_attribute a
         WHERE a.attrelid = $1 AND a.attnum > 0 AND NOT a.attisdropped
           AND col_description($1::oid, a.attnum) IS NOT NULL",
        &[&oid],
      )
      .await?;
    for row in &comments {
      let column: Option<String> = row.get(0);
      let comment: String = row.get(1);
      let literal = format!("'{}'", comment.replace('\'', "''"));
      match column {
        Some(column) => ddl.push_str(&format!(
          "\n\nCOMMENT ON COLUMN {target}.{} IS {literal};",
          quote_ident(&column)
        )),
        None => ddl.push_str(&format!("\n\nCOMMENT ON TABLE {target} IS {literal};")),
      }
    }

    Ok(ddl)
  }
}

#[cfg(test)]
mod tests {
  use super::super::tests::test_connection_from_env;
  use crate::connectors::{Introspect, TableKind};
  use crate::error::Error;

  #[tokio::test]
  async fn integration_postgres_schema_snapshot() {
    let Some(pg) = test_connection_from_env().await else {
      return;
    };
    let snapshot = pg.schema_snapshot().await.unwrap();

    let names: Vec<&str> = snapshot.schemas.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["app", "public"]);

    let app = &snapshot.schemas[0];
    let customers = app.tables.iter().find(|t| t.name == "customers").unwrap();
    assert_eq!(customers.kind, TableKind::Table);
    assert_eq!(customers.primary_key, vec!["id"]);
    let email = customers
      .columns
      .iter()
      .find(|c| c.name == "email")
      .unwrap();
    assert!(email.nullable);
    assert_eq!(email.data_type, "text");
    let tags = customers.columns.iter().find(|c| c.name == "tags").unwrap();
    assert_eq!(tags.data_type, "text[]");
    let id = customers.columns.iter().find(|c| c.name == "id").unwrap();
    assert!(id.default.as_deref().unwrap().contains("nextval"));

    let orders = app.tables.iter().find(|t| t.name == "orders").unwrap();
    let fk = &orders.foreign_keys[0];
    assert_eq!(fk.columns, vec!["customer_id"]);
    assert_eq!(fk.referenced_schema, "app");
    assert_eq!(fk.referenced_table, "customers");
    assert_eq!(fk.referenced_columns, vec!["id"]);
    assert!(orders
      .indexes
      .iter()
      .any(|i| i.name == "orders_customer_idx" && !i.unique));

    let view = app
      .tables
      .iter()
      .find(|t| t.name == "recent_orders")
      .unwrap();
    assert_eq!(view.kind, TableKind::View);
    assert!(!view.columns.is_empty());

    let matview = app
      .tables
      .iter()
      .find(|t| t.name == "order_totals")
      .unwrap();
    assert_eq!(matview.kind, TableKind::MaterializedView);
  }

  #[tokio::test]
  async fn integration_postgres_table_ddl_assembles_full_definition() {
    let Some(pg) = test_connection_from_env().await else {
      return;
    };
    let ddl = pg.table_ddl("app", "orders").await.unwrap();
    assert!(ddl.contains(r#"CREATE TABLE "app"."orders" ("#), "{ddl}");
    assert!(ddl.contains(r#""id" integer DEFAULT nextval("#), "{ddl}");
    assert!(ddl.contains(r#""amount" numeric(10,2) NOT NULL"#), "{ddl}");
    assert!(ddl.contains("PRIMARY KEY (id)"), "{ddl}");
    // pg_get_constraintdef carries what the snapshot doesn't: the full FK clause.
    assert!(
      ddl.contains("FOREIGN KEY (customer_id) REFERENCES app.customers(id)"),
      "{ddl}"
    );
    assert!(ddl.contains("CREATE INDEX orders_customer_idx"), "{ddl}");

    // CHECK constraints and comments come from the catalog too.
    assert!(
      ddl.contains(r#"ADD CONSTRAINT "orders_amount_positive" CHECK (amount > 0::numeric)"#),
      "{ddl}"
    );
    // NOT NULL lives on the column line, never as a duplicate constraint.
    assert!(!ddl.contains("NOT NULL amount"), "{ddl}");
    assert!(
      ddl.contains(
        r#"COMMENT ON TABLE "app"."orders" IS 'Customer orders; amounts in the customer''s currency.';"#
      ),
      "{ddl}"
    );
    assert!(
      ddl.contains(
        r#"COMMENT ON COLUMN "app"."orders"."receipt" IS 'Raw PDF bytes, NULL until issued.';"#
      ),
      "{ddl}"
    );

    let probe = pg.table_ddl("app", "ddl_probe").await.unwrap();
    assert!(
      probe.contains(r#""id" bigint GENERATED ALWAYS AS IDENTITY NOT NULL"#),
      "{probe}"
    );
    assert!(
      probe.contains(r#""seq" smallint GENERATED BY DEFAULT AS IDENTITY NOT NULL"#),
      "{probe}"
    );
    assert!(
      probe.contains(r#""n_doubled" integer GENERATED ALWAYS AS ((n * 2)) STORED"#),
      "{probe}"
    );
    assert!(probe.contains(r#""label" text COLLATE "C""#), "{probe}");
    // The generation expression must never degrade to a plain DEFAULT.
    assert!(!probe.contains(r#""n_doubled" integer DEFAULT"#), "{probe}");
    assert!(
      probe.contains(r#""n" integer DEFAULT 1 NOT NULL"#),
      "{probe}"
    );

    let view = pg.table_ddl("app", "recent_orders").await.unwrap();
    assert!(
      view.starts_with(r#"CREATE VIEW "app"."recent_orders" AS"#),
      "{view}"
    );
    assert!(view.contains("JOIN app.customers c"), "{view}");

    let matview = pg.table_ddl("app", "order_totals").await.unwrap();
    assert!(
      matview.starts_with(r#"CREATE MATERIALIZED VIEW "app"."order_totals" AS"#),
      "{matview}"
    );

    assert!(matches!(
      pg.table_ddl("app", "nope").await,
      Err(Error::NotFound { .. })
    ));
  }
}
