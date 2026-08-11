//! Import/export: the pure logic behind the transfer dialogs.

use std::path::PathBuf;

use soquel_core::transfer::{
  DuplicateStrategy, ExportSummary, ImportOutcome, ImportPreview, PreviewEntry,
};

pub const DEFAULT_EXPORT_NAME: &str = "connections.soquel";

pub const DUPLICATE_STRATEGIES: [DuplicateStrategy; 3] = [
  DuplicateStrategy::Skip,
  DuplicateStrategy::Replace,
  DuplicateStrategy::KeepBoth,
];

pub fn strategy_label(strategy: DuplicateStrategy) -> &'static str {
  match strategy {
    DuplicateStrategy::Skip => "Skip them",
    DuplicateStrategy::Replace => "Replace them",
    DuplicateStrategy::KeepBoth => "Keep both",
  }
}

pub fn strategy_hint(strategy: DuplicateStrategy) -> &'static str {
  match strategy {
    DuplicateStrategy::Skip => "Keep what is already here, ignore the file version.",
    DuplicateStrategy::Replace => "Overwrite the existing entry, its password included.",
    DuplicateStrategy::KeepBoth => "Import a second copy under a suffixed name.",
  }
}

/// A passphrase is only worth asking for when it can actually be re-typed right.
pub fn passphrase_issue(passphrase: &str, confirmation: &str) -> Option<&'static str> {
  if passphrase.chars().count() < 8 {
    return Some("Use at least 8 characters.");
  }
  if passphrase != confirmation {
    return Some("The two passphrases do not match.");
  }
  None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
  Connection,
  Tunnel,
}

#[derive(Debug, Clone)]
pub struct PlanEntry {
  pub kind: EntryKind,
  pub entry: PreviewEntry,
}

#[derive(Debug, Clone)]
pub struct ImportPlan {
  pub entries: Vec<PlanEntry>,
  pub duplicates: usize,
  pub problems: usize,
  pub secrets: usize,
  pub commands: usize,
}

/// Connections and tunnels read as one list: the counts the dialog announces
/// are about the file as a whole.
pub fn import_plan(preview: &ImportPreview) -> ImportPlan {
  let entries: Vec<PlanEntry> = preview
    .connections
    .iter()
    .map(|entry| PlanEntry {
      kind: EntryKind::Connection,
      entry: entry.clone(),
    })
    .chain(preview.tunnels.iter().map(|entry| PlanEntry {
      kind: EntryKind::Tunnel,
      entry: entry.clone(),
    }))
    .collect();
  ImportPlan {
    duplicates: entries.iter().filter(|e| e.entry.duplicate).count(),
    problems: entries.iter().filter(|e| e.entry.problem.is_some()).count(),
    secrets: entries.iter().filter(|e| e.entry.has_secret).count(),
    commands: entries.iter().filter(|e| e.entry.has_command).count(),
    entries,
  }
}

pub fn export_summary_message(summary: &ExportSummary) -> String {
  let mut parts = vec![plural(summary.connections, "connection")];
  if summary.tunnels > 0 {
    parts.push(plural(summary.tunnels, "tunnel"));
  }
  if summary.secrets > 0 {
    parts.push(plural(summary.secrets, "password"));
  }
  format!("Exported {}", parts.join(", "))
}

pub fn import_outcome_message(outcome: &ImportOutcome) -> String {
  let mut parts: Vec<String> = Vec::new();
  if outcome.created > 0 {
    parts.push(format!("{} added", outcome.created));
  }
  if outcome.replaced > 0 {
    parts.push(format!("{} replaced", outcome.replaced));
  }
  if outcome.skipped > 0 {
    parts.push(format!("{} skipped", outcome.skipped));
  }
  if outcome.tunnels_created > 0 {
    parts.push(plural(outcome.tunnels_created, "tunnel"));
  }
  if parts.is_empty() {
    "Nothing to import".to_string()
  } else {
    format!("Imported: {}", parts.join(", "))
  }
}

fn plural(count: u32, noun: &str) -> String {
  format!("{count} {noun}{}", if count == 1 { "" } else { "s" })
}

/// gpui's save prompt has no extension filters: a bare name gets the
/// extension, a deliberately typed one stays.
pub fn ensure_soquel_extension(mut path: PathBuf) -> PathBuf {
  if path.extension().is_none() {
    path.set_extension("soquel");
  }
  path
}

