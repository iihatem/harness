//! 1.3: the ledger holds one record per request, private to the user, and no content.

mod common;

use std::{os::unix::fs::PermissionsExt, sync::Arc, time::Duration};

use common::*;
use harness_core::{
    message::{ToolCall, Usage},
    meter::{AccountKind, Meter, RequestRecord},
    provider::{FinishReason, ProviderError, ProviderEvent},
    testing::{MockProvider, Script},
};
use harness_usage::{
    ledger::{Ledger, LedgerRecord},
    meter::{UsageMeter, project_id},
    paths::Dirs,
};
use serde_json::{Value, json};

/// 2026-10-02 12:00:00 UTC.
const OCTOBER: u64 = 1_790_942_400;

fn request(model: &str, local: bool, outcome: &str) -> RequestRecord {
    RequestRecord {
        session: "s1".into(),
        role: "main".into(),
        model: model.into(),
        local,
        usage: Usage {
            input_tokens: 10_000,
            output_tokens: 500,
            cached_tokens: 8_000,
            reasoning_tokens: 200,
            ..Usage::default()
        },
        duration: Duration::from_millis(1_234),
        outcome: outcome.into(),
    }
}

fn meter(data: &std::path::Path, workspace: &std::path::Path) -> UsageMeter {
    UsageMeter::open(data, workspace).with_clock(Arc::new(|| OCTOBER))
}

#[test]
fn a_record_has_the_buckets_the_account_and_the_time_and_is_private() {
    let data = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    meter(data.path(), workspace.path()).record_request(&request("openai/gpt-5", false, "ok"));
    let dirs = Dirs::under(data.path());
    let file = dirs.usage.join("ledger-2026-10.jsonl");
    let mode =
        |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&file), 0o600);
    assert_eq!(mode(&dirs.usage), 0o700);
    let records = Ledger::new(&dirs.usage).read();
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(record.t, OCTOBER);
    assert_eq!(record.session, "s1");
    assert_eq!(record.role, "main");
    assert_eq!(record.model, "openai/gpt-5");
    assert_eq!(record.account, AccountKind::ApiKey);
    // The disjoint buckets: 2,000 uncached, 8,000 read from the cache, 500 out, 200 of them
    // reasoning.
    assert_eq!(
        (
            record.input,
            record.cache_read,
            record.cache_write,
            record.output,
            record.reasoning
        ),
        (2_000, 8_000, 0, 500, 200)
    );
    assert_eq!(record.ms, 1_234);
    assert_eq!(record.outcome, "ok");
    assert_eq!(record.project, project_id(workspace.path()));
}

#[test]
fn the_project_is_a_hash_never_the_path() {
    let data = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    meter(data.path(), workspace.path()).record_request(&request("openai/gpt-5", false, "ok"));
    let text = std::fs::read_to_string(Dirs::under(data.path()).usage.join("ledger-2026-10.jsonl"))
        .unwrap();
    assert!(!text.contains(workspace.path().to_str().unwrap()), "{text}");
    let id = project_id(workspace.path());
    assert_eq!(id.len(), 16);
    assert_ne!(id, project_id(&workspace.path().join("other")));
}

// A session that uses only a local model, with no network: each request is recorded with
// account kind `local` and cost 0.
#[test]
fn a_local_model_is_recorded_as_local_and_free() {
    let data = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    meter(data.path(), workspace.path()).record_request(&request("ollama/qwen3-coder", true, "ok"));
    let records = Ledger::new(&Dirs::under(data.path()).usage).read();
    assert_eq!(records[0].account, AccountKind::Local);
    assert_eq!(records[0].billed_usd, Some(0.0));
    assert_eq!(records[0].list_usd, Some(0.0));
}

#[test]
fn a_record_has_only_counts_and_ids() {
    let data = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    meter(data.path(), workspace.path()).record_request(&request("openai/gpt-5", false, "ok"));
    let text = std::fs::read_to_string(Dirs::under(data.path()).usage.join("ledger-2026-10.jsonl"))
        .unwrap();
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
            "account",
            "billed_usd",
            "cache_read",
            "cache_write",
            "cache_write_1h",
            "input",
            "list_usd",
            "model",
            "ms",
            "outcome",
            "output",
            "price",
            "project",
            "reasoning",
            "role",
            "session",
            "t",
            "v",
            "window"
        ]
    );
}

#[test]
fn a_torn_last_line_is_skipped_when_reading() {
    let data = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let meter = meter(data.path(), workspace.path());
    meter.record_request(&request("openai/gpt-5", false, "ok"));
    let file = Dirs::under(data.path()).usage.join("ledger-2026-10.jsonl");
    let mut text = std::fs::read_to_string(&file).unwrap();
    text.push_str("{\"v\":1,\"t\":17909");
    std::fs::write(&file, text).unwrap();
    meter.record_request(&request("openai/gpt-5", false, "ok"));
    // The crash cut the line; the record after it starts on the same line and is lost with it,
    // but the first stays readable, and reading does not fail.
    let records = Ledger::new(&Dirs::under(data.path()).usage).read();
    assert!(!records.is_empty());
    assert_eq!(records[0].outcome, "ok");
}

#[test]
fn a_ledger_that_cannot_be_written_warns_once_and_never_panics() {
    let data = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    // `usage` is a file, so the directory cannot be made.
    std::fs::write(data.path().join("usage"), "in the way").unwrap();
    let meter = meter(data.path(), workspace.path());
    meter.record_request(&request("openai/gpt-5", false, "ok"));
    meter.record_request(&request("openai/gpt-5", false, "ok"));
    let warnings = meter.take_warnings();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("usage ledger"), "{warnings:?}");
    assert!(meter.take_warnings().is_empty());
}

// A turn makes 3 model requests and the third fails with HTTP 503 after retries.
#[tokio::test(start_paused = true)]
async fn a_turn_with_a_failed_request_gives_three_records_and_no_content() {
    let data = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let reply = |id: &str| {
        Script::Reply(vec![
            Ok(ProviderEvent::ToolCall(ToolCall {
                id: id.into(),
                name: "echo".into(),
                // The model's own words: they must not reach the ledger.
                arguments: json!({"text": "cat .env"}).to_string(),
            })),
            Ok(ProviderEvent::Usage(Usage {
                input_tokens: 100,
                output_tokens: 10,
                ..Usage::default()
            })),
            Ok(ProviderEvent::Finished(FinishReason::ToolCalls)),
        ])
    };
    let mut script = vec![reply("c1"), reply("c2")];
    script.extend((0..5).map(|_| {
        Script::error(ProviderError::Http {
            status: 503,
            body: "src/secret.rs is not here".into(),
            retry_after: None,
        })
    }));
    let mut agent = agent(MockProvider::new(script), workspace.path())
        .with_meter(Arc::new(meter(data.path(), workspace.path())));
    run(&mut agent, "what does src/secret.rs do?").await;
    let dirs = Dirs::under(data.path());
    let file = dirs.usage.join("ledger-2026-10.jsonl");
    let text = std::fs::read_to_string(&file).unwrap();
    assert_eq!(text.lines().count(), 3, "{text}");
    let records: Vec<LedgerRecord> = Ledger::new(&dirs.usage).read();
    let outcomes: Vec<&str> = records.iter().map(|r| r.outcome.as_str()).collect();
    assert_eq!(outcomes, ["ok", "ok", "error:unavailable"]);
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    for forbidden in [
        "src/secret.rs",
        ".env",
        "what does",
        "echo",
        "system prompt",
    ] {
        assert!(!text.contains(forbidden), "{forbidden}: {text}");
    }
}
