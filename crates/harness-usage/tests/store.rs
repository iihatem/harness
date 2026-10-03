//! 1.4: the report cache is built from the ledger, rebuilt from the lines it lacks, and safe to
//! delete; reports group by model, provider, day and project.

use harness_core::meter::AccountKind;
use harness_usage::{
    ledger::{Ledger, LedgerRecord},
    paths::Dirs,
    store::{Group, Query, Store},
};

/// 2026-09-30 12:00:00 UTC, and the next two days.
const SEPT_30: u64 = 1_790_769_600;
const DAY: u64 = 86_400;

fn record(
    t: u64,
    model: &str,
    project: &str,
    account: AccountKind,
    input: u64,
    billed: Option<f64>,
) -> LedgerRecord {
    LedgerRecord {
        v: 1,
        t,
        session: "s1".into(),
        project: project.into(),
        role: "main".into(),
        model: model.into(),
        account,
        input,
        cache_read: 0,
        cache_write: 0,
        cache_write_1h: 0,
        output: 10,
        reasoning: 0,
        billed_usd: billed,
        list_usd: billed,
        price: None,
        ms: 100,
        outcome: "ok".into(),
        window: None,
    }
}

/// A ledger with two models, two providers, three days (the last in October) and two projects.
fn seeded(data: &std::path::Path) -> Dirs {
    let dirs = Dirs::under(data);
    let ledger = Ledger::new(&dirs.usage);
    for r in [
        record(
            SEPT_30,
            "openai/gpt-5",
            "p1",
            AccountKind::ApiKey,
            1_000,
            Some(0.50),
        ),
        record(
            SEPT_30 + 60,
            "openai/gpt-5",
            "p1",
            AccountKind::ApiKey,
            2_000,
            Some(0.25),
        ),
        record(
            SEPT_30 + DAY,
            "ollama/qwen3-coder",
            "p2",
            AccountKind::Local,
            4_000,
            Some(0.0),
        ),
        record(
            SEPT_30 + 2 * DAY,
            "openai/gpt-5",
            "p2",
            AccountKind::ApiKey,
            500,
            None,
        ),
        record(
            SEPT_30 + 2 * DAY,
            "chatgpt/gpt-5",
            "p1",
            AccountKind::Subscription,
            700,
            Some(0.0),
        ),
    ] {
        ledger.append(&r).unwrap();
    }
    dirs
}

fn totals(store: &Store, by: Group) -> Vec<(String, u64, u64)> {
    store
        .report(&Query::all(by))
        .unwrap()
        .rows
        .iter()
        .map(|r| (r.key.clone(), r.requests, r.tokens.input))
        .collect()
}

#[test]
fn the_cache_is_built_from_the_ledger_and_grouped_four_ways() {
    let data = tempfile::tempdir().unwrap();
    let dirs = seeded(data.path());
    let mut store = Store::open(&dirs).unwrap();
    assert_eq!(store.sync().unwrap(), 5);
    assert_eq!(
        totals(&store, Group::Model),
        [
            ("chatgpt/gpt-5".to_string(), 1, 700),
            ("ollama/qwen3-coder".to_string(), 1, 4_000),
            ("openai/gpt-5".to_string(), 3, 3_500),
        ]
    );
    assert_eq!(
        totals(&store, Group::Provider),
        [
            ("chatgpt".to_string(), 1, 700),
            ("ollama".to_string(), 1, 4_000),
            ("openai".to_string(), 3, 3_500),
        ]
    );
    assert_eq!(
        totals(&store, Group::Day),
        [
            ("2026-09-30".to_string(), 2, 3_000),
            ("2026-10-01".to_string(), 1, 4_000),
            ("2026-10-02".to_string(), 2, 1_200),
        ]
    );
    assert_eq!(
        totals(&store, Group::Project),
        [("p1".to_string(), 3, 3_700), ("p2".to_string(), 2, 4_500)]
    );
}

#[test]
fn a_period_limits_the_report() {
    let data = tempfile::tempdir().unwrap();
    let dirs = seeded(data.path());
    let mut store = Store::open(&dirs).unwrap();
    store.sync().unwrap();
    let query = Query {
        by: Group::Model,
        since: Some("2026-10-01".into()),
        until: Some("2026-10-01".into()),
    };
    let report = store.report(&query).unwrap();
    assert_eq!(report.rows.len(), 1);
    assert_eq!(report.rows[0].key, "ollama/qwen3-coder");
    assert_eq!(report.total.requests, 1);
}

// Deleting `index.sqlite` and running the report again shows the same totals as before.
#[test]
fn deleting_the_cache_and_rebuilding_gives_the_same_report() {
    let data = tempfile::tempdir().unwrap();
    let dirs = seeded(data.path());
    let before = {
        let mut store = Store::open(&dirs).unwrap();
        store.sync().unwrap();
        store.report(&Query::all(Group::Model)).unwrap()
    };
    std::fs::remove_file(dirs.usage.join("index.sqlite")).unwrap();
    let mut store = Store::open(&dirs).unwrap();
    assert_eq!(store.sync().unwrap(), 5);
    assert_eq!(store.report(&Query::all(Group::Model)).unwrap(), before);
}

