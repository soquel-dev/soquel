use std::collections::HashSet;

use serde_json::Value;
use soquel_core::connectors::QueryColumn;
use soquel_core::profiles::ConnectorKind;

#[derive(Debug, Clone, Default)]
pub struct PlanNode {
  /// Hierarchical path ("0", "0.1", ...): ancestor checks are prefix checks.
  pub id: String,
  pub depth: usize,
  pub node_type: String,
  /// "on app.events e", "using orders_pkey", ...
  pub target: Option<String>,
  pub condition: Option<String>,
  pub total_cost: f64,
  pub plan_rows: f64,
  /// Per-loop averages; None without ANALYZE.
  pub actual_rows: Option<f64>,
  /// Kept for parity with the webview model; the tree does not render it yet.
  #[allow(dead_code)]
  pub actual_loops: Option<f64>,
  /// Loop-adjusted totals.
  pub inclusive_ms: Option<f64>,
  pub exclusive_ms: Option<f64>,
  pub inclusive_cost: f64,
  pub exclusive_cost: f64,
  /// Exclusive share of the root total (time when analyzed, cost otherwise), 0..1.
  pub heat: f64,
  /// Planner estimate off by >= 10x versus actual rows.
  pub estimate_off: bool,
  pub never_executed: bool,
  pub children: Vec<PlanNode>,
}

#[derive(Debug, Clone)]
pub struct ExplainPlan {
  pub root: PlanNode,
  pub planning_ms: Option<f64>,
  pub execution_ms: Option<f64>,
  pub analyzed: bool,
}

const CONDITION_KEYS: [&str; 8] = [
  "Index Cond",
  "Recheck Cond",
  "Hash Cond",
  "Merge Cond",
  "Join Filter",
  "Filter",
  "Sort Key",
  "Group Key",
];

/// The dialect-specific SQL that produces a tree-renderable plan.
/// sqlite has no ANALYZE variant: EXPLAIN QUERY PLAN is the only tree form.
pub fn explain_sql(kind: ConnectorKind, analyze: bool, sql: &str) -> String {
  match kind {
    ConnectorKind::Sqlite => format!("EXPLAIN QUERY PLAN {sql}"),
    ConnectorKind::Mysql => {
      if analyze {
        format!("EXPLAIN ANALYZE {sql}")
      } else {
        format!("EXPLAIN FORMAT=JSON {sql}")
      }
    }
    _ => {
      if analyze {
        format!("EXPLAIN (ANALYZE, FORMAT JSON) {sql}")
      } else {
        format!("EXPLAIN (FORMAT JSON) {sql}")
      }
    }
  }
}

fn joined_rows(rows: &[Vec<Option<String>>]) -> String {
  rows
    .iter()
    .map(|row| row.first().and_then(|c| c.as_deref()).unwrap_or(""))
    .collect::<Vec<_>>()
    .join("\n")
    .trim()
    .to_string()
}

/// mysql's EXPLAIN ANALYZE speaks TREE text (no JSON on 8.0): render it as-is.
#[allow(dead_code)]
pub fn explain_tree_text(columns: &[QueryColumn], rows: &[Vec<Option<String>>]) -> Option<String> {
  if columns.len() != 1 || columns[0].name != "EXPLAIN" {
    return None;
  }
  let text = joined_rows(rows);
  text.starts_with("->").then_some(text)
}

/// None unless the statement is a plan result set (pg/mysql json, sqlite EQP).
pub fn parse_explain(
  columns: &[QueryColumn],
  rows: &[Vec<Option<String>>],
) -> Option<Vec<ExplainPlan>> {
  if is_sqlite_query_plan(columns) {
    return parse_sqlite_explain(rows);
  }
  if columns.len() != 1 {
    return None;
  }
  if columns[0].name == "EXPLAIN" {
    return parse_mysql_explain(rows);
  }
  if columns[0].name != "QUERY PLAN" {
    return None;
  }
  let text = joined_rows(rows);
  if !text.starts_with('[') {
    return None;
  }
  let entries: Value = serde_json::from_str(&text).ok()?;
  let entries = entries.as_array()?;

  let mut plans = Vec::new();
  for entry in entries {
    let raw = entry.get("Plan")?.as_object()?;
    let analyzed = raw.get("Actual Total Time").is_some_and(Value::is_number);
    let mut root = build_node(raw, "0".to_string(), 0);
    let basis = if analyzed {
      root.inclusive_ms.unwrap_or(0.)
    } else {
      root.inclusive_cost
    };
    apply_heat(&mut root, if basis > 0. { basis } else { 1. }, analyzed);
    plans.push(ExplainPlan {
      root,
      planning_ms: number_or_null(entry.get("Planning Time")),
      execution_ms: number_or_null(entry.get("Execution Time")),
      analyzed,
    });
  }
  (!plans.is_empty()).then_some(plans)
}

/// Ancestor check is a prefix check on the hierarchical ids; the collapsed
/// node itself stays visible ("0.1" hides "0.1.0" but not "0.10").
pub fn hidden_by_collapse(id: &str, collapsed: &HashSet<String>) -> bool {
  collapsed
    .iter()
    .any(|ancestor| id.starts_with(&format!("{ancestor}.")))
}

// -------- mysql: query_block / nested_loop / table shapes --------

