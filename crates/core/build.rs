use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
  // A licence covers builds released inside its window, and that check runs
  // offline: a binary that cannot say when it was made cannot tell whether a
  // licence covers it. Nothing can supply this after the fact.
  let seconds = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .expect("system clock is before 1970")
    .as_secs();
  let built = time::OffsetDateTime::from_unix_timestamp(seconds as i64)
    .expect("build timestamp out of range")
    .format(&time::format_description::well_known::Rfc3339)
    .expect("rfc3339 formatting");

  println!("cargo:rustc-env=SOQUEL_BUILD_DATE={built}");
}
