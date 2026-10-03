//! 1.1: the crate builds with a bundled SQLite, so the usage cache needs nothing installed.

#[test]
fn the_cache_uses_a_bundled_sqlite() {
    let version = harness_usage::sqlite_version();
    assert!(version.starts_with("3."), "{version}");
}

#[test]
fn an_in_memory_cache_opens_and_answers_a_query() {
    let n = harness_usage::sqlite_smoke().unwrap();
    assert_eq!(n, 2);
}
