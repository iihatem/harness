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
    meter::{
        AccountKind, Avoided, Meter, RequestCost, RequestRecord, Window, WindowSnapshot,
        WindowSource,
    },
    permission::Mode,
    provider::{FinishReason, ProviderError, ProviderEvent},
    redact::{EventRedactor, Redactor},
    testing::{MockProvider, Script},
};
use serde_json::json;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Recording {
    records: Mutex<Vec<RequestRecord>>,
    warnings: Mutex<Vec<String>>,
    windows: Mutex<Vec<WindowSnapshot>>,
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

    fn record_window(&self, snapshot: &WindowSnapshot) {
        self.windows.lock().unwrap().push(snapshot.clone());
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

// Paused time: the cancel comes when the agent is waiting on the hung stream, never before it
// has started the request, however loaded the machine is.
#[tokio::test(start_paused = true)]
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

fn snapshot() -> WindowSnapshot {
    WindowSnapshot {
        windows: vec![Window {
            window_minutes: Some(300),
            used_percent: Some(62.0),
            resets_at: Some(1_790_946_000),
            source: WindowSource::Header,
        }],
        observed_at: 1_790_943_000,
    }
}

// A provider that learns where a subscription's windows stand says so; the runtime shows it as
// an event and tells the meter, which names it in the request's record.
#[tokio::test]
async fn window_snapshots_are_events_and_reach_the_meter() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![Script::Reply(vec![
        Ok(ProviderEvent::RateLimits(snapshot())),
        Ok(ProviderEvent::TextDelta("hi".into())),
        Ok(ProviderEvent::Finished(FinishReason::Stop)),
    ])]);
    let meter = Arc::new(Recording::default());
    let mut agent =
        agent(provider, Mode::Auto, Arc::new(NonInteractive), dir.path()).with_meter(meter.clone());
    let (_, events) = run(&mut agent, "go").await;
    assert!(events.contains(&AgentEvent::RateLimits {
        snapshot: snapshot()
    }));
    assert_eq!(*meter.windows.lock().unwrap(), vec![snapshot()]);
}

// A snapshot arrives with a response's headers, before its text: it must not make the redactor
// release text it holds back because it could still become a secret.
#[test]
fn a_window_snapshot_does_not_release_text_held_back_for_a_secret() {
    let redactor = Redactor::default();
    redactor.add("sk-secret-0123456789");
    let mut events = EventRedactor::new(Arc::new(redactor));
    let first = events.push(AgentEvent::TextDelta {
        text: "the key is sk-secret-01".into(),
    });
    let shown: String = first
        .iter()
        .filter_map(|e| match e {
            AgentEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(!shown.contains("sk-secret"), "{shown}");
    let between = events.push(AgentEvent::RateLimits {
        snapshot: snapshot(),
    });
    assert_eq!(between.len(), 1, "{between:?}");
    assert!(matches!(between[0], AgentEvent::RateLimits { .. }));
}

// A4: the usage a provider reported before a request was stopped, or failed, is billed.
#[tokio::test(start_paused = true)]
async fn a_stopped_request_keeps_the_usage_reported_before_it_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![Script::Hang(vec![
        ProviderEvent::Usage(Usage {
            input_tokens: 700,
            ..Usage::default()
        }),
        ProviderEvent::TextDelta("partial".into()),
    ])]);
    let meter = Arc::new(Recording::default());
    let mut agent =
        agent(provider, Mode::Auto, Arc::new(NonInteractive), dir.path()).with_meter(meter.clone());
    let cancel = CancellationToken::new();
    let stop = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        stop.cancel();
    });
    run_with(&mut agent, "go", cancel).await;
    let records = meter.records.lock().unwrap().clone();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].outcome, "error:interrupted");
    assert_eq!(records[0].usage.input_tokens, 700);
}

#[tokio::test(start_paused = true)]
async fn a_request_that_fails_mid_stream_keeps_the_usage_reported_before() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![Script::Reply(vec![
        Ok(ProviderEvent::Usage(Usage {
            input_tokens: 900,
            ..Usage::default()
        })),
        Ok(ProviderEvent::TextDelta("partial".into())),
        Err(ProviderError::Protocol("broken".into())),
    ])]);
    let meter = Arc::new(Recording::default());
    let mut agent =
        agent(provider, Mode::Auto, Arc::new(NonInteractive), dir.path()).with_meter(meter.clone());
    run(&mut agent, "go").await;
    let records = meter.records.lock().unwrap().clone();
    assert_eq!(records.len(), 1);
    assert!(records[0].outcome.starts_with("error:"), "{:?}", records[0]);
    assert_eq!(records[0].usage.input_tokens, 900);
}

fn usage_events(input: u64) -> Vec<Result<ProviderEvent, ProviderError>> {
    vec![Ok(ProviderEvent::Usage(Usage {
        input_tokens: input,
        output_tokens: 7,
        ..Usage::default()
    }))]
}

/// A session with one answered turn, then a compaction whose summary request is `summary`.
/// Returns the records and how the compaction ended.
async fn compaction_with(
    summary: Vec<Script>,
    cancel: Option<CancellationToken>,
) -> (Vec<RequestRecord>, bool) {
    let dir = tempfile::tempdir().unwrap();
    let mut script = vec![Script::text("hi")];
    script.extend(summary);
    let provider = MockProvider::new(script);
    let meter = Arc::new(Recording::default());
    let mut agent =
        agent(provider, Mode::Auto, Arc::new(NonInteractive), dir.path()).with_meter(meter.clone());
    run(&mut agent, "hello").await;
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let result = agent.compact(None, &tx, cancel.unwrap_or_default()).await;
    let records = meter.records.lock().unwrap().clone();
    (records, result.is_ok())
}

