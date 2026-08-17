//! Tests for relative-time formatting used by the time-machine timeline (crate root).

use super::*;

fn at(s: &str) -> chrono::NaiveDateTime {
    chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").unwrap()
}

#[test]
fn buckets_seconds_minutes_hours_days() {
    let now = at("2026-07-08 12:00:00");
    assert_eq!(rel_time_from("2026-07-08 11:59:30", now), "30s ago");
    assert_eq!(rel_time_from("2026-07-08 11:58:00", now), "2m ago");
    assert_eq!(rel_time_from("2026-07-08 09:00:00", now), "3h ago");
    assert_eq!(rel_time_from("2026-07-05 12:00:00", now), "3d ago");
}

#[test]
fn future_and_now_and_malformed() {
    let now = at("2026-07-08 12:00:00");
    // A clock-skewed future timestamp degrades to "just now", not a negative age.
    assert_eq!(rel_time_from("2026-07-08 12:00:30", now), "just now");
    assert_eq!(rel_time_from("2026-07-08 12:00:00", now), "0s ago");
    // Unparseable input falls back to the raw string.
    assert_eq!(rel_time_from("not-a-date", now), "not-a-date");
}
