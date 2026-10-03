//! Fix wave 1 (review A): torn tails, budgets that fail open, one warning per kind, a window id
//! only once written, the pricing body cap and negative prices, and `forget` leaving no trace in
//! the cache.

use std::{
    io::Write,
    sync::{Arc, atomic::AtomicU64, atomic::Ordering},
    time::Duration,
};

use harness_core::{
    message::Usage,
    meter::{
        AccountKind, BudgetKind, GateCounts, Meter, RequestRecord, TurnRecord, Window,
        WindowSnapshot, WindowSource,
    },
};
use harness_usage::{
    budget::Budgets,
    export::forget,
    ledger::{Ledger, LedgerRecord},
    meter::UsageMeter,
    outcomes::{OutcomeLog, OutcomeRecord},
    paths::Dirs,
    pricing::{Price, Pricing, trim_models_dev},
    store::{Group, Query, Store},
};

/// 2026-10-02 12:00:00 UTC.
const NOW: u64 = 1_790_942_400;

fn record(t: u64, session: &str) -> LedgerRecord {
    LedgerRecord {
        v: 1,
        t,
        session: session.into(),
        project: "p".into(),
        role: "main".into(),
        model: "openai/gpt-5".into(),
        account: AccountKind::ApiKey,
        input: 10,
        cache_read: 0,
        cache_write: 0,
        cache_write_1h: 0,
        output: 2,
        reasoning: 0,
        billed_usd: Some(0.5),
        list_usd: Some(0.5),
        price: None,
        ms: 7,
        outcome: "ok".into(),
        window: None,
    }
}

fn meter(data: &std::path::Path, budgets: Budgets) -> UsageMeter {
    let pricing = Pricing::load(
        &data.join("none.json"),
        vec![(
            "openai/gpt-5".into(),
            Price {
                input: Some(1_000_000.0),
                output: Some(1_000_000.0),
                ..Price::default()
            },
        )],
    );
    let clock = Arc::new(AtomicU64::new(NOW));
    UsageMeter::open(data, data)
        .with_clock(Arc::new(move || clock.load(Ordering::SeqCst)))
        .with_pricing(pricing)
        .with_budgets(budgets)
}

/// A request costing $`tokens` on gpt-5, in session `s`.
fn spend(m: &UsageMeter, tokens: u64) {
    m.record_request(&RequestRecord {
        session: "s".into(),
        role: "main".into(),
        model: "openai/gpt-5".into(),
        local: false,
        usage: Usage {
            input_tokens: tokens,
            ..Usage::default()
        },
        duration: Duration::from_millis(1),
        outcome: "ok".into(),
    });
}

fn turn(id: &str) -> TurnRecord {
    TurnRecord {
        session: "s".into(),
        turn: id.into(),
        role: "main".into(),
        model: "ollama/q".into(),
        selected_by: "config".into(),
        input_tokens: 1,
        output_tokens: 1,
        first_token_ms: None,
        duration_ms: 1,
        tool_calls: 0,
        invalid_calls: 0,
        retries: 0,
        finish_reason: "stop".into(),
        started_at: NOW - 1,
        ended_at: NOW,
        gates: GateCounts::default(),
    }
}

