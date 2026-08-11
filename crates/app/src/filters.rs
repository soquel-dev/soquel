use soquel_core::connectors::{ColumnFilter, ColumnKind, FilterOp};

const COMPARISONS: [FilterOp; 6] = [
  FilterOp::Eq,
  FilterOp::Neq,
  FilterOp::Lt,
  FilterOp::Lte,
  FilterOp::Gt,
  FilterOp::Gte,
];
const TEXTUAL: [FilterOp; 2] = [FilterOp::Contains, FilterOp::StartsWith];
const NULLNESS: [FilterOp; 2] = [FilterOp::IsNull, FilterOp::IsNotNull];

fn concat(parts: &[&[FilterOp]]) -> Vec<FilterOp> {
  parts.iter().flat_map(|ops| ops.iter().copied()).collect()
}

// Exhaustive: a new ColumnKind refuses to compile until it picks its operators.
pub fn ops_for_kind(kind: ColumnKind) -> Vec<FilterOp> {
  match kind {
    ColumnKind::Bool => concat(&[&[FilterOp::Eq, FilterOp::Neq], &NULLNESS]),
    ColumnKind::Number => concat(&[&COMPARISONS, &NULLNESS]),
    ColumnKind::Text => concat(&[&[FilterOp::Eq, FilterOp::Neq], &TEXTUAL, &NULLNESS]),
    ColumnKind::Json => concat(&[&TEXTUAL, &NULLNESS]),
    ColumnKind::Bytes => NULLNESS.to_vec(),
    ColumnKind::DateTime => concat(&[&COMPARISONS, &NULLNESS]),
    ColumnKind::Uuid => concat(&[&[FilterOp::Eq, FilterOp::Neq], &NULLNESS]),
    ColumnKind::Array => concat(&[&TEXTUAL, &NULLNESS]),
    ColumnKind::Other => concat(&[&[FilterOp::Eq, FilterOp::Neq], &TEXTUAL, &NULLNESS]),
  }
}

pub fn op_label(op: FilterOp) -> &'static str {
  match op {
    FilterOp::Eq => "=",
    FilterOp::Neq => "!=",
    FilterOp::Lt => "<",
    FilterOp::Lte => "<=",
    FilterOp::Gt => ">",
    FilterOp::Gte => ">=",
    FilterOp::Contains => "contains",
    FilterOp::StartsWith => "starts with",
    FilterOp::IsNull => "is null",
    FilterOp::IsNotNull => "is not null",
  }
}

pub fn op_needs_value(op: FilterOp) -> bool {
  !matches!(op, FilterOp::IsNull | FilterOp::IsNotNull)
}

pub fn filter_label(filter: &ColumnFilter) -> String {
  let op = op_label(filter.op);
  if op_needs_value(filter.op) {
    format!(
      "{} {} {}",
      filter.column,
      op,
      filter.value.as_deref().unwrap_or("")
    )
  } else {
    format!("{} {}", filter.column, op)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn bytes_only_offer_nullness() {
    assert_eq!(
      ops_for_kind(ColumnKind::Bytes),
      vec![FilterOp::IsNull, FilterOp::IsNotNull]
    );
  }

  #[test]
  fn text_offers_equality_textual_and_nullness() {
    let ops = ops_for_kind(ColumnKind::Text);
    assert!(ops.contains(&FilterOp::Contains));
    assert!(ops.contains(&FilterOp::StartsWith));
    assert!(ops.contains(&FilterOp::Eq));
    assert!(!ops.contains(&FilterOp::Lt));
  }

  #[test]
  fn number_offers_comparisons_not_textual() {
    let ops = ops_for_kind(ColumnKind::Number);
    assert!(ops.contains(&FilterOp::Lte));
    assert!(!ops.contains(&FilterOp::Contains));
  }

  #[test]
  fn label_includes_value_only_when_the_op_takes_one() {
    let with_value = ColumnFilter {
      column: "plan".into(),
      op: FilterOp::Eq,
      value: Some("pro".into()),
    };
    assert_eq!(filter_label(&with_value), "plan = pro");

    let nullness = ColumnFilter {
      column: "payload".into(),
      op: FilterOp::IsNull,
      value: None,
    };
    assert_eq!(filter_label(&nullness), "payload is null");
  }
}
