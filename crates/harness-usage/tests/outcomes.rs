//! 1.11: the outcome log: one line per turn, no content, local, on by default, independent of the
//! ledger.

use std::{os::unix::fs::PermissionsExt, sync::Arc, time::Duration};

use harness_core::{
    message::Usage,
    meter::{GateCounts, Meter, RequestRecord, TurnRecord},
};
use harness_usage::{ledger::Ledger, meter::UsageMeter, paths::Dirs};
use serde_json::Value;

/// 2026-10-02 12:00:00 UTC.
const OCTOBER: u64 = 1_790_942_400;

fn turn(id: &str, reason: &str) -> TurnRecord {
    TurnRecord {
        session: "s1".into(),
        turn: id.into(),
        role: "main".into(),
        model: "ollama/qwen3-coder".into(),
        selected_by: "config".into(),
        input_tokens: 1_000,
        output_tokens: 50,
        first_token_ms: Some(400),
        duration_ms: 5_000,
        tool_calls: 4,
        invalid_calls: 1,
        retries: 0,
        finish_reason: reason.into(),
        started_at: OCTOBER - 5,
        ended_at: OCTOBER,
        gates: GateCounts::default(),
    }
}

fn meter(data: &std::path::Path) -> UsageMeter {
    UsageMeter::open(data, data).with_clock(Arc::new(|| OCTOBER))
}

fn records(data: &std::path::Path) -> Vec<Value> {
    let file = Dirs::under(data).outcomes.join("2026-10.jsonl");
    std::fs::read_to_string(file)
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

// A turn on `main` with `ollama/qwen3-coder` makes 4 tool calls, 1 invalid, and completes.
#[test]
fn a_turn_is_one_line_with_the_d10_fields() {
    let data = tempfile::tempdir().unwrap();
    meter(data.path()).record_turn(&turn("e1", "completed"));
    let file = Dirs::under(data.path()).outcomes.join("2026-10.jsonl");
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(file.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    let records = records(data.path());
    assert_eq!(records.len(), 1);
    let r = &records[0];
    assert_eq!(r["role"], "main");
    assert_eq!(r["model"], "ollama/qwen3-coder");
    assert_eq!(r["selected_by"], "config");
    assert_eq!(r["tool_calls"], 4);
    assert_eq!(r["invalid_calls"], 1);
    assert_eq!(r["finish_reason"], "completed");
    assert_eq!(r["input_tokens"], 1_000);
    assert_eq!(r["output_tokens"], 50);
    assert_eq!(r["first_token_ms"], 400);
    assert_eq!(r["duration_ms"], 5_000);
    assert_eq!(r["retries"], 0);
    assert_eq!(r["user_signal"], "continued");
    assert_eq!(
        r["gates"],
        serde_json::json!({"passed": 0, "failed": 0, "skipped": 0})
    );
    assert_eq!(r["session"], "s1");
    // The cost is by reference to the ledger: where to look, not a copy of it.
    assert_eq!(
        r["ledger"],
        serde_json::json!({"session": "s1", "from": OCTOBER - 5, "to": OCTOBER})
    );
    assert!(r.get("cost_usd").is_none() && r.get("billed_usd").is_none());
    // The hand-off fields are there, for the model roles that fill them.
    assert!(r["handoff"].is_null());
}

#[test]
fn a_record_holds_only_counts_hashes_and_ids() {
    let data = tempfile::tempdir().unwrap();
    meter(data.path()).record_turn(&turn("e1", "completed"));
    let file = Dirs::under(data.path()).outcomes.join("2026-10.jsonl");
    let text = std::fs::read_to_string(file).unwrap();
    let value: Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
    let mut keys: Vec<&str> = value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "duration_ms",
            "finish_reason",
            "first_token_ms",
            "gates",
            "handoff",
            "input_tokens",
            "invalid_calls",
            "ledger",
            "model",
            "output_tokens",
            "project",
            "retries",
            "role",
            "selected_by",
            "session",
            "t",
            "tool_calls",
            "turn",
            "user_signal",
            "v"
        ]
    );
}

#[test]
fn an_interrupted_turn_has_that_signal() {
    let data = tempfile::tempdir().unwrap();
    meter(data.path()).record_turn(&turn("e1", "interrupted"));
    assert_eq!(records(data.path())[0]["user_signal"], "interrupted");
}

// The user rewinds to before a turn: that turn's record has user signal `rewound`.
#[test]
fn a_rewound_turn_is_marked_and_the_others_are_not() {
    let data = tempfile::tempdir().unwrap();
    let m = meter(data.path());
    m.record_turn(&turn("e1", "completed"));
    m.record_turn(&turn("e2", "completed"));
    let mut other = turn("e2", "completed");
    other.session = "s2".into();
    m.record_turn(&other);
    m.turns_rewound("s1", &["e2".to_string()]);
    let signals: Vec<(String, String, String)> = records(data.path())
        .iter()
        .map(|r| {
            (
                r["session"].as_str().unwrap().into(),
                r["turn"].as_str().unwrap().into(),
                r["user_signal"].as_str().unwrap().into(),
            )
        })
        .collect();
    assert_eq!(
        signals,
        [
            ("s1".into(), "e1".into(), "continued".into()),
            ("s1".into(), "e2".into(), "rewound".into()),
            ("s2".into(), "e2".into(), "continued".into()),
        ]
    );
    let file = Dirs::under(data.path()).outcomes.join("2026-10.jsonl");
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

// `[outcomes] enabled = false` and a turn finishes: no file under `outcomes/` is created or
// changed, and the usage ledger is still written.
#[test]
fn turned_off_writes_no_outcome_file_and_the_ledger_goes_on() {
    let data = tempfile::tempdir().unwrap();
    let m = meter(data.path()).with_outcomes(false);
    m.record_request(&RequestRecord {
        session: "s1".into(),
        role: "main".into(),
        model: "ollama/qwen3-coder".into(),
        local: true,
        usage: Usage {
            input_tokens: 5,
            ..Usage::default()
        },
        duration: Duration::from_millis(1),
        outcome: "ok".into(),
    });
    m.record_turn(&turn("e1", "completed"));
    m.turns_rewound("s1", &["e1".to_string()]);
    assert!(!Dirs::under(data.path()).outcomes.exists());
    assert_eq!(Ledger::new(&Dirs::under(data.path()).usage).read().len(), 1);
}

#[test]
fn the_outcome_log_does_not_need_the_ledger() {
    let data = tempfile::tempdir().unwrap();
    meter(data.path()).record_turn(&turn("e1", "completed"));
    assert!(
        Ledger::new(&Dirs::under(data.path()).usage)
            .read()
            .is_empty()
    );
    assert_eq!(records(data.path()).len(), 1);
}
