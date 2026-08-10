use soquel_core::connectors::ColumnFilter;

#[derive(Debug, Clone)]
pub enum WorkspaceTab {
  Table {
    id: String,
    schema: String,
    table: String,
    initial_filters: Vec<ColumnFilter>,
  },
  Sql {
    id: String,
    title: String,
  },
}

impl WorkspaceTab {
  pub fn id(&self) -> &str {
    match self {
      WorkspaceTab::Table { id, .. } | WorkspaceTab::Sql { id, .. } => id,
    }
  }

  pub fn title(&self) -> String {
    match self {
      WorkspaceTab::Table { schema, table, .. } => format!("{schema}.{table}"),
      WorkspaceTab::Sql { title, .. } => title.clone(),
    }
  }
}

#[derive(Debug, Clone, Default)]
pub struct TabsState {
  pub tabs: Vec<WorkspaceTab>,
  pub active_id: Option<String>,
}

/// What the free tier allows per connection; a licence lifts it.
pub const FREE_TABS: usize = 2;

fn new_id() -> String {
  uuid::Uuid::new_v4().to_string()
}

/// Activates the existing tab for that table (replacing its initial filters),
/// or appends a new one. None when the limit refuses to open more.
///
/// Only opening is limited. Re-activating a tab that is already there adds
/// nothing, and refusing it would block navigation instead of a purchase.
pub fn open_table_tab(
  state: &TabsState,
  schema: &str,
  table: &str,
  filters: Vec<ColumnFilter>,
  limit: usize,
) -> Option<TabsState> {
  if let Some(existing) = state.tabs.iter().find(|tab| {
    matches!(tab, WorkspaceTab::Table { schema: s, table: t, .. } if s == schema && t == table)
  }) {
    let existing_id = existing.id().to_string();
    let tabs = state
      .tabs
      .iter()
      .map(|tab| {
        if tab.id() == existing_id {
          let WorkspaceTab::Table { id, schema, table, .. } = tab else {
            unreachable!()
          };
          WorkspaceTab::Table {
            id: id.clone(),
            schema: schema.clone(),
            table: table.clone(),
            initial_filters: filters.clone(),
          }
        } else {
          tab.clone()
        }
      })
      .collect();
    return Some(TabsState {
      tabs,
      active_id: Some(existing_id),
    });
  }
  if state.tabs.len() >= limit {
    return None;
  }
  let tab = WorkspaceTab::Table {
    id: new_id(),
    schema: schema.to_string(),
    table: table.to_string(),
    initial_filters: filters,
  };
  let active_id = Some(tab.id().to_string());
  let mut tabs = state.tabs.clone();
  tabs.push(tab);
  Some(TabsState { tabs, active_id })
}

/// None when the limit refuses to open more.
pub fn open_sql_tab(state: &TabsState, limit: usize) -> Option<TabsState> {
  if state.tabs.len() >= limit {
    return None;
  }
  let taken: Vec<u32> = state
    .tabs
    .iter()
    .filter_map(|tab| match tab {
      WorkspaceTab::Sql { title, .. } => title.strip_prefix("sql ")?.parse().ok(),
      _ => None,
    })
    .collect();
  let number = taken.iter().max().map_or(1, |max| max + 1);
  let tab = WorkspaceTab::Sql {
    id: new_id(),
    title: format!("sql {number}"),
  };
  let active_id = Some(tab.id().to_string());
  let mut tabs = state.tabs.clone();
  tabs.push(tab);
  Some(TabsState { tabs, active_id })
}

/// Next active tab = right neighbor, else left, else none.
pub fn close_tab(state: &TabsState, id: &str) -> TabsState {
  let Some(index) = state.tabs.iter().position(|tab| tab.id() == id) else {
    return state.clone();
  };
  let tabs: Vec<WorkspaceTab> = state
    .tabs
    .iter()
    .filter(|tab| tab.id() != id)
    .cloned()
    .collect();
  if state.active_id.as_deref() != Some(id) {
    return TabsState {
      tabs,
      active_id: state.active_id.clone(),
    };
  }
  let next = tabs
    .get(index)
    .or_else(|| tabs.get(index.wrapping_sub(1)))
    .map(|tab| tab.id().to_string());
  TabsState {
    tabs,
    active_id: next,
  }
}

