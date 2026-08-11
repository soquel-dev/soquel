#[derive(Debug, Clone, PartialEq)]
pub struct HistoryEntry {
  pub sql: String,
  pub at_ms: u64,
  pub duration_ms: f64,
  pub ok: bool,
}

const CAP: usize = 100;

/// Newest first; consecutive reruns of the same sql collapse into one entry.
pub fn push_history(mut entries: Vec<HistoryEntry>, entry: HistoryEntry) -> Vec<HistoryEntry> {
  if entries.first().is_some_and(|first| first.sql == entry.sql) {
    entries.remove(0);
  }
  entries.insert(0, entry);
  entries.truncate(CAP);
  entries
}

pub fn filter_history(entries: &[HistoryEntry], query: &str) -> Vec<HistoryEntry> {
  let needle = query.trim().to_lowercase();
  if needle.is_empty() {
    return entries.to_vec();
  }
  entries
    .iter()
    .filter(|entry| entry.sql.to_lowercase().contains(&needle))
    .cloned()
    .collect()
}

#[cfg(test)]
mod tests {
  use super::*;

  fn entry(sql: &str, at_ms: u64) -> HistoryEntry {
    HistoryEntry {
      sql: sql.to_string(),
      at_ms,
      duration_ms: 1.,
      ok: true,
    }
  }

  #[test]
  fn newest_first_and_consecutive_reruns_collapse() {
    let entries = push_history(Vec::new(), entry("select 1", 1));
    let entries = push_history(entries, entry("select 2", 2));
    let entries = push_history(entries, entry("select 2", 3));
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].sql, "select 2");
    assert_eq!(entries[0].at_ms, 3);
    assert_eq!(entries[1].sql, "select 1");
  }

  #[test]
  fn a_non_consecutive_rerun_keeps_both() {
    let entries = push_history(Vec::new(), entry("a", 1));
    let entries = push_history(entries, entry("b", 2));
    let entries = push_history(entries, entry("a", 3));
    assert_eq!(entries.len(), 3);
  }

  #[test]
  fn caps_at_one_hundred() {
    let mut entries = Vec::new();
    for i in 0..150 {
      entries = push_history(entries, entry(&format!("q{i}"), i));
    }
    assert_eq!(entries.len(), 100);
    assert_eq!(entries[0].sql, "q149");
  }

  #[test]
  fn filters_case_insensitively_and_blank_returns_all() {
    let entries = vec![entry("SELECT * FROM users", 1), entry("delete from t", 2)];
    assert_eq!(filter_history(&entries, "select").len(), 1);
    assert_eq!(filter_history(&entries, "  ").len(), 2);
  }
}