// A1: a crash left half a line with no newline; the next record is not glued to it.
#[test]
fn an_append_after_a_torn_last_line_is_not_lost() {
    let data = tempfile::tempdir().unwrap();
    let dirs = Dirs::under(data.path());
    let ledger = Ledger::new(&dirs.usage);
    ledger.append(&record(NOW, "first")).unwrap();
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(ledger.file_for(NOW))
        .unwrap();
    file.write_all(br#"{"v":1,"t":17909"#).unwrap();
    drop(file);
    ledger.append(&record(NOW, "second")).unwrap();
    let sessions: Vec<String> = ledger.read().into_iter().map(|r| r.session).collect();
    assert_eq!(sessions, ["first", "second"]);
    // The report cache reads it the same way.
    let mut store = Store::open(&dirs).unwrap();
    store.sync().unwrap();
    assert_eq!(
        store
            .report(&Query::all(Group::Day))
            .unwrap()
            .total
            .requests,
        2
    );
}

#[test]
fn an_outcome_after_a_torn_last_line_is_not_lost() {
    let data = tempfile::tempdir().unwrap();
    let dirs = Dirs::under(data.path());
    let log = OutcomeLog::new(&dirs.outcomes);
    log.append(&OutcomeRecord::of(&turn("t1"), "p")).unwrap();
    let path = log.files().pop().unwrap();
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    file.write_all(br#"{"v":1,"t":"#).unwrap();
    drop(file);
    log.append(&OutcomeRecord::of(&turn("t2"), "p")).unwrap();
    log.mark_rewound("s", &["t2".to_string()], NOW).unwrap();
    let read = log.read();
    assert_eq!(read.len(), 2);
    assert_eq!(read[1].user_signal, "rewound");
}

/// A data dir where the report cache and this month's ledger file cannot be used.
fn broken_data() -> tempfile::TempDir {
    let data = tempfile::tempdir().unwrap();
    let dirs = Dirs::under(data.path());
    std::fs::create_dir_all(&dirs.usage).unwrap();
    std::fs::create_dir(dirs.usage.join("index.sqlite")).unwrap();
    std::fs::create_dir(dirs.usage.join("ledger-2026-10.jsonl")).unwrap();
    data
}

// A2: with the ledger and the cache unusable, the session budget still stops the session.
#[test]
fn a_session_budget_still_stops_when_the_ledger_cannot_be_used() {
    let data = broken_data();
    let m = meter(
        data.path(),
        Budgets {
            session_usd: Some(1.5),
            ..Budgets::default()
        },
    );
    assert!(m.check_budget("s", AccountKind::ApiKey).stop.is_none());
    spend(&m, 1);
    assert!(m.check_budget("s", AccountKind::ApiKey).stop.is_none());
    spend(&m, 1);
    let stop = m
        .check_budget("s", AccountKind::ApiKey)
        .stop
        .expect("stopped");
    assert_eq!(stop.budget, BudgetKind::Session);
    assert!(stop.spent_usd >= 2.0);
}

// A2: a daily cap that cannot be checked says so every turn, until it can be.
#[test]
fn a_budget_that_cannot_be_checked_says_so_every_turn_until_it_can() {
    let data = broken_data();
    let m = meter(
        data.path(),
        Budgets {
            daily_usd: Some(5.0),
            ..Budgets::default()
        },
    );
    for _ in 0..3 {
        m.check_budget("s", AccountKind::ApiKey);
        let warnings = m.take_warnings();
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("cannot check the daily budget")),
            "{warnings:?}"
        );
    }
    // The cache comes back.
    let dirs = Dirs::under(data.path());
    std::fs::remove_dir(dirs.usage.join("index.sqlite")).unwrap();
    std::fs::remove_dir(dirs.usage.join("ledger-2026-10.jsonl")).unwrap();
    m.check_budget("s", AccountKind::ApiKey);
    assert!(m.take_warnings().is_empty());
}

// A2: the in-memory fallback applies to the daily cap too.
#[test]
fn a_daily_budget_falls_back_to_this_process_spend() {
    let data = broken_data();
    let m = meter(
        data.path(),
        Budgets {
            daily_usd: Some(1.5),
            ..Budgets::default()
        },
    );
    spend(&m, 2);
    let stop = m
        .check_budget("s", AccountKind::ApiKey)
        .stop
        .expect("stopped");
    assert_eq!(stop.budget, BudgetKind::Daily);
}

// A3: a second, different problem is still said, and the same one only once.
#[test]
fn each_kind_of_warning_is_given_once() {
    let data = broken_data();
    let m = meter(data.path(), Budgets::default());
    spend(&m, 1);
    spend(&m, 1);
    m.record_turn(&turn("t1"));
    let outcomes = Dirs::under(data.path()).outcomes;
    std::fs::remove_dir_all(&outcomes).ok();
    std::fs::write(&outcomes, "a file, not a directory").unwrap();
    m.record_turn(&turn("t2"));
    m.record_turn(&turn("t3"));
    let warnings = m.take_warnings();
    assert_eq!(
        warnings
            .iter()
            .filter(|w| w.contains("usage ledger"))
            .count(),
        1,
        "{warnings:?}"
    );
    assert_eq!(
        warnings
            .iter()
            .filter(|w| w.contains("outcome log"))
            .count(),
        1,
        "{warnings:?}"
    );
}

fn snapshot() -> WindowSnapshot {
    WindowSnapshot {
        windows: vec![Window {
            window_minutes: Some(300),
            used_percent: Some(40.0),
            resets_at: Some(NOW + 3_600),
            source: WindowSource::Header,
        }],
        observed_at: NOW,
    }
}

