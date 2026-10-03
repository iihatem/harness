//! 1.3: the runtime reports one record for every model request, including a failed or stopped
//! one and the summary request of a compaction, to the meter it is given.

mod common;

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use common::*;
use harness_core::{
    agent::NonInteractive,
    event::{AgentEvent, TurnEndReason},
    message::Usage,
    meter::{AccountKind, Avoided, Meter, RequestCost, RequestRecord},
    permission::Mode,
    provider::{FinishReason, ProviderError, ProviderEvent},
    testing::{MockProvider, Script},
};
use serde_json::json;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Recording {
    records: Mutex<Vec<RequestRecord>>,
    warnings: Mutex<Vec<String>>,
}

impl Meter for Recording {
    fn record_request(&self, request: &RequestRecord) -> RequestCost {
        self.records.lock().unwrap().push(request.clone());
        RequestCost {
            account: AccountKind::ApiKey,
            billed_usd: Some(0.25),
            list_usd: Some(0.25),
            avoided: Avoided::NotApplicable,
        }
    }

    fn take_warnings(&self) -> Vec<String> {
        std::mem::take(&mut *self.warnings.lock().unwrap())
    }
}

fn reply_with_usage(call: Option<&str>, input: u64, output: u64) -> Script {
    let mut events = Vec::new();
    if let Some(id) = call {
        events.push(Ok(ProviderEvent::ToolCall(
            harness_core::message::ToolCall {
                id: id.into(),
                name: "echo".into(),
                arguments: json!({"text": "x"}).to_string(),
            },
        )));
    } else {
        events.push(Ok(ProviderEvent::TextDelta("done".into())));
    }
    events.push(Ok(ProviderEvent::Usage(Usage {
        input_tokens: input,
        output_tokens: output,
        ..Usage::default()
    })));
    events.push(Ok(ProviderEvent::Finished(if call.is_some() {
        FinishReason::ToolCalls
    } else {
        FinishReason::Stop
    })));
    Script::Reply(events)
}

fn unavailable() -> Script {
    Script::error(ProviderError::Http {
        status: 503,
        body: String::new(),
        retry_after: None,
    })
}

// A turn makes 3 model requests and the third fails with HTTP 503 after retries: 3 records, the
// first two `ok`, the third `error:unavailable`.
#[tokio::test(start_paused = true)]
async fn one_record_per_request_and_a_failed_one_is_recorded_once() {
    let dir = tempfile::tempdir().unwrap();
    let mut script = vec![
        reply_with_usage(Some("c1"), 100, 10),
        reply_with_usage(Some("c2"), 200, 20),
    ];
    script.extend((0..5).map(|_| unavailable()));
    let provider = MockProvider::new(script);
    let meter = Arc::new(Recording::default());
    let mut agent =
        agent(provider, Mode::Auto, Arc::new(NonInteractive), dir.path()).with_meter(meter.clone());
    let (reason, _) = run(&mut agent, "go").await;
    assert_eq!(reason, TurnEndReason::Error);
    let records = meter.records.lock().unwrap().clone();
    let outcomes: Vec<&str> = records.iter().map(|r| r.outcome.as_str()).collect();
    assert_eq!(outcomes, ["ok", "ok", "error:unavailable"]);
    assert_eq!(records[0].usage.input_tokens, 100);
    assert_eq!(records[1].usage.output_tokens, 20);
    assert_eq!(records[2].usage, Usage::default());
    for record in &records {
        assert_eq!(record.model, "mock/m1");
        assert_eq!(record.role, "main");
        assert_eq!(record.session, agent.session().id());
        assert!(!record.local);
    }
    // The failed request's duration includes the waits between its attempts.
    assert!(
        records[2].duration >= Duration::from_secs(1),
        "{:?}",
        records[2]
    );
}

#[tokio::test]
async fn a_reply_that_reports_no_usage_is_recorded_with_no_tokens() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![Script::text("hi")]);
    let meter = Arc::new(Recording::default());
    let mut agent =
        agent(provider, Mode::Auto, Arc::new(NonInteractive), dir.path()).with_meter(meter.clone());
    run(&mut agent, "go").await;
    let records = meter.records.lock().unwrap().clone();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].outcome, "ok");
    assert_eq!(records[0].usage, Usage::default());
}

