//! 1.10: a subscription limit that ends a turn is told to the frontend with its reset time, so an
//! interactive session can offer to resume.

mod common;

use std::sync::Arc;

use common::*;
use harness_core::{
    agent::NonInteractive,
    event::{AgentEvent, TurnEndReason},
    permission::Mode,
    provider::ProviderError,
    testing::{MockProvider, Script},
};

fn limit(body: &str) -> Script {
    Script::error(ProviderError::Http {
        status: 429,
        body: body.into(),
        retry_after: None,
    })
}

#[tokio::test]
async fn a_limit_with_a_reset_time_is_an_event_before_the_error() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![limit(
        r#"{"error":{"type":"usage_limit_reached","resets_at":1790946000}}"#,
    )]);
    let mut agent = agent(provider, Mode::Auto, Arc::new(NonInteractive), dir.path());
    let (reason, events) = run(&mut agent, "go").await;
    assert_eq!(reason, TurnEndReason::Error);
    let at = events
        .iter()
        .position(|e| {
            matches!(
                e,
                AgentEvent::LimitReached {
                    resets_at: 1_790_946_000
                }
            )
        })
        .unwrap_or_else(|| panic!("{events:?}"));
    let error = events
        .iter()
        .position(|e| matches!(e, AgentEvent::Error { .. }))
        .unwrap();
    assert!(at < error, "{events:?}");
}

#[tokio::test]
async fn a_limit_that_says_when_it_resets_in_seconds_gets_a_time() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![limit(
        r#"{"error":{"type":"usage_limit_reached","resets_in_seconds":600}}"#,
    )]);
    let mut agent = agent(provider, Mode::Auto, Arc::new(NonInteractive), dir.path());
    let (_, events) = run(&mut agent, "go").await;
    let now = harness_core::time::now_unix();
    let at = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::LimitReached { resets_at } => Some(*resets_at),
            _ => None,
        })
        .expect("a limit event");
    assert!((now + 590..=now + 610).contains(&at), "{at} vs {now}");
}

#[tokio::test]
async fn a_limit_with_no_reset_time_and_other_errors_give_no_event() {
    for body in [
        r#"{"error":{"type":"usage_limit_reached"}}"#,
        r#"{"error":{"type":"insufficient_quota"}}"#,
        "slow down",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let provider = MockProvider::new(vec![
            limit(body),
            limit(body),
            limit(body),
            limit(body),
            limit(body),
        ]);
        let mut agent = agent(provider, Mode::Auto, Arc::new(NonInteractive), dir.path());
        let (_, events) =
            tokio::time::timeout(std::time::Duration::from_secs(60), run(&mut agent, "go"))
                .await
                .unwrap();
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::LimitReached { .. })),
            "{body}: {events:?}"
        );
    }
}