const MYSQL_WRAPPERS: [(&str, &str); 7] = [
  ("ordering_operation", "Ordering"),
  ("grouping_operation", "Grouping"),
  ("duplicates_removal", "Distinct"),
  ("windowing", "Windowing"),
  ("buffer_result", "Buffer"),
  // mariadb spells its wrappers differently.
  ("filesort", "Filesort"),
  ("temporary_table", "Temporary table"),
];

fn mysql_access_type(access: &str) -> Option<&'static str> {
  Some(match access {
    "ALL" => "Table scan",
    "index" => "Index scan",
    "range" => "Range scan",
    "ref" => "Ref lookup",
    "eq_ref" => "Unique lookup",
    "const" | "system" => "Const row",
    "fulltext" => "Fulltext search",
    "index_merge" => "Index merge",
    "unique_subquery" => "Unique subquery",
    "index_subquery" => "Index subquery",
    _ => return None,
  })
}

type RawNode = serde_json::Map<String, Value>;

fn parse_mysql_explain(rows: &[Vec<Option<String>>]) -> Option<Vec<ExplainPlan>> {
  let text = joined_rows(rows);
  if !text.starts_with('{') {
    return None;
  }
  let parsed: Value = serde_json::from_str(&text).ok()?;
  let block = parsed.get("query_block")?.as_object()?;

  let mut root = mysql_query_block(block);
  // An unrecognized shape parses to a bare childless block: the flat table
  // is more honest than an empty tree.
  if root.children.is_empty() {
    return None;
  }
  finalize_mysql_plan(&mut root, "0".to_string(), 0);
  let basis = if root.inclusive_cost > 0. {
    root.inclusive_cost
  } else {
    1.
  };
  apply_heat(&mut root, basis, false);
  // FORMAT=JSON carries estimates only; ANALYZE speaks TREE text instead.
  Some(vec![ExplainPlan {
    root,
    planning_ms: None,
    execution_ms: None,
    analyzed: false,
  }])
}

fn mysql_query_block(block: &RawNode) -> PlanNode {
  let mut node = mysql_node("Query block", block);
  node.inclusive_cost = mysql_cost(block, "query_cost")
    .or_else(|| number_or_null(block.get("cost")))
    .unwrap_or(0.);
  node.total_cost = node.inclusive_cost;
  node
}

fn mysql_node(node_type: &str, raw: &RawNode) -> PlanNode {
  let children = mysql_children(raw);
  let mut flags = Vec::new();
  if raw.get("using_filesort") == Some(&Value::Bool(true)) {
    flags.push("using filesort");
  }
  if raw.get("using_temporary_table") == Some(&Value::Bool(true)) {
    flags.push("using temporary");
  }
  let cost = mysql_cost(raw, "prefix_cost")
    .or_else(|| mysql_cost(raw, "query_cost"))
    .or_else(|| number_or_null(raw.get("cost")));
  let inclusive_cost = cost.unwrap_or_else(|| {
    children
      .iter()
      .fold(0., |total, child| total.max(child.inclusive_cost))
  });
  let condition = if let Some(Value::String(filter)) = raw.get("attached_condition") {
    Some(format!("Filter: {filter}"))
  } else if let Some(Value::String(key)) = raw.get("sort_key") {
    Some(format!("Sort key: {key}"))
  } else if !flags.is_empty() {
    Some(flags.join(", "))
  } else {
    None
  };
  PlanNode {
    node_type: node_type.to_string(),
    target: mysql_target(raw),
    condition,
    total_cost: inclusive_cost,
    plan_rows: number_or_null(raw.get("rows_examined_per_scan"))
      .or_else(|| number_or_null(raw.get("rows")))
      .unwrap_or(0.),
    inclusive_cost,
    children,
    ..Default::default()
  }
}

fn mysql_children(raw: &RawNode) -> Vec<PlanNode> {
  let mut children = Vec::new();
  for (key, label) in MYSQL_WRAPPERS {
    if let Some(Value::Object(wrapped)) = raw.get(key) {
      children.push(mysql_node(label, wrapped));
    }
  }
  if let Some(Value::Array(loop_entries)) = raw.get("nested_loop") {
    let entries: Vec<&RawNode> = loop_entries
      .iter()
      .filter_map(|entry| entry.get("table")?.as_object())
      .collect();
    let mut tables: Vec<PlanNode> = entries.iter().map(|table| mysql_table(table)).collect();
    // mysql's prefix_cost is cumulative across join siblings: displayed cost
    // keeps the prefix, the heat math needs each table's own share (the
    // delta). mariadb reports plain per-table costs: sum them as-is.
    let cumulative = entries
      .iter()
      .any(|entry| mysql_cost(entry, "prefix_cost").is_some());
    let loop_cost;
    if cumulative {
      let mut previous_prefix: f64 = 0.;
      for table in &mut tables {
        let prefix = table.inclusive_cost;
        table.inclusive_cost = (prefix - previous_prefix).max(0.);
        previous_prefix = prefix.max(previous_prefix);
      }
      loop_cost = previous_prefix;
    } else {
      loop_cost = tables.iter().map(|table| table.inclusive_cost).sum();
    }
    let mut nested = mysql_node("Nested loop", &RawNode::new());
    nested.children = tables;
    nested.inclusive_cost = loop_cost;
    nested.total_cost = loop_cost;
    children.push(nested);
  }
  if let Some(Value::Object(table)) = raw.get("table") {
    children.push(mysql_table(table));
  }
  if let Some(Value::Array(subqueries)) = raw.get("attached_subqueries") {
    for entry in subqueries {
      if let Some(Value::Object(block)) = entry.get("query_block") {
        let mut subquery = mysql_node("Subquery", &RawNode::new());
        subquery.children = vec![mysql_query_block(block)];
        subquery.inclusive_cost = subquery.children[0].inclusive_cost;
        subquery.total_cost = subquery.inclusive_cost;
        children.push(subquery);
      }
    }
  }
  if let Some(Value::Object(materialized)) = raw.get("materialized_from_subquery")
    && let Some(Value::Object(block)) = materialized.get("query_block")
  {
    children.push(mysql_query_block(block));
  }
  if let Some(Value::Object(union)) = raw.get("union_result") {
    let branches: Vec<PlanNode> = union
      .get("query_specifications")
      .and_then(Value::as_array)
      .map(|specs| {
        specs
          .iter()
          .filter_map(|entry| entry.get("query_block")?.as_object())
          .map(mysql_query_block)
          .collect()
      })
      .unwrap_or_default();
    let mut result = mysql_node("Union", union);
    result.inclusive_cost = branches.iter().map(|b| b.inclusive_cost).sum();
    result.total_cost = result.inclusive_cost;
    result.children = branches;
    children.push(result);
  }
  children
}