pub fn activate_sibling(state: &TabsState, direction: i32) -> TabsState {
  if state.tabs.is_empty() {
    return state.clone();
  }
  let index = state
    .tabs
    .iter()
    .position(|tab| Some(tab.id()) == state.active_id.as_deref())
    .unwrap_or(0) as i32;
  let len = state.tabs.len() as i32;
  let next = (index + direction).rem_euclid(len) as usize;
  TabsState {
    tabs: state.tabs.clone(),
    active_id: Some(state.tabs[next].id().to_string()),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  const NO_LIMIT: usize = usize::MAX;

  fn open_table(state: &TabsState, schema: &str, table: &str, limit: usize) -> TabsState {
    open_table_tab(state, schema, table, Vec::new(), limit).unwrap()
  }

  #[test]
  fn appends_and_activates_a_new_table_tab() {
    let state = open_table(&TabsState::default(), "app", "customers", NO_LIMIT);
    assert_eq!(state.tabs.len(), 1);
    assert_eq!(state.active_id.as_deref(), Some(state.tabs[0].id()));
  }

  #[test]
  fn activates_the_existing_tab_and_replaces_its_filters_instead_of_duplicating() {
    let state = open_table(&TabsState::default(), "app", "customers", NO_LIMIT);
    let state = open_table(&state, "app", "orders", NO_LIMIT);
    let filters = vec![soquel_core::connectors::ColumnFilter {
      column: "id".into(),
      op: soquel_core::connectors::FilterOp::Eq,
      value: Some("1".into()),
    }];
    let state = open_table_tab(&state, "app", "customers", filters, NO_LIMIT).unwrap();

    assert_eq!(state.tabs.len(), 2);
    assert_eq!(state.active_id.as_deref(), Some(state.tabs[0].id()));
    let WorkspaceTab::Table {
      initial_filters, ..
    } = &state.tabs[0]
    else {
      panic!("expected a table tab");
    };
    assert_eq!(initial_filters.len(), 1);
  }

  #[test]
  fn numbers_editors_past_the_highest_existing_one() {
    let state = open_sql_tab(&TabsState::default(), NO_LIMIT).unwrap();
    let state = open_sql_tab(&state, NO_LIMIT).unwrap();
    let titles: Vec<String> = state.tabs.iter().map(|tab| tab.title()).collect();
    assert_eq!(titles, vec!["sql 1", "sql 2"]);

    let state = close_tab(&state, state.tabs[0].id());
    let state = open_sql_tab(&state, NO_LIMIT).unwrap();
    // "sql 2" is still open: the next editor must not reuse its number.
    let titles: Vec<String> = state.tabs.iter().map(|tab| tab.title()).collect();
    assert_eq!(titles, vec!["sql 2", "sql 3"]);
  }

  fn three() -> TabsState {
    let state = open_table(&TabsState::default(), "app", "a", NO_LIMIT);
    let state = open_table(&state, "app", "b", NO_LIMIT);
    open_table(&state, "app", "c", NO_LIMIT)
  }

  #[test]
  fn activates_the_right_neighbor_then_the_left_one_at_the_end() {
    let mut state = three();
    state.active_id = Some(state.tabs[1].id().to_string());
    let state = close_tab(&state, state.tabs[1].id());
    // was "c", shifted into slot 1
    assert_eq!(state.active_id.as_deref(), Some(state.tabs[1].id()));

    let mut state = state;
    state.active_id = Some(state.tabs[1].id().to_string());
    let state = close_tab(&state, state.tabs[1].id());
    assert_eq!(state.active_id.as_deref(), Some(state.tabs[0].id()));
  }

  #[test]
  fn keeps_the_active_tab_when_closing_another_one() {
    let state = three();
    let active = state.active_id.clone();
    let state = close_tab(&state, state.tabs[0].id());
    assert_eq!(state.active_id, active);
  }

  #[test]
  fn empties_cleanly() {
    let state = open_table(&TabsState::default(), "app", "a", NO_LIMIT);
    let state = close_tab(&state, state.tabs[0].id());
    assert!(state.tabs.is_empty());
    assert!(state.active_id.is_none());
  }

  #[test]
  fn cycles_in_both_directions_and_wraps() {
    let state = open_table(&TabsState::default(), "app", "a", NO_LIMIT);
    let state = open_table(&state, "app", "b", NO_LIMIT);

    let state = activate_sibling(&state, 1);
    assert_eq!(state.active_id.as_deref(), Some(state.tabs[0].id()));
    let state = activate_sibling(&state, -1);
    assert_eq!(state.active_id.as_deref(), Some(state.tabs[1].id()));
  }

  #[test]
  fn refuses_a_third_tab_of_either_kind() {
    let state = open_sql_tab(&TabsState::default(), FREE_TABS).unwrap();
    let full = open_sql_tab(&state, FREE_TABS).unwrap();
    assert_eq!(full.tabs.len(), 2);
    assert!(open_sql_tab(&full, FREE_TABS).is_none());
    assert!(open_table_tab(&full, "public", "orders", Vec::new(), FREE_TABS).is_none());
  }

  #[test]
  fn still_activates_a_tab_that_is_already_open() {
    // Re-activating opens nothing. Refusing it would block navigation between
    // the tabs someone already has, which is not what the limit is for.
    let opened = open_table(&TabsState::default(), "public", "users", FREE_TABS);
    let two = open_sql_tab(&opened, FREE_TABS).unwrap();
    assert_eq!(two.tabs.len(), 2);

    let back = open_table_tab(&two, "public", "users", Vec::new(), FREE_TABS).unwrap();

    assert_eq!(back.tabs.len(), 2);
    assert_eq!(back.active_id, opened.active_id);
  }
}