// A11: a snapshot that could not be written is not named by the ledger, and is written the next
// time it is seen.
#[test]
fn a_window_that_failed_to_write_is_not_referenced_and_is_retried() {
    let data = tempfile::tempdir().unwrap();
    let dirs = Dirs::under(data.path());
    std::fs::create_dir_all(&dirs.usage).unwrap();
    let windows = dirs.usage.join("windows-2026-10.jsonl");
    std::fs::create_dir(&windows).unwrap();
    let m = meter(data.path(), Budgets::default());
    m.record_window(&snapshot());
    spend(&m, 1);
    assert_eq!(Ledger::new(&dirs.usage).read()[0].window, None);
    std::fs::remove_dir(&windows).unwrap();
    m.record_window(&snapshot());
    spend(&m, 1);
    let records = Ledger::new(&dirs.usage).read();
    assert_eq!(records[1].window.as_deref(), Some(snapshot().id().as_str()));
    assert!(
        std::fs::read_to_string(&windows)
            .unwrap()
            .contains(&snapshot().id())
    );
}

// A9: a negative price is refused whatever else the model has.
#[test]
fn a_negative_price_is_refused_even_without_an_output_price() {
    let data = r#"{"openai":{"models":{
        "gpt-5":{"cost":{"input":1,"output":2}},
        "odd":{"cost":{"input":-1}}}}}"#;
    let err = trim_models_dev(data, "2026-10-03").unwrap_err();
    assert!(err.to_string().contains("negative"), "{err}");
    let data = r#"{"openai":{"models":{
        "gpt-5":{"cost":{"input":1,"output":2}},
        "odd":{"cost":{"cache_read":-0.5}}}}}"#;
    assert!(trim_models_dev(data, "2026-10-03").is_err());
}

// A8: after `forget`, the cache file holds nothing of what was forgotten.
#[test]
fn forget_leaves_nothing_of_the_records_in_the_cache_file() {
    let data = tempfile::tempdir().unwrap();
    let dirs = Dirs::under(data.path());
    let ledger = Ledger::new(&dirs.usage);
    for i in 0..50 {
        ledger
            .append(&record(NOW + i, "session-marker-zq7"))
            .unwrap();
    }
    let mut store = Store::open(&dirs).unwrap();
    store.sync().unwrap();
    drop(store);
    forget(&dirs, None).unwrap();
    let bytes = std::fs::read(dirs.usage.join("index.sqlite")).unwrap();
    let marker = b"session-marker-zq7";
    assert!(
        !bytes.windows(marker.len()).any(|w| w == marker),
        "the cache still holds a forgotten session id"
    );
}

// A5: a body past the cap stops being read once the cap is passed, even with no Content-Length.
#[tokio::test]
async fn an_update_stops_reading_a_chunked_body_at_the_cap() {
    use tokio::{io::AsyncWriteExt, net::TcpListener};
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let written = Arc::new(AtomicU64::new(0));
    let counter = written.clone();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 1024];
        let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut buf).await;
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n")
            .await
            .unwrap();
        let chunk = vec![b' '; 1024 * 1024];
        for _ in 0..200 {
            let head = format!("{:x}\r\n", chunk.len());
            let ok = socket.write_all(head.as_bytes()).await.is_ok()
                && socket.write_all(&chunk).await.is_ok()
                && socket.write_all(b"\r\n").await.is_ok();
            if !ok {
                return;
            }
            counter.fetch_add(1, Ordering::SeqCst);
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let err = harness_usage::pricing::update(
        &format!("http://127.0.0.1:{port}/api.json"),
        &dir.path().join("pricing.json"),
        "2026-10-03",
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("too large"), "{err}");
    let _ = tokio::time::timeout(Duration::from_secs(10), server).await;
    assert!(
        written.load(Ordering::SeqCst) < 200,
        "the whole body was read"
    );
}

// A7: several processes opening a new cache at once all get a working one.
#[test]
fn opening_a_new_cache_from_several_threads_at_once_works() {
    let data = tempfile::tempdir().unwrap();
    let dirs = Dirs::under(data.path());
    Ledger::new(&dirs.usage).append(&record(NOW, "s")).unwrap();
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let dirs = dirs.clone();
            std::thread::spawn(move || {
                let mut store = Store::open(&dirs).unwrap();
                store.sync().unwrap();
                store
                    .report(&Query::all(Group::Day))
                    .unwrap()
                    .total
                    .requests
            })
        })
        .collect();
    for handle in handles {
        assert_eq!(handle.join().unwrap(), 1);
    }
}
