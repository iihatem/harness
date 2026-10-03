//! 1.2: token counts split into disjoint buckets, so cost never counts a token twice; and a
//! spend-cap 429 is never retried.

mod common;

use std::sync::Arc;

use common::*;
use harness_core::{
    agent::NonInteractive,
    event::{AgentEvent, TurnEndReason},
    message::Usage,
    permission::Mode,
    provider::ProviderError,
    testing::{MockProvider, Script},
};

#[test]
fn cached_input_is_not_counted_twice() {
    // 10,000 prompt tokens of which 8,000 were read from the cache, 500 output tokens of
    // which 200 were reasoning.
    let usage = Usage {
        input_tokens: 10_000,
        output_tokens: 500,
        cached_tokens: 8_000,
        reasoning_tokens: 200,
        ..Usage::default()
    };
    let buckets = usage.buckets();
    assert_eq!(buckets.input, 2_000);
    assert_eq!(buckets.cache_read, 8_000);
    assert_eq!(buckets.output, 500);
    assert_eq!(buckets.reasoning, 200);
    // Reasoning is part of the output: the priced total has 2,000 + 8,000 + 500 tokens.
    assert_eq!(buckets.priced_total(), 10_500);
}

#[test]
fn cache_writes_come_out_of_the_input_and_split_by_tier() {
    let usage = Usage {
        input_tokens: 1_000 + 300 + 200,
        output_tokens: 50,
        cached_tokens: 200,
        cache_write_tokens: 300,
        cache_write_1h_tokens: 100,
        ..Usage::default()
    };
    let buckets = usage.buckets();
    assert_eq!(buckets.input, 1_000);
    assert_eq!(buckets.cache_read, 200);
    assert_eq!(buckets.cache_write, 200, "the 5-minute (or unstated) tier");
    assert_eq!(buckets.cache_write_1h, 100);
    assert_eq!(buckets.priced_total(), 1_000 + 200 + 200 + 100 + 50);
}

#[test]
fn a_report_that_adds_up_wrongly_never_goes_negative() {
    let usage = Usage {
        input_tokens: 5,
        cached_tokens: 9,
        ..Usage::default()
    };
    assert_eq!(usage.buckets().input, 0);
}

#[test]
fn usage_written_before_the_new_fields_still_reads() {
    let old = r#"{"input_tokens":7,"output_tokens":3,"cached_tokens":2}"#;
    let usage: Usage = serde_json::from_str(old).unwrap();
    assert_eq!(usage.input_tokens, 7);
    assert_eq!(usage.cache_write_tokens, 0);
    assert_eq!(usage.reasoning_tokens, 0);
}

fn spend_cap() -> ProviderError {
    ProviderError::Http {
        status: 429,
        body: r#"{"type":"error","error":{"type":"rate_limit_error","message":"enforced_spend_limit_reached: this organization hit its spend limit"}}"#.into(),
        retry_after: None,
    }
}

#[test]
fn a_spend_cap_is_not_a_quota_and_not_retryable() {
    let error = spend_cap();
    assert!(error.is_spend_cap());
    assert!(!error.is_retryable());
    // It is not the subscription limit that a fallback or an auto-resume answers.
    assert!(!error.is_quota_exhausted());
    // An ordinary 429 is still a rate limit, retried.
    let plain = ProviderError::Http {
        status: 429,
        body: "slow down".into(),
        retry_after: None,
    };
    assert!(!plain.is_spend_cap());
    assert!(plain.is_retryable());
}

#[tokio::test(start_paused = true)]
async fn a_spend_cap_429_surfaces_at_once_with_no_retry() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![Script::error(spend_cap()), Script::text("never")]);
    let mut agent = agent(
        provider.clone(),
        Mode::Auto,
        Arc::new(NonInteractive),
        dir.path(),
    );
    let (reason, events) = run(&mut agent, "go").await;
    assert_eq!(reason, TurnEndReason::Error);
    assert_eq!(provider.requests().len(), 1, "no second request");
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::Retrying { .. }))
    );
    let message = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::Error { message, .. } => Some(message.clone()),
            _ => None,
        })
        .expect("an error event");
    assert!(message.contains("spend limit"), "{message}");
}
