//! Dates in reports are UTC calendar dates.

use harness_usage::date::{day_start, is_date};

#[test]
fn a_date_is_checked_and_turned_into_the_start_of_its_day() {
    assert_eq!(day_start("1970-01-02"), Some(86_400));
    assert_eq!(day_start("2026-10-01"), Some(1_790_726_400 + 86_400));
    assert_eq!(day_start("2024-02-29"), Some(1_709_164_800));
}

#[test]
fn dates_that_do_not_exist_are_refused() {
    for bad in [
        "2026-02-29",
        "2026-13-01",
        "2026-00-10",
        "2026-10-32",
        "26-10-01",
        "2026-1-1",
        "yesterday",
        "",
    ] {
        assert!(!is_date(bad), "{bad}");
        assert_eq!(day_start(bad), None, "{bad}");
    }
    assert!(is_date("2026-10-01"));
}