#[cfg(test)]
mod tests {
  use super::*;

  fn entry(name: &str) -> PreviewEntry {
    PreviewEntry {
      name: name.to_string(),
      target: "db.internal:5432/app".to_string(),
      has_secret: false,
      has_command: false,
      duplicate: false,
      problem: None,
    }
  }

  fn preview(connections: Vec<PreviewEntry>, tunnels: Vec<PreviewEntry>) -> ImportPreview {
    ImportPreview {
      encrypted: false,
      needs_passphrase: false,
      connections,
      tunnels,
    }
  }

  #[test]
  fn passphrase_issue_wants_length_then_agreement() {
    assert_eq!(
      passphrase_issue("short", "short"),
      Some("Use at least 8 characters.")
    );
    assert_eq!(
      passphrase_issue("long enough", "long enuff"),
      Some("The two passphrases do not match.")
    );
    assert_eq!(passphrase_issue("long enough", "long enough"), None);
  }

  #[test]
  fn import_plan_reads_the_file_as_one_list_tagged_by_kind() {
    let plan = import_plan(&preview(
      vec![entry("pg"), entry("mysql")],
      vec![entry("bastion")],
    ));
    let kinds: Vec<EntryKind> = plan.entries.iter().map(|e| e.kind).collect();
    assert_eq!(
      kinds,
      vec![
        EntryKind::Connection,
        EntryKind::Connection,
        EntryKind::Tunnel
      ]
    );
  }

  #[test]
  fn import_plan_counts_duplicates_secrets_commands_and_problems() {
    let plan = import_plan(&preview(
      vec![
        PreviewEntry {
          duplicate: true,
          has_secret: true,
          has_command: true,
          ..entry("pg")
        },
        PreviewEntry {
          problem: Some("broken".to_string()),
          ..entry("bad")
        },
      ],
      vec![PreviewEntry {
        duplicate: true,
        has_command: true,
        ..entry("bastion")
      }],
    ));
    assert_eq!(plan.duplicates, 2);
    assert_eq!(plan.secrets, 1);
    assert_eq!(plan.commands, 2);
    assert_eq!(plan.problems, 1);
  }

  #[test]
  fn export_summary_leaves_out_what_the_file_does_not_carry() {
    assert_eq!(
      export_summary_message(&ExportSummary {
        connections: 1,
        tunnels: 0,
        secrets: 0,
        encrypted: false,
      }),
      "Exported 1 connection"
    );
    assert_eq!(
      export_summary_message(&ExportSummary {
        connections: 4,
        tunnels: 2,
        secrets: 3,
        encrypted: true,
      }),
      "Exported 4 connections, 2 tunnels, 3 passwords"
    );
  }

  #[test]
  fn import_outcome_reports_only_what_happened() {
    assert_eq!(
      import_outcome_message(&ImportOutcome {
        created: 2,
        replaced: 0,
        skipped: 1,
        tunnels_created: 0,
      }),
      "Imported: 2 added, 1 skipped"
    );
    assert_eq!(
      import_outcome_message(&ImportOutcome::default()),
      "Nothing to import"
    );
    assert_eq!(
      import_outcome_message(&ImportOutcome {
        created: 1,
        replaced: 0,
        skipped: 0,
        tunnels_created: 1,
      }),
      "Imported: 1 added, 1 tunnel"
    );
  }

  #[test]
  fn ensure_soquel_extension_appends_only_when_bare() {
    assert_eq!(
      ensure_soquel_extension(PathBuf::from("/tmp/out")),
      PathBuf::from("/tmp/out.soquel")
    );
    assert_eq!(
      ensure_soquel_extension(PathBuf::from("/tmp/out.json")),
      PathBuf::from("/tmp/out.json")
    );
  }

  #[test]
  fn strategies_keep_their_order_and_copy() {
    assert_eq!(
      DUPLICATE_STRATEGIES,
      [
        DuplicateStrategy::Skip,
        DuplicateStrategy::Replace,
        DuplicateStrategy::KeepBoth
      ]
    );
    assert_eq!(strategy_label(DuplicateStrategy::Skip), "Skip them");
    assert_eq!(strategy_label(DuplicateStrategy::Replace), "Replace them");
    assert_eq!(strategy_label(DuplicateStrategy::KeepBoth), "Keep both");
    assert_eq!(
      strategy_hint(DuplicateStrategy::KeepBoth),
      "Import a second copy under a suffixed name."
    );
  }
}
