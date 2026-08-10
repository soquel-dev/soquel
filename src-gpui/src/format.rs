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

#[cfg(test)]
mod tests {
  use super::*;

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