fn mysql_table(raw: &RawNode) -> PlanNode {
  let access = raw
    .get("access_type")
    .and_then(Value::as_str)
    .unwrap_or_default();
  match mysql_access_type(access) {
    Some(label) => mysql_node(label, raw),
    None => mysql_node(
      &format!(
        "Access ({})",
        if access.is_empty() { "unknown" } else { access }
      ),
      raw,
    ),
  }
}

fn mysql_target(raw: &RawNode) -> Option<String> {
  let mut parts = Vec::new();
  if let Some(Value::String(name)) = raw.get("table_name") {
    parts.push(format!("on {name}"));
  }
  if let Some(Value::String(key)) = raw.get("key") {
    parts.push(format!("using {key}"));
  }
  (!parts.is_empty()).then(|| parts.join(" "))
}

fn mysql_cost(raw: &RawNode, key: &str) -> Option<f64> {
  let value = raw.get("cost_info")?.as_object()?.get(key)?;
  match value {
    Value::String(text) if !text.is_empty() => text.parse().ok(),
    other => number_or_null(Some(other)),
  }
}

/// mysql costs are cumulative (prefix_cost): same inclusive/exclusive math as pg.
fn finalize_mysql_plan(node: &mut PlanNode, id: String, depth: usize) {
  node.depth = depth;
  let children_cost: f64 = node.children.iter().map(|c| c.inclusive_cost).sum();
  node.exclusive_cost = (node.inclusive_cost - children_cost).max(0.);
  for (index, child) in node.children.iter_mut().enumerate() {
    finalize_mysql_plan(child, format!("{id}.{index}"), depth + 1);
  }
  node.id = id;
}

// -------- sqlite: EXPLAIN QUERY PLAN (id, parent, notused, detail) --------

fn is_sqlite_query_plan(columns: &[QueryColumn]) -> bool {
  columns.len() == 4
    && columns[0].name == "id"
    && columns[1].name == "parent"
    && columns[3].name == "detail"
}

fn parse_sqlite_explain(rows: &[Vec<Option<String>>]) -> Option<Vec<ExplainPlan>> {
  struct Entry {
    node: Option<PlanNode>,
    children: Vec<usize>,
  }
  let mut entries: Vec<Entry> = Vec::new();
  let mut slot_of: std::collections::HashMap<i64, usize> = std::collections::HashMap::new();
  let mut root_children: Vec<usize> = Vec::new();
  for row in rows {
    let id: i64 = row.first()?.as_deref()?.parse().ok()?;
    let parent: i64 = row.get(1)?.as_deref()?.parse().ok()?;
    let detail = row.get(3).and_then(|c| c.as_deref()).unwrap_or("");
    let slot = entries.len();
    entries.push(Entry {
      node: Some(sqlite_node(detail)),
      children: Vec::new(),
    });
    // parent references node ids; top-level rows point at 0, which never
    // exists, and an unknown parent lands on the root the same way.
    match slot_of.get(&parent) {
      Some(&parent_slot) => entries[parent_slot].children.push(slot),
      None => root_children.push(slot),
    }
    slot_of.insert(id, slot);
  }
  if entries.is_empty() {
    return None;
  }

  fn take(slot: usize, entries: &mut [Entry]) -> PlanNode {
    let mut node = entries[slot].node.take().expect("each slot attaches once");
    let children = std::mem::take(&mut entries[slot].children);
    for child in children {
      let child = take(child, entries);
      node.children.push(child);
    }
    node
  }

  let mut root = PlanNode {
    node_type: "Query plan".to_string(),
    ..Default::default()
  };
  for slot in root_children {
    let child = take(slot, &mut entries);
    root.children.push(child);
  }
  if root.children.is_empty() {
    return None;
  }
  finalize_sqlite_plan(&mut root, "0".to_string(), 0);
  // EQP carries no costs or rows: structure only.
  Some(vec![ExplainPlan {
    root,
    planning_ms: None,
    execution_ms: None,
    analyzed: false,
  }])
}

