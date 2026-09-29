mod common;

use std::{sync::Arc, time::Duration};

use common::*;
use harness_core::agent::NonInteractive;
use harness_core::event::{AgentEvent, TurnEndReason};
use harness_core::message::Message;
use harness_core::permission::Mode;
use harness_core::provider::{ProviderError, ProviderEvent};
use harness_core::retry::RetryPolicy;
use harness_core::testing::{MockProvider, Script};
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn http(status: u16, retry_after: Option<Duration>) -> ProviderError {
    ProviderError::Http {
        status,
        body: String::new(),
        retry_after,
    }
}

fn retries(events: &[AgentEvent]) -> Vec<(u32, u64)> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Retrying {
                attempt, delay_ms, ..
            } => Some((*attempt, *delay_ms)),
            _ => None,
        })
        .collect()
}

#[test]
fn backoff_grows_is_capped_and_honours_retry_after() {
    let policy = RetryPolicy::default();
    assert_eq!(
        policy.delay(1, Some(Duration::from_secs(3))),
        Duration::from_secs(3)
    );
    let first = policy.delay(1, None);
    assert!(first >= Duration::from_millis(500) && first < Duration::from_millis(1000));
    let third = policy.delay(3, None);
    assert!(third >= Duration::from_millis(2000) && third < Duration::from_millis(2500));
    assert!(policy.delay(20, None) < Duration::from_millis(30_500));
}

#[tokio::test(start_paused = true)]
async fn rate_limits_are_retried_after_the_requested_delay() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::error(http(429, Some(Duration::from_secs(3)))),
        Script::text("ok"),
    ]);
    let mut agent = agent(provider, Mode::Auto, Arc::new(NonInteractive), dir.path());
    let started = tokio::time::Instant::now();
    let (reason, events) = run(&mut agent, "go").await;
    assert_eq!(reason, TurnEndReason::Completed);
    assert_eq!(retries(&events), vec![(1, 3000)]);
    assert!(started.elapsed() >= Duration::from_secs(3));
}

#[tokio::test(start_paused = true)]
async fn gives_up_after_five_attempts_and_the_session_stays_usable() {
    let dir = tempfile::tempdir().unwrap();
    let mut script: Vec<Script> = (0..5).map(|_| Script::error(http(503, None))).collect();
    script.push(Script::text("back"));
    let provider = MockProvider::new(script);
    let mut agent = agent(
        provider.clone(),
        Mode::Auto,
        Arc::new(NonInteractive),
        dir.path(),
    );
    let (reason, events) = run(&mut agent, "go").await;
    assert_eq!(reason, TurnEndReason::Error);
    assert_eq!(retries(&events).len(), 4);
    assert_eq!(provider.requests().len(), 5);
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Error { .. })));
    let (again, _) = run(&mut agent, "retry later").await;
    assert_eq!(again, TurnEndReason::Completed);
}

#[tokio::test(start_paused = true)]
async fn non_retryable_errors_are_not_retried() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![Script::error(http(401, None))]);
    let mut agent = agent(
        provider.clone(),
        Mode::Auto,
        Arc::new(NonInteractive),
        dir.path(),
    );
    let (reason, events) = run(&mut agent, "go").await;
    assert_eq!(reason, TurnEndReason::Error);
    assert!(retries(&events).is_empty());
    assert_eq!(provider.requests().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn errors_after_streamed_output_are_not_retried_and_keep_the_partial_text() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![Script::Reply(vec![
        Ok(ProviderEvent::TextDelta("partial".into())),
        Err(ProviderError::Network("reset".into())),
    ])]);
    let mut agent = agent(provider, Mode::Auto, Arc::new(NonInteractive), dir.path());
    let (reason, events) = run(&mut agent, "go").await;
    assert_eq!(reason, TurnEndReason::Error);
    assert!(retries(&events).is_empty());
    assert!(
        matches!(&agent.history()[1], Message::Assistant { content, .. } if content == "partial")
    );
}

#[tokio::test(start_paused = true)]
async fn interrupt_during_the_model_stream_keeps_partial_output() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![Script::Hang(vec![ProviderEvent::TextDelta(
        "thinking".into(),
    )])]);
    let mut agent = agent(provider, Mode::Auto, Arc::new(NonInteractive), dir.path());
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        trigger.cancel();
    });
    let (reason, events) = run_with(&mut agent, "go", cancel).await;
    assert_eq!(reason, TurnEndReason::Interrupted);
    assert_eq!(
        events.last(),
        Some(&AgentEvent::TurnFinished {
            reason: TurnEndReason::Interrupted
        })
    );
    assert!(
        matches!(&agent.history()[1], Message::Assistant { content, .. } if content == "thinking")
    );
}