#[test]
fn only_the_lines_the_cache_lacks_are_read() {
    let data = tempfile::tempdir().unwrap();
    let dirs = seeded(data.path());
    let mut store = Store::open(&dirs).unwrap();
    assert_eq!(store.sync().unwrap(), 5);
    assert_eq!(store.sync().unwrap(), 0);
    Ledger::new(&dirs.usage)
        .append(&record(
            SEPT_30 + 3 * DAY,
            "openai/gpt-5",
            "p1",
            AccountKind::ApiKey,
            9,
            Some(0.01),
        ))
        .unwrap();
    assert_eq!(store.sync().unwrap(), 1);
    assert_eq!(
        store
            .report(&Query::all(Group::Model))
            .unwrap()
            .total
            .requests,
        6
    );
    // A second handle on the same files sees the same.
    let mut other = Store::open(&dirs).unwrap();
    assert_eq!(other.sync().unwrap(), 0);
    assert_eq!(
        other
            .report(&Query::all(Group::Model))
            .unwrap()
            .total
            .requests,
        6
    );
}

#[test]
fn a_line_still_being_written_is_read_once_it_is_complete() {
    let data = tempfile::tempdir().unwrap();
    let dirs = seeded(data.path());
    let file = dirs.usage.join("ledger-2026-10.jsonl");
    let text = std::fs::read_to_string(&file).unwrap();
    let last = text.lines().last().unwrap().to_string();
    // The last line is cut short, with no newline yet.
    let mut cut = text
        .lines()
        .take(text.lines().count() - 1)
        .collect::<Vec<_>>()
        .join("\n");
    cut.push('\n');
    cut.push_str(&last[..20]);
    std::fs::write(&file, &cut).unwrap();
    let mut store = Store::open(&dirs).unwrap();
    assert_eq!(store.sync().unwrap(), 4);
    std::fs::write(&file, text).unwrap();
    assert_eq!(store.sync().unwrap(), 1);
}

#[test]
fn a_ledger_file_that_was_removed_or_rewritten_is_forgotten_by_the_cache() {
    let data = tempfile::tempdir().unwrap();
    let dirs = seeded(data.path());
    let mut store = Store::open(&dirs).unwrap();
    store.sync().unwrap();
    std::fs::remove_file(dirs.usage.join("ledger-2026-10.jsonl")).unwrap();
    store.sync().unwrap();
    assert_eq!(
        store
            .report(&Query::all(Group::Model))
            .unwrap()
            .total
            .requests,
        2
    );
    // Rewritten shorter than what the cache has read of it.
    let file = dirs.usage.join("ledger-2026-09.jsonl");
    let first = std::fs::read_to_string(&file)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .to_string();
    std::fs::write(&file, format!("{first}\n")).unwrap();
    store.sync().unwrap();
    assert_eq!(
        store
            .report(&Query::all(Group::Model))
            .unwrap()
            .total
            .requests,
        1
    );
}

#[test]
fn a_cache_that_is_not_a_database_is_replaced() {
    let data = tempfile::tempdir().unwrap();
    let dirs = seeded(data.path());
    std::fs::write(dirs.usage.join("index.sqlite"), "this is not sqlite").unwrap();
    let mut store = Store::open(&dirs).unwrap();
    assert_eq!(store.sync().unwrap(), 5);
    assert_eq!(
        store
            .report(&Query::all(Group::Model))
            .unwrap()
            .total
            .requests,
        5
    );
}

#[test]
fn the_cache_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let data = tempfile::tempdir().unwrap();
    let dirs = seeded(data.path());
    Store::open(&dirs).unwrap();
    let mode = std::fs::metadata(dirs.usage.join("index.sqlite"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn a_null_cost_is_counted_as_unknown_never_as_zero() {
    let data = tempfile::tempdir().unwrap();
    let dirs = seeded(data.path());
    let mut store = Store::open(&dirs).unwrap();
    store.sync().unwrap();
    let report = store.report(&Query::all(Group::Model)).unwrap();
    let gpt = report
        .rows
        .iter()
        .find(|r| r.key == "openai/gpt-5")
        .unwrap();
    // Two priced requests, $0.50 and $0.25, and one with no price.
    assert!((gpt.billed.usd - 0.75).abs() < 1e-9, "{gpt:?}");
    assert_eq!(gpt.billed.unknown, 1);
    // A local model's cost is known: 0.
    let local = report
        .rows
        .iter()
        .find(|r| r.key == "ollama/qwen3-coder")
        .unwrap();
    assert_eq!(local.billed.unknown, 0);
    assert_eq!(report.total.billed.unknown, 1);
}

#[test]
fn an_empty_ledger_is_said_to_be_empty() {
    let data = tempfile::tempdir().unwrap();
    let dirs = Dirs::under(data.path());
    let mut store = Store::open(&dirs).unwrap();
    store.sync().unwrap();
    let report = store.report(&Query::all(Group::Model)).unwrap();
    assert!(report.ledger_empty);
    assert!(report.rows.is_empty());
}