/// "SEARCH orders USING INDEX orders_pkey (id=?)" -> type/target/condition.
fn sqlite_node(detail: &str) -> PlanNode {
  let mut body = detail.trim().to_string();
  let mut condition = None;
  if body.ends_with(')')
    && let Some(open) = body.rfind('(')
    && open > 0
  {
    condition = Some(body[open + 1..body.len() - 1].to_string());
    body = body[..open].trim().to_string();
  }
  let mut node = PlanNode {
    node_type: if body.is_empty() {
      "Step".to_string()
    } else {
      body.clone()
    },
    ..Default::default()
  };
  let scan = if body.starts_with("SCAN ") {
    Some("SCAN")
  } else if body.starts_with("SEARCH ") {
    Some("SEARCH")
  } else {
    None
  };
  if let Some(scan) = scan {
    let rest = &body[scan.len() + 1..];
    node.node_type = if scan == "SCAN" {
      "Table scan".to_string()
    } else {
      "Index search".to_string()
    };
    node.target = Some(match rest.find(" USING ") {
      None => format!("on {rest}"),
      Some(using_at) => format!(
        "on {} using {}",
        &rest[..using_at],
        rest[using_at + " USING ".len()..].to_lowercase()
      ),
    });
  }
  node.condition = condition;
  node
}

fn finalize_sqlite_plan(node: &mut PlanNode, id: String, depth: usize) {
  node.depth = depth;
  for (index, child) in node.children.iter_mut().enumerate() {
    finalize_sqlite_plan(child, format!("{id}.{index}"), depth + 1);
  }
  node.id = id;
}

/// Pre-order flatten for flat rendering with indent guides.
pub fn flatten_plan(root: &PlanNode) -> Vec<&PlanNode> {
  let mut rows = Vec::new();
  fn walk<'a>(node: &'a PlanNode, rows: &mut Vec<&'a PlanNode>) {
    rows.push(node);
    for child in &node.children {
      walk(child, rows);
    }
  }
  walk(root, &mut rows);
  rows
}

fn build_node(raw: &RawNode, id: String, depth: usize) -> PlanNode {
  let children: Vec<PlanNode> = raw
    .get("Plans")
    .and_then(Value::as_array)
    .map(|plans| {
      plans
        .iter()
        .filter_map(Value::as_object)
        .enumerate()
        .map(|(index, child)| build_node(child, format!("{id}.{index}"), depth + 1))
        .collect()
    })
    .unwrap_or_default();

  let loops = number_or_null(raw.get("Actual Loops"));
  let per_loop_ms = number_or_null(raw.get("Actual Total Time"));
  let inclusive_ms = per_loop_ms.map(|ms| ms * loops.unwrap_or(1.));
  let children_ms: f64 = children
    .iter()
    .map(|child| child.inclusive_ms.unwrap_or(0.))
    .sum();
  let inclusive_cost = number_or_null(raw.get("Total Cost")).unwrap_or(0.);
  let children_cost: f64 = children.iter().map(|child| child.inclusive_cost).sum();

  let plan_rows = number_or_null(raw.get("Plan Rows")).unwrap_or(0.);
  let actual_rows = number_or_null(raw.get("Actual Rows"));
  let never_executed = loops == Some(0.);
  let estimate_off = actual_rows
    .is_some_and(|actual| !never_executed && off_by_tenfold(plan_rows.max(1.), actual.max(1.)));

  PlanNode {
    id,
    depth,
    node_type: raw
      .get("Node Type")
      .and_then(Value::as_str)
      .unwrap_or("Unknown")
      .to_string(),
    target: pg_target(raw),
    condition: pg_condition(raw),
    total_cost: inclusive_cost,
    plan_rows,
    actual_rows,
    actual_loops: loops,
    inclusive_ms,
    exclusive_ms: inclusive_ms.map(|ms| (ms - children_ms).max(0.)),
    inclusive_cost,
    exclusive_cost: (inclusive_cost - children_cost).max(0.),
    heat: 0.,
    estimate_off,
    never_executed,
    children,
  }
}

fn apply_heat(node: &mut PlanNode, basis: f64, analyzed: bool) {
  let exclusive = if analyzed {
    node.exclusive_ms.unwrap_or(0.)
  } else {
    node.exclusive_cost
  };
  node.heat = (exclusive / basis).min(1.);
  for child in &mut node.children {
    apply_heat(child, basis, analyzed);
  }
}

fn pg_target(raw: &RawNode) -> Option<String> {
  let mut parts: Vec<String> = Vec::new();
  let relation = raw
    .get("Relation Name")
    .or_else(|| raw.get("CTE Name"))
    .or_else(|| raw.get("Function Name"))
    .and_then(Value::as_str);
  if let Some(relation) = relation {
    let qualified = match raw.get("Schema").and_then(Value::as_str) {
      Some(schema) => format!("{schema}.{relation}"),
      None => relation.to_string(),
    };
    let alias = raw.get("Alias").and_then(Value::as_str);
    parts.push(match alias {
      Some(alias) if alias != relation => format!("on {qualified} {alias}"),
      _ => format!("on {qualified}"),
    });
  }
  if let Some(index) = raw.get("Index Name").and_then(Value::as_str) {
    parts.push(format!("using {index}"));
  }
  if let Some(join) = raw.get("Join Type").and_then(Value::as_str)
    && join != "Inner"
  {
    parts.insert(0, join.to_lowercase());
  }
  (!parts.is_empty()).then(|| parts.join(" "))
}

