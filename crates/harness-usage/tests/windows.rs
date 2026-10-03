//! 1.7: the window snapshot a request saw is kept beside the ledger, and its id is in the record.

use std::{sync::Arc, time::Duration};

use harness_core::{
    message::Usage,
    meter::{Meter, RequestRecord, Window, WindowSnapshot, WindowSource},
};
use harness_usage::{ledger::Ledger, meter::UsageMeter, paths::Dirs};
use serde_json::Value;

const OCTOBER: u64 = 1_790_942_400;

fn request() -> RequestRecord {
    RequestRecord {
        session: "s".into(),
        role: "main".into(),
        model: "chatgpt/gpt-5".into(),
        local: false,
        usage: Usage::default(),
        duration: Duration::from_millis(5),
        outcome: "ok".into(),
    }
}

fn snapshot(percent: f64) -> WindowSnapshot {
    WindowSnapshot {
        windows: vec![Window {
            window_minutes: Some(300),
            used_percent: Some(percent),
            resets_at: Some(OCTOBER + 3_600),
            source: WindowSource::Header,
        }],
        observed_at: OCTOBER,
    }
}

#[test]
fn the_request_that_saw_a_snapshot_names_it_and_the_next_does_not() {
    let data = tempfile::tempdir().unwrap();
    let meter = UsageMeter::open(data.path(), data.path()).with_clock(Arc::new(|| OCTOBER));
    let seen = snapshot(42.0);
    meter.record_window(&seen);
    meter.record_request(&request());
    meter.record_request(&request());
    let records = Ledger::new(&Dirs::under(data.path()).usage).read();
    assert_eq!(records[0].window.as_deref(), Some(seen.id().as_str()));
    assert_eq!(records[1].window, None);
}

#[test]
fn the_snapshot_is_written_once_with_the_windows_and_no_more_than_that() {
    let data = tempfile::tempdir().unwrap();
    let meter = UsageMeter::open(data.path(), data.path()).with_clock(Arc::new(|| OCTOBER));
    let seen = snapshot(42.0);
    meter.record_window(&seen);
    meter.record_window(&seen);
    meter.record_window(&snapshot(55.0));
    let file = Dirs::under(data.path()).usage.join("windows-2026-10.jsonl");
    let text = std::fs::read_to_string(&file).unwrap();
    assert_eq!(text.lines().count(), 2, "{text}");
    let first: Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
    assert_eq!(first["id"], seen.id());
    assert_eq!(first["windows"][0]["minutes"], 300);
    assert_eq!(first["windows"][0]["used_percent"], 42.0);
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

// Final review, Minor 1: the same windows seen again a second later are the same snapshot.
#[test]
fn an_unchanged_snapshot_seen_later_is_written_once() {
    let data = tempfile::tempdir().unwrap();
    let meter = UsageMeter::open(data.path(), data.path()).with_clock(Arc::new(|| OCTOBER));
    for later in 0..3 {
        let mut seen = snapshot(42.0);
        seen.observed_at += later;
        meter.record_window(&seen);
    }
    let file = Dirs::under(data.path()).usage.join("windows-2026-10.jsonl");
    let text = std::fs::read_to_string(&file).unwrap();
    assert_eq!(text.lines().count(), 1, "{text}");
}