#[tokio::test(start_paused = true)]
async fn interrupt_during_a_tool_answers_every_pending_call() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::Reply(vec![
            Ok(ProviderEvent::ToolCall(harness_core::message::ToolCall {
                id: "a".into(),
                name: "sleepy".into(),
                arguments: "{}".into(),
            })),
            Ok(ProviderEvent::ToolCall(harness_core::message::ToolCall {
                id: "b".into(),
                name: "echo".into(),
                arguments: r#"{"text":"x"}"#.into(),
            })),
        ]),
        Script::text("must not be requested"),
    ]);
    let mut agent = agent(
        provider.clone(),
        Mode::Auto,
        Arc::new(NonInteractive),
        dir.path(),
    );
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        trigger.cancel();
    });
    let (reason, _) = run_with(&mut agent, "go", cancel).await;
    assert_eq!(reason, TurnEndReason::Interrupted);
    assert_eq!(provider.requests().len(), 1);
    let tool_results: Vec<&str> = agent
        .history()
        .iter()
        .filter_map(|m| match m {
            Message::Tool { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        tool_results,
        ["a", "b"],
        "every tool call must have a result"
    );
}

// Review Focus: a Retry-After long enough that we refuse to wait automatically (see
// `a_retry_after_longer_than_a_minute_fails_the_turn_instead_of_waiting`) must not be confused
// with an ordinary, retried backoff that the user interrupts while it's waiting.
#[tokio::test(start_paused = true)]
async fn interrupt_during_backoff_stops_waiting() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![Script::error(http(
        429,
        Some(Duration::from_secs(50)),
    ))]);
    let mut agent = agent(provider, Mode::Auto, Arc::new(NonInteractive), dir.path());
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(1)).await;
        trigger.cancel();
    });
    let started = tokio::time::Instant::now();
    let (reason, _) = run_with(&mut agent, "go", cancel).await;
    assert_eq!(reason, TurnEndReason::Interrupted);
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[tokio::test(start_paused = true)]
async fn a_retry_after_longer_than_a_minute_fails_the_turn_instead_of_waiting() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![Script::error(http(
        429,
        Some(Duration::from_secs(3600)),
    ))]);
    let mut agent = agent(provider, Mode::Auto, Arc::new(NonInteractive), dir.path());
    let (reason, events) = run(&mut agent, "go").await;
    assert_eq!(reason, TurnEndReason::Error);
    assert!(retries(&events).is_empty(), "{:?}", retries(&events));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Error { message, .. } if message.contains("3600"))),
        "{events:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn tool_calls_still_run_normally_without_interrupts() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call("c1", "echo", json!({"text": "fine"})),
        Script::text("ok"),
    ]);
    let mut agent = agent(provider, Mode::Auto, Arc::new(NonInteractive), dir.path());
    let (reason, events) = run(&mut agent, "go").await;
    assert_eq!(reason, TurnEndReason::Completed);
    assert_eq!(finished_outputs(&events), vec![("fine".to_string(), false)]);
}

fn quota(body: &str) -> ProviderError {
    ProviderError::Http {
        status: 429,
        body: body.to_string(),
        retry_after: None,
    }
}

const USAGE_LIMIT: &str = r#"{"error":{"type":"usage_limit_reached","message":"The usage limit has been reached","plan_type":"plus","resets_at":1790208000}}"#;

#[test]
fn exhausted_quotas_are_not_worth_retrying() {
    for body in [
        USAGE_LIMIT,
        r#"{"error":{"message":"You exceeded your current quota, please check your plan and billing details.","type":"insufficient_quota","code":"insufficient_quota"}}"#,
        r#"{"error":{"type":"usage_not_included","message":"Upgrade to use this model"}}"#,
    ] {
        let error = quota(body);
        assert!(error.is_quota_exhausted(), "{body}");
        assert!(!error.is_retryable(), "{body}");
    }
    let busy = quota(r#"{"error":{"type":"rate_limit_exceeded","message":"Slow down"}}"#);
    assert!(!busy.is_quota_exhausted());
    assert!(busy.is_retryable());
    assert_eq!(quota(USAGE_LIMIT).resets_at(), Some(1_790_208_000));
    let relative = quota(r#"{"error":{"type":"usage_limit_reached","resets_in_seconds":60}}"#);
    let resets = relative.resets_at().unwrap();
    let now = harness_core::time::now_unix();
    assert!((now + 55..=now + 65).contains(&resets), "{resets} vs {now}");
}

// Review C, M7: a reset time far out does not overflow.
#[test]
fn a_reset_time_far_out_does_not_overflow() {
    let far = quota(
        r#"{"error":{"type":"usage_limit_reached","resets_in_seconds":18446744073709551615}}"#,
    );
    assert_eq!(far.resets_at(), Some(u64::MAX));
}

// Review C, M7: a quota 429 that also asks to wait longer than harness waits still tells when the
// limit resets.
#[tokio::test(start_paused = true)]
async fn a_usage_limit_with_a_long_retry_after_keeps_its_reset_time() {
    let dir = tempfile::tempdir().unwrap();
    let limited = ProviderError::Http {
        status: 429,
        body: USAGE_LIMIT.to_string(),
        retry_after: Some(std::time::Duration::from_secs(3 * 3600)),
    };
    let provider = MockProvider::new(vec![Script::error(limited)]);
    let mut agent = agent(provider, Mode::Auto, Arc::new(NonInteractive), dir.path());
    let (reason, events) = run(&mut agent, "go").await;
    assert_eq!(reason, TurnEndReason::Error);
    let message = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::Error { message, .. } => Some(message.clone()),
            _ => None,
        })
        .unwrap();
    assert!(message.contains("2026-09-24T00:00:00Z"), "{message}");
    assert!(message.contains("usage limit"), "{message}");
}

