//! R2: `forget` is the only rewriter of usage files, and it takes an exclusive lock; every
//! append takes the lock shared, so a record another process appends while `forget` runs is not
//! lost.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use harness_core::meter::AccountKind;
use harness_usage::{
    export::forget,
    ledger::{Ledger, LedgerRecord},
    outcomes::OutcomeLog,
    paths::Dirs,
};

/// 2026-10-01 12:00 UTC, and 2026-10-02 12:00 UTC.
const OCT_1: u64 = 1_790_856_000;
const OCT_2: u64 = OCT_1 + 86_400;

fn record(t: u64) -> LedgerRecord {
    LedgerRecord {
        v: 1,
        t,
        session: "s".into(),
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

// Appends race with a `forget --before` that rewrites the same month's file over and over: every
// appended record is still there at the end.
#[test]
fn an_append_made_while_forget_rewrites_the_file_is_not_lost() {
    let data = tempfile::tempdir().unwrap();
    let dirs = Dirs::under(data.path());
    let ledger = Ledger::new(&dirs.usage);
    // Old records, so that every `forget` has something to drop and rewrites the file.
    const APPENDS: u64 = 300;
    let done = Arc::new(AtomicBool::new(false));
    let forgetter = {
        let (dirs, done) = (dirs.clone(), done.clone());
        thread::spawn(move || {
            let mut rounds = 0;
            while !done.load(Ordering::SeqCst) {
                forget(&dirs, Some("2026-10-02")).unwrap();
                rounds += 1;
            }
            rounds
        })
    };
    for i in 0..APPENDS {
        // One old record per round keeps the rewrite going.
        ledger.append(&record(OCT_1 + i)).unwrap();
        ledger.append(&record(OCT_2 + i)).unwrap();
    }
    done.store(true, Ordering::SeqCst);
    let rounds = forgetter.join().unwrap();
    assert!(rounds > 0);
    let kept = ledger.read().into_iter().filter(|r| r.t >= OCT_2).count();
    assert_eq!(
        kept as u64, APPENDS,
        "{} of {APPENDS} appended records survived {rounds} rewrites",
        kept
    );
}

// Appends wait for the lock `forget` holds, and go on when it is released.
#[test]
fn appends_wait_while_forget_holds_the_lock() {
    let data = tempfile::tempdir().unwrap();
    let dirs = Dirs::under(data.path());
    let usage = harness_usage::lock::exclusive(&dirs.usage).unwrap();
    let outcomes = harness_usage::lock::exclusive(&dirs.outcomes).unwrap();
    let (tx, rx) = mpsc::channel();
    let ledger = Ledger::new(&dirs.usage);
    let log = OutcomeLog::new(&dirs.outcomes);
    {
        let tx = tx.clone();
        thread::spawn(move || {
            ledger.append(&record(OCT_2)).unwrap();
            tx.send("ledger").unwrap();
        });
    }
    {
        let tx = tx.clone();
        let log = log.clone();
        thread::spawn(move || {
            log.mark_rewound("s", &["e1".to_string()], OCT_2).unwrap();
            tx.send("signal").unwrap();
        });
    }
    assert!(
        rx.recv_timeout(Duration::from_millis(300)).is_err(),
        "an append went ahead of the exclusive lock"
    );
    drop((usage, outcomes));
    let mut finished = vec![
        rx.recv_timeout(Duration::from_secs(10)).unwrap(),
        rx.recv_timeout(Duration::from_secs(10)).unwrap(),
    ];
    finished.sort();
    assert_eq!(finished, ["ledger", "signal"]);
}

// Appends do not exclude each other: two shared holds at once.
#[test]
fn appends_share_the_lock() {
    let data = tempfile::tempdir().unwrap();
    let dir = data.path().join("usage");
    let first = harness_usage::lock::shared(&dir).unwrap();
    let second = harness_usage::lock::shared(&dir).unwrap();
    drop((first, second));
}