fn pg_condition(raw: &RawNode) -> Option<String> {
  for key in CONDITION_KEYS {
    match raw.get(key) {
      Some(Value::String(value)) => return Some(format!("{key}: {value}")),
      Some(Value::Array(values)) if !values.is_empty() => {
        let joined = values
          .iter()
          .filter_map(Value::as_str)
          .collect::<Vec<_>>()
          .join(", ");
        return Some(format!("{key}: {joined}"));
      }
      _ => {}
    }
  }
  None
}

fn off_by_tenfold(a: f64, b: f64) -> bool {
  a.max(b) / a.min(b) >= 10.
}

fn number_or_null(value: Option<&Value>) -> Option<f64> {
  value.and_then(Value::as_f64).filter(|n| n.is_finite())
}

pub fn format_ms(ms: f64) -> String {
  if ms >= 1000. {
    format!("{:.2}s", ms / 1000.)
  } else if ms >= 100. {
    format!("{ms:.0}ms")
  } else if ms >= 10. {
    format!("{ms:.1}ms")
  } else {
    format!("{ms:.2}ms")
  }
}

pub fn format_rows(rows: f64) -> String {
  crate::format::format_estimated_rows(rows)
}

#[cfg(test)]
mod tests {
  use soquel_core::connectors::ColumnKind;

  use super::*;

  fn column(name: &str, data_type: Option<&str>, kind: ColumnKind) -> QueryColumn {
    QueryColumn {
      name: name.to_string(),
      data_type: data_type.map(Into::into),
      kind,
    }
  }

  fn query_plan_columns() -> Vec<QueryColumn> {
    vec![column("QUERY PLAN", Some("jsonb"), ColumnKind::Json)]
  }

  fn rows_of(text: &str) -> Vec<Vec<Option<String>>> {
    text
      .trim_end()
      .split('\n')
      .map(|line| vec![Some(line.to_string())])
      .collect()
  }