// Spec: "Subscription limit reached".
#[tokio::test(start_paused = true)]
async fn a_usage_limit_ends_the_turn_with_its_reset_time() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::error(quota(USAGE_LIMIT)),
        Script::text("later"),
    ]);
    let mut agent = agent(
        provider.clone(),
        Mode::Auto,
        Arc::new(NonInteractive),
        dir.path(),
    );
    let (reason, events) = run(&mut agent, "go").await;
    assert_eq!(reason, TurnEndReason::Error);
    assert!(retries(&events).is_empty());
    assert_eq!(provider.requests().len(), 1);
    let message = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::Error { message, .. } => Some(message.clone()),
            _ => None,
        })
        .unwrap();
    assert!(message.contains("2026-09-24T00:00:00Z"), "{message}");
    assert!(message.contains("--model"), "{message}");
    // The session stays usable.
    let (reason, _) = run(&mut agent, "again").await;
    assert_eq!(reason, TurnEndReason::Completed);
}

/// The error a server that sent nothing within the wait for the reply's first data ends the
/// stream with.
fn no_start(local: bool) -> ProviderError {
    ProviderError::NoStart {
        message: format!(
            "the server did not start its reply within {}",
            if local { "30 min" } else { "300 s" }
        ),
        local,
    }
}

fn error_message(events: &[AgentEvent]) -> String {
    events
        .iter()
        .find_map(|e| match e {
            AgentEvent::Error { message, .. } => Some(message.clone()),
            _ => None,
        })
        .unwrap()
}

// Ruling on the final review's I-1: a local server that did not start its reply within 30
// minutes is not asked again, since a retry would start what it was doing (loading the model,
// reading the prompt) over, and a headless run would wait hours. The message says what to check.
#[tokio::test(start_paused = true)]
async fn a_local_server_that_never_starts_its_reply_is_not_retried() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![Script::error(no_start(true)), Script::text("late")]);
    let mut agent = agent(
        provider.clone(),
        Mode::Auto,
        Arc::new(NonInteractive),
        dir.path(),
    );
    let (reason, events) = run(&mut agent, "go").await;
    assert_eq!(reason, TurnEndReason::Error);
    assert!(retries(&events).is_empty(), "{events:?}");
    assert_eq!(provider.requests().len(), 1);
    let message = error_message(&events);
    for part in ["local server", "30 min", "not retr", "model", "smaller"] {
        assert!(message.contains(part), "{part}: {message}");
    }
}

// The final review's I-1, optional part: a hosted provider that did not start its reply within
// 300 s is asked once more, not four times, so a headless run waits 10 minutes at most.
#[tokio::test(start_paused = true)]
async fn a_hosted_first_data_timeout_is_retried_once() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::error(no_start(false)),
        Script::error(no_start(false)),
        Script::text("late"),
    ]);
    let mut hosted = agent(
        provider.clone(),
        Mode::Auto,
        Arc::new(NonInteractive),
        dir.path(),
    );
    let (reason, events) = run(&mut hosted, "go").await;
    assert_eq!(reason, TurnEndReason::Error);
    assert_eq!(retries(&events).len(), 1, "{events:?}");
    assert_eq!(provider.requests().len(), 2);
    assert!(
        error_message(&events).contains("did not start its reply within 300 s"),
        "{events:?}"
    );
    // Once is enough when the retry answers.
    let provider = MockProvider::new(vec![Script::error(no_start(false)), Script::text("ok")]);
    let mut hosted = agent(
        provider.clone(),
        Mode::Auto,
        Arc::new(NonInteractive),
        dir.path(),
    );
    let (reason, _) = run(&mut hosted, "go").await;
    assert_eq!(reason, TurnEndReason::Completed);
    // Other network errors keep their five attempts.
    let mut script: Vec<Script> = (0..4)
        .map(|_| Script::error(ProviderError::Network("reset".into())))
        .collect();
    script.push(Script::text("ok"));
    let provider = MockProvider::new(script);
    let mut hosted = agent(provider, Mode::Auto, Arc::new(NonInteractive), dir.path());
    let (reason, events) = run(&mut hosted, "go").await;
    assert_eq!(reason, TurnEndReason::Completed);
    assert_eq!(retries(&events).len(), 4);
}