// Final review, Important 2: a summary request is billed whatever becomes of its result.
#[tokio::test]
async fn a_summary_that_was_cut_off_is_metered() {
    let mut events = vec![Ok(ProviderEvent::TextDelta("half a summ".into()))];
    events.extend(usage_events(5_000));
    events.push(Ok(ProviderEvent::Finished(FinishReason::Length)));
    let (records, ok) = compaction_with(vec![Script::Reply(events)], None).await;
    assert!(!ok);
    assert_eq!(records.len(), 2, "{records:?}");
    assert_eq!(records[1].usage.input_tokens, 5_000);
    assert_eq!(records[1].outcome, "error:incomplete");
}

#[tokio::test]
async fn an_empty_summary_is_metered() {
    let mut events = usage_events(5_000);
    events.push(Ok(ProviderEvent::Finished(FinishReason::Stop)));
    let (records, ok) = compaction_with(vec![Script::Reply(events)], None).await;
    assert!(!ok);
    assert_eq!(records.len(), 2, "{records:?}");
    assert_eq!(records[1].usage.input_tokens, 5_000);
    assert_eq!(records[1].outcome, "error:incomplete");
}

#[tokio::test]
async fn a_summary_request_that_failed_after_reporting_usage_is_metered() {
    let mut events = usage_events(5_000);
    events.push(Err(ProviderError::Http {
        status: 404,
        body: String::new(),
        retry_after: None,
    }));
    let (records, ok) = compaction_with(vec![Script::Reply(events)], None).await;
    assert!(!ok);
    assert_eq!(records.len(), 2, "{records:?}");
    assert_eq!(records[1].usage.input_tokens, 5_000);
    assert_eq!(records[1].outcome, "error:rejected");
}

#[tokio::test]
async fn a_summary_request_rejected_as_too_long_is_metered_with_its_usage() {
    let mut events = usage_events(5_000);
    events.push(Err(ProviderError::Http {
        status: 400,
        body: "This model's maximum context length is 8192 tokens".into(),
        retry_after: None,
    }));
    let (records, ok) = compaction_with(vec![Script::Reply(events)], None).await;
    assert!(!ok);
    assert!(records.len() >= 2, "{records:?}");
    assert_eq!(records[1].usage.input_tokens, 5_000);
    assert_eq!(records[1].outcome, "error:context_overflow");
}

#[tokio::test(start_paused = true)]
async fn a_summary_request_the_user_stopped_is_metered_with_its_usage() {
    let cancel = CancellationToken::new();
    let stop = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        stop.cancel();
    });
    let (records, ok) = compaction_with(
        vec![Script::Hang(
            usage_events(5_000)
                .into_iter()
                .map(Result::unwrap)
                .collect(),
        )],
        Some(cancel),
    )
    .await;
    assert!(!ok);
    assert_eq!(records.len(), 2, "{records:?}");
    assert_eq!(records[1].usage.input_tokens, 5_000);
    assert_eq!(records[1].outcome, "error:interrupted");
}

// Minor 3: what an attempt reported before it was retried is its own ledger record, with the
// outcome `error:<kind>`; the attempt that answers is the request's own record.
#[tokio::test(start_paused = true)]
async fn usage_an_attempt_reported_before_it_was_retried_is_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let mut failing = usage_events(300);
    failing.push(Err(ProviderError::Http {
        status: 503,
        body: String::new(),
        retry_after: None,
    }));
    let provider = MockProvider::new(vec![
        Script::Reply(failing),
        reply_with_usage(None, 100, 10),
    ]);
    let meter = Arc::new(Recording::default());
    let mut agent =
        agent(provider, Mode::Auto, Arc::new(NonInteractive), dir.path()).with_meter(meter.clone());
    run(&mut agent, "go").await;
    let records = meter.records.lock().unwrap().clone();
    let seen: Vec<(&str, u64)> = records
        .iter()
        .map(|r| (r.outcome.as_str(), r.usage.input_tokens))
        .collect();
    assert_eq!(seen, [("error:unavailable", 300), ("ok", 100)]);
}

#[tokio::test(start_paused = true)]
async fn usage_a_summary_attempt_reported_before_it_was_retried_is_recorded() {
    let mut failing = usage_events(300);
    failing.push(Err(ProviderError::Http {
        status: 503,
        body: String::new(),
        retry_after: None,
    }));
    let ok = vec![
        Ok(ProviderEvent::TextDelta("the summary".into())),
        Ok(ProviderEvent::Usage(Usage {
            input_tokens: 100,
            ..Usage::default()
        })),
        Ok(ProviderEvent::Finished(FinishReason::Stop)),
    ];
    let (records, done) =
        compaction_with(vec![Script::Reply(failing), Script::Reply(ok)], None).await;
    assert!(done);
    let seen: Vec<(&str, u64)> = records[1..]
        .iter()
        .map(|r| (r.outcome.as_str(), r.usage.input_tokens))
        .collect();
    assert_eq!(seen, [("error:unavailable", 300), ("ok", 100)]);
}