  const ANALYZED: &str = r#"[
    {
      "Planning Time": 0.2,
      "Execution Time": 10,
      "Plan": {
        "Node Type": "Hash Join",
        "Join Type": "Inner",
        "Hash Cond": "(o.customer_id = c.id)",
        "Total Cost": 100,
        "Plan Rows": 1000,
        "Actual Rows": 900,
        "Actual Loops": 1,
        "Actual Total Time": 9,
        "Plans": [
          {
            "Node Type": "Seq Scan",
            "Relation Name": "orders",
            "Schema": "app",
            "Alias": "o",
            "Filter": "(amount > 0)",
            "Total Cost": 60,
            "Plan Rows": 10,
            "Actual Rows": 5000,
            "Actual Loops": 1,
            "Actual Total Time": 6
          },
          {
            "Node Type": "Hash",
            "Total Cost": 30,
            "Plan Rows": 100,
            "Actual Rows": 0,
            "Actual Loops": 0,
            "Actual Total Time": 0,
            "Plans": [
              {
                "Node Type": "Index Only Scan",
                "Relation Name": "customers",
                "Schema": "app",
                "Alias": "customers",
                "Index Name": "customers_pkey",
                "Total Cost": 20,
                "Plan Rows": 100,
                "Actual Rows": 100,
                "Actual Loops": 2,
                "Actual Total Time": 1
              }
            ]
          }
        ]
      }
    }
  ]"#;

  #[test]
  fn rejects_non_explain_statements_and_text_format_plans() {
    assert!(parse_explain(&[], &[]).is_none());
    assert!(
      parse_explain(
        &[column("QUERY PLAN", Some("text"), ColumnKind::Text)],
        &[vec![Some(
          "Seq Scan on orders  (cost=0.00..1.00 rows=1 width=4)".to_string()
        )]],
      )
      .is_none()
    );
    assert!(
      parse_explain(
        &[column("id", Some("int4"), ColumnKind::Number)],
        &[vec![Some("1".to_string())]],
      )
      .is_none()
    );
  }

  #[test]
  fn parses_an_analyzed_plan_with_loop_adjusted_exclusive_times() {
    let plans = parse_explain(&query_plan_columns(), &[vec![Some(ANALYZED.to_string())]]).unwrap();
    assert_eq!(plans.len(), 1);
    let plan = &plans[0];
    assert!(plan.analyzed);
    assert_eq!(plan.planning_ms, Some(0.2));
    assert_eq!(plan.execution_ms, Some(10.));

    let root = &plan.root;
    assert_eq!(root.node_type, "Hash Join");
    assert_eq!(
      root.condition.as_deref(),
      Some("Hash Cond: (o.customer_id = c.id)")
    );
    // 9 - (6 + 0) = 3; the index scan (1ms x 2 loops) nests under Hash.
    assert_eq!(root.exclusive_ms, Some(3.));

    let scan = &root.children[0];
    let hash = &root.children[1];
    assert_eq!(scan.target.as_deref(), Some("on app.orders o"));
    assert_eq!(scan.condition.as_deref(), Some("Filter: (amount > 0)"));
    // 10 estimated vs 5000 actual: off by 10x or more.
    assert!(scan.estimate_off);
    assert!((scan.heat - 6. / 9.).abs() < 1e-9);

    assert!(hash.never_executed);
    assert!(!hash.estimate_off);
    let index = &hash.children[0];
    assert_eq!(
      index.target.as_deref(),
      Some("on app.customers using customers_pkey")
    );
    assert_eq!(index.inclusive_ms, Some(2.));
    assert_eq!(index.id, "0.1.0");
  }

  #[test]
  fn falls_back_to_cost_heat_without_analyze() {
    let raw = r#"[{"Plan": {"Node Type": "Seq Scan", "Relation Name": "events", "Total Cost": 200, "Plan Rows": 10000}}]"#;
    let plans = parse_explain(&query_plan_columns(), &[vec![Some(raw.to_string())]]).unwrap();
    let plan = &plans[0];
    assert!(!plan.analyzed);
    assert!(plan.root.inclusive_ms.is_none());
    assert_eq!(plan.root.heat, 1.);
    assert!(!plan.root.estimate_off);
  }

  #[test]
  fn joins_multi_row_json_output_before_parsing() {
    let pretty: String =
      serde_json::to_string_pretty(&serde_json::from_str::<Value>(ANALYZED).unwrap()).unwrap();
    let plans = parse_explain(
      &[column("QUERY PLAN", None, ColumnKind::Other)],
      &rows_of(&pretty),
    )
    .unwrap();
    assert_eq!(plans[0].root.node_type, "Hash Join");
  }

  // Captured from the seeded test databases: the real field names and shapes.
  fn invariants(raw: &str) -> Vec<ExplainPlan> {
    let plans = parse_explain(&query_plan_columns(), &rows_of(raw)).unwrap();
    for plan in &plans {
      let nodes = flatten_plan(&plan.root);
      let ids: HashSet<&str> = nodes.iter().map(|node| node.id.as_str()).collect();
      assert_eq!(ids.len(), nodes.len());
      for node in nodes {
        assert!(node.heat >= 0. && node.heat <= 1.);
        assert!(node.exclusive_cost >= 0.);
        if let Some(ms) = node.exclusive_ms {
          assert!(ms.is_finite() && ms >= 0.);
        }
      }
    }
    plans
  }

  #[test]
  fn handles_a_join_aggregate_sort_analyze_plan() {
    let plans = invariants(include_str!("fixtures/explain-join-sort.json"));
    assert!(plans[0].analyzed);
    assert!(plans[0].execution_ms.is_some());
    let nodes = flatten_plan(&plans[0].root);
    assert!(nodes.iter().any(|node| node.node_type == "Hash Join"));
    // No VERBOSE: postgres omits Schema, targets are unqualified.
    assert!(
      nodes
        .iter()
        .any(|node| node.target.as_deref() == Some("on orders o"))
    );
  }

  #[test]
  fn handles_a_parallel_gather_plan() {
    let plans = invariants(include_str!("fixtures/explain-parallel.json"));
    let nodes = flatten_plan(&plans[0].root);
    let gather = nodes
      .iter()
      .find(|node| node.node_type == "Gather")
      .unwrap();
    assert!(!gather.children.is_empty());
  }

  #[test]
  fn handles_cte_and_initplan_children() {
    let plans = invariants(include_str!("fixtures/explain-subplan.json"));
    let nodes = flatten_plan(&plans[0].root);
    assert!(nodes.iter().any(|node| node.node_type == "CTE Scan"));
    assert!(nodes.len() >= 4);
  }

  #[test]
  fn handles_a_cost_only_plan() {
    let plans = invariants(include_str!("fixtures/explain-cost-only.json"));
    assert!(!plans[0].analyzed);
    assert!(plans[0].execution_ms.is_none());
    let nodes = flatten_plan(&plans[0].root);
    assert!(nodes.iter().all(|node| node.inclusive_ms.is_none()));
    assert!(plans[0].root.heat > 0.);
  }

  fn mysql_columns() -> Vec<QueryColumn> {
    vec![column("EXPLAIN", Some("json"), ColumnKind::Json)]
  }

  #[test]
  fn parses_wrappers_nested_loops_and_table_access_types() {
    let raw = include_str!("fixtures/explain-mysql-join.json");
    let plans = parse_explain(&mysql_columns(), &[vec![Some(raw.trim_end().to_string())]]).unwrap();
    assert_eq!(plans.len(), 1);
    let root = &plans[0].root;
    assert!(!plans[0].analyzed);
    assert_eq!(root.node_type, "Query block");
    assert!((root.inclusive_cost - 1.65).abs() < 1e-9);

    let nodes = flatten_plan(root);
    let types: Vec<&str> = nodes.iter().map(|node| node.node_type.as_str()).collect();
    assert!(types.contains(&"Ordering"));
    assert!(types.contains(&"Grouping"));
    assert!(types.contains(&"Nested loop"));
    assert!(types.contains(&"Table scan"));
    assert!(types.contains(&"Unique lookup"));

    let scan = nodes
      .iter()
      .find(|node| node.node_type == "Table scan")
      .unwrap();
    assert_eq!(scan.target.as_deref(), Some("on o"));
    assert!(scan.condition.as_deref().unwrap().contains("amount"));
    let lookup = nodes
      .iter()
      .find(|node| node.node_type == "Unique lookup")
      .unwrap();
    assert_eq!(lookup.target.as_deref(), Some("on c using PRIMARY"));
    // Displayed cost keeps mysql's cumulative prefix_cost.
    assert!((lookup.total_cost - 1.65).abs() < 1e-9);

    let ids: HashSet<&str> = nodes.iter().map(|node| node.id.as_str()).collect();
    assert_eq!(ids.len(), nodes.len());
    for node in &nodes {
      assert!(node.heat >= 0. && node.heat <= 1.);
      assert!(node.exclusive_cost >= 0.);
    }
  }

  #[test]
  fn attaches_subqueries_as_child_query_blocks() {
    let raw = include_str!("fixtures/explain-mysql-subquery.json");
    let plans = parse_explain(&mysql_columns(), &[vec![Some(raw.trim_end().to_string())]]).unwrap();
    let nodes = flatten_plan(&plans[0].root);
    assert!(nodes.iter().any(|node| node.node_type == "Subquery"));
    assert!(
      nodes
        .iter()
        .filter(|node| node.node_type == "Query block")
        .count()
        >= 2
    );
  }

  #[test]
  fn expands_union_branches_as_child_query_blocks() {
    let raw = include_str!("fixtures/explain-mysql-union.json");
    let plans = parse_explain(&mysql_columns(), &[vec![Some(raw.trim_end().to_string())]]).unwrap();
    let nodes = flatten_plan(&plans[0].root);
    let union = nodes.iter().find(|node| node.node_type == "Union").unwrap();
    assert_eq!(union.children.len(), 2);
    assert_eq!(union.condition.as_deref(), Some("using temporary"));
    assert!(
      nodes
        .iter()
        .filter(|node| node.node_type == "Table scan")
        .count()
        >= 2
    );
    assert!(union.inclusive_cost > 0.);
  }

  #[test]
  fn expands_derived_tables_through_materialized_from_subquery() {
    let raw = include_str!("fixtures/explain-mysql-derived.json");
    let plans = parse_explain(&mysql_columns(), &[vec![Some(raw.trim_end().to_string())]]).unwrap();
    let nodes = flatten_plan(&plans[0].root);
    assert!(
      nodes
        .iter()
        .any(|node| node.target.as_deref() == Some("on d"))
    );
    assert!(
      nodes
        .iter()
        .filter(|node| node.node_type == "Query block")
        .count()
        >= 2
    );
    assert!(nodes.iter().any(|node| {
      node
        .target
        .as_deref()
        .is_some_and(|t| t.contains("on events"))
    }));
  }

  #[test]
  fn parses_the_mariadb_flavor_bare_costs_filesort_temporary_wrappers() {
    let raw = include_str!("fixtures/explain-mariadb-join.json");
    let plans = parse_explain(&mysql_columns(), &[vec![Some(raw.trim_end().to_string())]]).unwrap();
    let root = &plans[0].root;
    assert!((root.inclusive_cost - 0.0148).abs() < 1e-3);

    let nodes = flatten_plan(root);
    let types: Vec<&str> = nodes.iter().map(|node| node.node_type.as_str()).collect();
    assert!(types.contains(&"Filesort"));
    assert!(types.contains(&"Temporary table"));
    assert!(types.contains(&"Nested loop"));
    let filesort = nodes
      .iter()
      .find(|node| node.node_type == "Filesort")
      .unwrap();
    assert!(filesort.condition.as_deref().unwrap().contains("Sort key"));
    let lookup = nodes
      .iter()
      .find(|node| node.node_type == "Unique lookup")
      .unwrap();
    assert_eq!(lookup.target.as_deref(), Some("on c using PRIMARY"));
    // Per-table costs are NOT cumulative here: the loop sums them.
    let nested = nodes
      .iter()
      .find(|node| node.node_type == "Nested loop")
      .unwrap();
    assert!((nested.inclusive_cost - (0.0113438 + 0.00350252)).abs() < 1e-5);
    for node in &nodes {
      assert!(node.heat >= 0. && node.heat <= 1.);
    }
  }

  #[test]
  fn degrades_to_none_on_an_unrecognized_json_shape() {
    let alien = r#"{"query_block": {"select_id": 1, "something_new": {"x": 1}}}"#;
    assert!(parse_explain(&mysql_columns(), &[vec![Some(alien.to_string())]]).is_none());
  }

  #[test]
  fn detects_the_explain_analyze_tree_text_and_rejects_json_for_it() {
    let tree = "-> Aggregate: count(0)  (cost=2.01 rows=1)\n    -> Table scan on events";
    let columns = vec![column("EXPLAIN", Some("text"), ColumnKind::Text)];
    let rows = vec![vec![Some(tree.to_string())]];
    assert_eq!(explain_tree_text(&columns, &rows).as_deref(), Some(tree));
    assert!(parse_explain(&columns, &rows).is_none());
    let json = include_str!("fixtures/explain-mysql-join.json");
    assert!(
      explain_tree_text(&mysql_columns(), &[vec![Some(json.trim_end().to_string())]]).is_none()
    );
  }

  #[test]
  fn wraps_per_dialect() {
    assert_eq!(
      explain_sql(ConnectorKind::Postgres, false, "select 1"),
      "EXPLAIN (FORMAT JSON) select 1"
    );
    assert_eq!(
      explain_sql(ConnectorKind::Postgres, true, "select 1"),
      "EXPLAIN (ANALYZE, FORMAT JSON) select 1"
    );
    assert_eq!(
      explain_sql(ConnectorKind::Mysql, false, "select 1"),
      "EXPLAIN FORMAT=JSON select 1"
    );
    assert_eq!(
      explain_sql(ConnectorKind::Mysql, true, "select 1"),
      "EXPLAIN ANALYZE select 1"
    );
    // sqlite has no analyze variant: both spellings collapse to EQP.
    assert_eq!(
      explain_sql(ConnectorKind::Sqlite, false, "select 1"),
      "EXPLAIN QUERY PLAN select 1"
    );
    assert_eq!(
      explain_sql(ConnectorKind::Sqlite, true, "select 1"),
      "EXPLAIN QUERY PLAN select 1"
    );
  }

  fn eqp_columns() -> Vec<QueryColumn> {
    vec![
      column("id", Some("integer"), ColumnKind::Number),
      column("parent", Some("integer"), ColumnKind::Number),
      column("notused", Some("integer"), ColumnKind::Number),
      column("detail", Some("text"), ColumnKind::Text),
    ]
  }

  fn eqp_row(id: &str, parent: &str, detail: &str) -> Vec<Option<String>> {
    vec![
      Some(id.to_string()),
      Some(parent.to_string()),
      Some("0".to_string()),
      Some(detail.to_string()),
    ]
  }

  #[test]
  fn builds_the_tree_from_parent_links_and_splits_scan_details() {
    let plans = parse_explain(
      &eqp_columns(),
      &[
        eqp_row("4", "0", "SCAN o"),
        eqp_row("11", "0", "SEARCH c USING INTEGER PRIMARY KEY (rowid=?)"),
        eqp_row("20", "0", "USE TEMP B-TREE FOR ORDER BY"),
      ],
    )
    .unwrap();
    let root = &plans[0].root;
    assert!(!plans[0].analyzed);
    assert_eq!(root.node_type, "Query plan");
    assert_eq!(root.children.len(), 3);
    assert_eq!(root.children[0].node_type, "Table scan");
    assert_eq!(root.children[0].target.as_deref(), Some("on o"));
    assert_eq!(root.children[1].node_type, "Index search");
    assert_eq!(
      root.children[1].target.as_deref(),
      Some("on c using integer primary key")
    );
    assert_eq!(root.children[1].condition.as_deref(), Some("rowid=?"));
    assert_eq!(root.children[2].node_type, "USE TEMP B-TREE FOR ORDER BY");
    assert_eq!(root.children[1].id, "0.1");
  }

  #[test]
  fn nests_children_under_their_parent_id() {
    let plans = parse_explain(
      &eqp_columns(),
      &[
        eqp_row("1", "0", "COMPOUND QUERY"),
        eqp_row("2", "1", "LEFT-MOST SUBQUERY"),
        eqp_row("5", "2", "SCAN customers"),
        eqp_row("8", "1", "UNION ALL"),
        eqp_row("10", "8", "SCAN orders"),
      ],
    )
    .unwrap();
    let compound = &plans[0].root.children[0];
    assert_eq!(compound.node_type, "COMPOUND QUERY");
    assert_eq!(compound.children.len(), 2);
    let scan = &compound.children[1].children[0];
    assert_eq!(scan.node_type, "Table scan");
    assert_eq!(scan.target.as_deref(), Some("on orders"));
    assert_eq!(scan.depth, 3);
  }

  #[test]
  fn returns_none_on_an_empty_plan_and_ignores_lookalike_shapes() {
    assert!(parse_explain(&eqp_columns(), &[]).is_none());
    assert!(
      parse_explain(
        &[column("id", Some("int4"), ColumnKind::Number)],
        &[vec![Some("1".to_string())]],
      )
      .is_none()
    );
  }

  #[test]
  fn hides_descendants_only_and_never_trips_on_sibling_prefixes() {
    let collapsed: HashSet<String> = ["0.1".to_string()].into();
    assert!(hidden_by_collapse("0.1.0", &collapsed));
    assert!(hidden_by_collapse("0.1.0.2", &collapsed));
    // The collapsed node itself stays visible.
    assert!(!hidden_by_collapse("0.1", &collapsed));
    // "0.10" shares the string prefix but is a sibling, not a child.
    assert!(!hidden_by_collapse("0.10", &collapsed));
    assert!(!hidden_by_collapse("0.0", &HashSet::new()));
  }

  #[test]
  fn yields_pre_order_rows_with_hierarchical_ids() {
    let plans = parse_explain(&query_plan_columns(), &[vec![Some(ANALYZED.to_string())]]).unwrap();
    let rows = flatten_plan(&plans[0].root);
    let ids: Vec<&str> = rows.iter().map(|row| row.id.as_str()).collect();
    assert_eq!(ids, vec!["0", "0.0", "0.1", "0.1.0"]);
    let depths: Vec<usize> = rows.iter().map(|row| row.depth).collect();
    assert_eq!(depths, vec![0, 1, 1, 2]);
  }

  #[test]
  fn scales_precision_with_magnitude() {
    assert_eq!(format_ms(0.056), "0.06ms");
    assert_eq!(format_ms(12.34), "12.3ms");
    assert_eq!(format_ms(456.7), "457ms");
    assert_eq!(format_ms(1234.), "1.23s");
  }
}
