//! 1.11: `usage export` and `usage forget`.

use harness_core::meter::AccountKind;
use harness_usage::{
    export::{Format, export, forget},
    ledger::{Ledger, LedgerRecord},
    paths::Dirs,
    store::{Group, Query, Store},
};

/// 2026-09-30 12:00 UTC, then each day after.
const SEPT_30: u64 = 1_790_769_600;
const DAY: u64 = 86_400;

fn record(t: u64, model: &str) -> LedgerRecord {
    LedgerRecord {
        v: 1,
        t,
        session: "s".into(),
        project: "p".into(),
        role: "main".into(),
        model: model.into(),
        account: AccountKind::ApiKey,
        input: 10,
        cache_read: 0,
        cache_write: 0,
        cache_write_1h: 0,
        output: 2,
        reasoning: 0,
        billed_usd: Some(0.5),
        list_usd: Some(0.5),
        price: Some("embedded 2026-10-03".into()),
        ms: 7,
        outcome: "ok".into(),
        window: None,
    }
}

/// Records on 09-30 (September), 10-01 and 10-02, outcome and window files for both months.
fn seeded(data: &std::path::Path) -> Dirs {
    let dirs = Dirs::under(data);
    let ledger = Ledger::new(&dirs.usage);
    for (i, model) in ["openai/gpt-5", "ollama/qwen3-coder", "openai/gpt-5"]
        .iter()
        .enumerate()
    {
        ledger
            .append(&record(SEPT_30 + i as u64 * DAY, model))
            .unwrap();
    }
    std::fs::create_dir_all(&dirs.outcomes).unwrap();
    for (month, t) in [("2026-09", SEPT_30), ("2026-10", SEPT_30 + DAY)] {
        std::fs::write(
            dirs.outcomes.join(format!("{month}.jsonl")),
            format!("{{\"t\":{t}}}\n{{\"t\":{}}}\n", t + 60),
        )
        .unwrap();
    }
    std::fs::write(
        dirs.usage.join("windows-2026-09.jsonl"),
        format!("{{\"id\":\"w\",\"t\":{SEPT_30}}}\n"),
    )
    .unwrap();
    dirs
}

fn lines(format: Format, since: Option<&str>, data: &std::path::Path) -> Vec<String> {
    let mut out = Vec::new();
    export(&Dirs::under(data), since, format, &mut out).unwrap();
    String::from_utf8(out)
        .unwrap()
        .lines()
        .map(String::from)
        .collect()
}

// `usage export --since 2026-10-01 --format csv`: a header row and one row per record since then.
#[test]
fn csv_has_a_header_and_a_row_per_record_since_the_date() {
    let data = tempfile::tempdir().unwrap();
    seeded(data.path());
    let lines = lines(Format::Csv, Some("2026-10-01"), data.path());
    assert_eq!(lines.len(), 3, "{lines:?}");
    assert!(
        lines[0].starts_with("t,time,session,project,role,model,account,input,"),
        "{}",
        lines[0]
    );
    assert!(
        lines[1].contains("ollama/qwen3-coder") && lines[1].contains("2026-10-01T12:00:00Z"),
        "{}",
        lines[1]
    );
    assert!(lines[2].contains("openai/gpt-5"));
    // A missing value is an empty cell, never 0.
    let none = lines[1].split(',').count();
    assert_eq!(none, lines[0].split(',').count());
}

#[test]
fn jsonl_is_the_default_and_every_line_is_a_record() {
    let data = tempfile::tempdir().unwrap();
    seeded(data.path());
    let all = lines(Format::Jsonl, None, data.path());
    assert_eq!(all.len(), 3);
    for line in &all {
        let record: LedgerRecord = serde_json::from_str(line).unwrap();
        assert_eq!(record.session, "s");
    }
    let since = lines(Format::Jsonl, Some("2026-10-02"), data.path());
    assert_eq!(since.len(), 1);
}