#[tokio::test]
async fn a_request_the_user_stopped_is_recorded_as_interrupted() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![Script::Hang(vec![ProviderEvent::TextDelta(
        "partial".into(),
    )])]);
    let meter = Arc::new(Recording::default());
    let mut agent =
        agent(provider, Mode::Auto, Arc::new(NonInteractive), dir.path()).with_meter(meter.clone());
    let cancel = CancellationToken::new();
    let stop = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        stop.cancel();
    });
    let (reason, _) = run_with(&mut agent, "go", cancel).await;
    assert_eq!(reason, TurnEndReason::Interrupted);
    let records = meter.records.lock().unwrap().clone();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].outcome, "error:interrupted");
}

#[tokio::test]
async fn a_compaction_summary_is_a_recorded_request() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::text("hi"),
        Script::Reply(vec![
            Ok(ProviderEvent::TextDelta("the summary".into())),
            Ok(ProviderEvent::Usage(Usage {
                input_tokens: 5_000,
                output_tokens: 40,
                ..Usage::default()
            })),
            Ok(ProviderEvent::Finished(FinishReason::Stop)),
        ]),
    ]);
    let meter = Arc::new(Recording::default());
    let mut agent =
        agent(provider, Mode::Auto, Arc::new(NonInteractive), dir.path()).with_meter(meter.clone());
    run(&mut agent, "hello").await;
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    agent
        .compact(None, &tx, CancellationToken::new())
        .await
        .unwrap();
    let records = meter.records.lock().unwrap().clone();
    assert_eq!(records.len(), 2);
    assert_eq!(records[1].usage.input_tokens, 5_000);
    assert_eq!(records[1].outcome, "ok");
}

#[tokio::test]
async fn what_the_meter_cannot_keep_reaches_the_user_as_a_warning() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![Script::text("hi")]);
    let meter = Arc::new(Recording::default());
    meter
        .warnings
        .lock()
        .unwrap()
        .push("cannot write the usage ledger: disk full".into());
    let mut agent =
        agent(provider, Mode::Auto, Arc::new(NonInteractive), dir.path()).with_meter(meter.clone());
    let (_, events) = run(&mut agent, "go").await;
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::Warning { message } if message.contains("usage ledger")
    )));
}

#[test]
fn failures_have_a_kind_for_the_ledger() {
    let http = |status: u16, body: &str| ProviderError::Http {
        status,
        body: body.into(),
        retry_after: None,
    };
    assert_eq!(http(503, "").kind(), "unavailable");
    assert_eq!(ProviderError::Network("x".into()).kind(), "unavailable");
    assert_eq!(http(429, "slow").kind(), "rate_limited");
    assert_eq!(
        http(429, r#"{"error":{"type":"usage_limit_reached"}}"#).kind(),
        "quota"
    );
    assert_eq!(
        http(429, "enforced_spend_limit_reached").kind(),
        "spend_cap"
    );
    assert_eq!(http(401, "").kind(), "auth");
    assert_eq!(http(404, "").kind(), "rejected");
    assert_eq!(
        http(400, "This model's maximum context length is 8192 tokens").kind(),
        "context_overflow"
    );
    assert_eq!(ProviderError::Protocol("x".into()).kind(), "protocol");
}

// Each request's cost reaches the frontends as an event, after the request ended, so the status
// line and `--json` can show it without a price table of their own.
#[tokio::test]
async fn a_request_s_cost_is_an_event() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![reply_with_usage(None, 100, 10)]);
    let meter = Arc::new(Recording::default());
    let mut agent =
        agent(provider, Mode::Auto, Arc::new(NonInteractive), dir.path()).with_meter(meter.clone());
    let (_, events) = run(&mut agent, "go").await;
    let metered: Vec<&AgentEvent> = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::Metered { .. }))
        .collect();
    assert_eq!(metered.len(), 1, "{events:?}");
    let AgentEvent::Metered { model, cost } = metered[0] else {
        unreachable!()
    };
    assert_eq!(model, "mock/m1");
    assert_eq!(cost.billed_usd, Some(0.25));
    assert_eq!(cost.account, AccountKind::ApiKey);
}
