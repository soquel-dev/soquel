/// Planner estimate: negative means never analyzed, shown as nothing.
/// Mirrors the webview's Intl compact notation (12, 1.2K, 1.5M, 2B).
pub fn format_estimated_rows(estimate: f64) -> String {
  if !estimate.is_finite() || estimate < 0. {
    return String::new();
  }
  let value = estimate.round();
  let (scaled, suffix) = if value >= 1e9 {
    (value / 1e9, "B")
  } else if value >= 1e6 {
    (value / 1e6, "M")
  } else if value >= 1e3 {
    (value / 1e3, "K")
  } else {
    return format!("{value:.0}");
  };
  let rounded = (scaled * 10.).round() / 10.;
  if rounded.fract() == 0. {
    format!("{rounded:.0}{suffix}")
  } else {
    format!("{rounded:.1}{suffix}")
  }
}

/// An RFC 3339 timestamp to a medium day like "Jan 15, 2027"; None when it does
/// not parse. Mirrors the webview's `formatDay` (Intl medium date).
pub fn format_day(raw: &str) -> Option<String> {
  const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
  ];
  let mut parts = raw.get(..10)?.split('-');
  let year: i32 = parts.next()?.parse().ok()?;
  let month: usize = parts.next()?.parse().ok()?;
  let day: u32 = parts.next()?.parse().ok()?;
  let name = MONTHS.get(month.checked_sub(1)?)?;
  Some(format!("{name} {day}, {year}"))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn format_day_reads_the_date_part() {
    assert_eq!(
      format_day("2027-01-15T00:00:00Z").as_deref(),
      Some("Jan 15, 2027")
    );
    assert_eq!(format_day("2026-12-03").as_deref(), Some("Dec 3, 2026"));
    assert_eq!(format_day("not-a-date"), None);
    assert_eq!(format_day("2027-13-01T00:00:00Z"), None);
  }

  #[test]
  fn hides_unanalyzed_and_small_numbers_stay_plain() {
    assert_eq!(format_estimated_rows(-1.), "");
    assert_eq!(format_estimated_rows(0.), "0");
    assert_eq!(format_estimated_rows(999.), "999");
  }

  #[test]
  fn compacts_with_one_decimal_at_most() {
    assert_eq!(format_estimated_rows(1234.), "1.2K");
    assert_eq!(format_estimated_rows(2000.), "2K");
    assert_eq!(format_estimated_rows(1_500_000.), "1.5M");
    assert_eq!(format_estimated_rows(1_000_000_000.), "1B");
  }
}