#[test]
fn csv_quotes_what_needs_it() {
    let data = tempfile::tempdir().unwrap();
    let dirs = Dirs::under(data.path());
    let mut r = record(SEPT_30, "a/b");
    r.price = Some("has, a comma and a \"quote\"".into());
    Ledger::new(&dirs.usage).append(&r).unwrap();
    let lines = lines(Format::Csv, None, data.path());
    assert!(
        lines[1].contains("\"has, a comma and a \"\"quote\"\"\""),
        "{}",
        lines[1]
    );
}

// `usage forget --all`: all ledger and outcome files are deleted and the report shows no usage.
#[test]
fn forget_all_deletes_the_ledger_the_outcomes_and_the_windows() {
    let data = tempfile::tempdir().unwrap();
    let dirs = seeded(data.path());
    let mut store = Store::open(&dirs).unwrap();
    store.sync().unwrap();
    assert_eq!(
        store
            .report(&Query::all(Group::Model))
            .unwrap()
            .total
            .requests,
        3
    );
    forget(&dirs, None).unwrap();
    assert!(Ledger::new(&dirs.usage).files().is_empty());
    assert!(!dirs.usage.join("windows-2026-09.jsonl").exists());
    // Only the empty lock file stays: deleting it under a process that waits on it would let two
    // holders in.
    assert!(
        std::fs::read_dir(&dirs.outcomes)
            .map_or(true, |d| d.flatten().all(|e| e.file_name() == ".lock"))
    );
    let mut store = Store::open(&dirs).unwrap();
    store.sync().unwrap();
    let report = store.report(&Query::all(Group::Model)).unwrap();
    assert!(report.ledger_empty, "the cache is rebuilt from nothing");
}

// `usage forget --before 2026-10-01`: outcome files for months before October are deleted and
// later ones remain.
#[test]
fn forget_before_a_month_keeps_that_month_and_later() {
    let data = tempfile::tempdir().unwrap();
    let dirs = seeded(data.path());
    forget(&dirs, Some("2026-10-01")).unwrap();
    assert!(!dirs.outcomes.join("2026-09.jsonl").exists());
    assert!(dirs.outcomes.join("2026-10.jsonl").exists());
    assert!(!dirs.usage.join("ledger-2026-09.jsonl").exists());
    assert!(dirs.usage.join("ledger-2026-10.jsonl").exists());
    assert!(!dirs.usage.join("windows-2026-09.jsonl").exists());
    let mut store = Store::open(&dirs).unwrap();
    store.sync().unwrap();
    assert_eq!(
        store
            .report(&Query::all(Group::Model))
            .unwrap()
            .total
            .requests,
        2
    );
}

// A date in the middle of a month drops only the records before it.
#[test]
fn forget_before_a_day_in_a_month_drops_the_earlier_records_of_that_month() {
    let data = tempfile::tempdir().unwrap();
    let dirs = seeded(data.path());
    forget(&dirs, Some("2026-10-02")).unwrap();
    let mut store = Store::open(&dirs).unwrap();
    store.sync().unwrap();
    let report = store.report(&Query::all(Group::Day)).unwrap();
    assert_eq!(report.rows.len(), 1);
    assert_eq!(report.rows[0].key, "2026-10-02");
    // The outcome lines of 10-01 are gone too; the file for the month stays.
    let text = std::fs::read_to_string(dirs.outcomes.join("2026-10.jsonl")).unwrap_or_default();
    assert!(
        !text.contains(&format!("\"t\":{}", SEPT_30 + DAY)),
        "{text}"
    );
}

#[test]
fn forget_refuses_a_date_that_is_not_one() {
    let data = tempfile::tempdir().unwrap();
    let dirs = seeded(data.path());
    assert!(forget(&dirs, Some("last tuesday")).is_err());
    assert_eq!(Ledger::new(&dirs.usage).read().len(), 3);
}
