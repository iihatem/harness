//! Per-turn statistics and where the context goes.

mod common;

use std::{sync::Arc, time::Duration};

use common::{Echo, agent, run};
use harness_core::{
    agent::{Agent, AgentConfig, NonInteractive},
    engine::{EngineConfig, PermissionEngine},
    event::{AgentEvent, TurnEndReason},
    message::{ChatRequest, Usage},
    permission::Mode,
    provider::{FinishReason, Provider, ProviderEvent, ProviderStream},
    testing::{MockProvider, Script},
    tokens::DEFAULT_CONTEXT_WINDOW,
    tool::{ToolContext, ToolRegistry},
};
use serde_json::json;

/// A provider whose reply starts 400 ms after the request and streams for a second.
struct Slow;

impl Provider for Slow {
    fn stream(&self, _request: ChatRequest) -> ProviderStream {
        Box::pin(async_stream())
    }
}

fn async_stream()
-> impl futures::Stream<Item = Result<ProviderEvent, harness_core::provider::ProviderError>> + Send
{
    futures::stream::unfold(0, |step| async move {
        let event = match step {
            0 => {
                tokio::time::sleep(Duration::from_millis(400)).await;
                ProviderEvent::TextDelta("hel".into())
            }
            1 => {
                tokio::time::sleep(Duration::from_millis(1000)).await;
                ProviderEvent::TextDelta("lo".into())
            }
            2 => ProviderEvent::Usage(Usage {
                input_tokens: 1_000,
                output_tokens: 50,
                cached_tokens: 800,
            }),
            3 => ProviderEvent::Finished(FinishReason::Stop),
            _ => return None,
        };
        Some((Ok(event), step + 1))
    })
}

fn stats(events: &[AgentEvent]) -> Vec<AgentEvent> {
    events
        .iter()
        .filter(|e| matches!(e, AgentEvent::TurnStats { .. }))
        .cloned()
        .collect()
}

#[tokio::test(start_paused = true)]
async fn a_turn_reports_its_stats_just_before_it_finishes() {
    let dir = tempfile::tempdir().unwrap();
    let policy = Arc::new(PermissionEngine::new(EngineConfig {
        mode: Mode::Auto,
        workspace: dir.path().to_path_buf(),
        read_dirs: vec![],
        rules: Default::default(),
        sandbox_available: false,
        writes_need_approval: false,
    }));
    let mut agent = Agent::new(
        Arc::new(Slow),
        ToolRegistry::new(vec![Arc::new(Echo)]),
        policy,
        Arc::new(NonInteractive),
        AgentConfig::new("mock/m1", "m1", "system prompt", dir.path().join("out")),
        ToolContext::new(dir.path()),
    );
    let (reason, events) = run(&mut agent, "hi").await;
    assert_eq!(reason, TurnEndReason::Completed);
    let n = events.len();
    assert_eq!(
        events[n - 2],
        AgentEvent::TurnStats {
            model: "mock/m1".into(),
            time_to_first_token_ms: Some(400),
            generation_ms: 1000,
            input_tokens: 1_000,
            output_tokens: 50,
            cached_tokens: 800,
        }
    );
    assert!(matches!(events[n - 1], AgentEvent::TurnFinished { .. }));
}

#[tokio::test]
async fn stats_add_up_the_turns_model_calls() {
    let dir = tempfile::tempdir().unwrap();
    let usage = |input, output| {
        Ok(ProviderEvent::Usage(Usage {
            input_tokens: input,
            output_tokens: output,
            cached_tokens: 0,
        }))
    };
    let provider = MockProvider::new(vec![
        Script::Reply(vec![
            Ok(ProviderEvent::ToolCall(harness_core::message::ToolCall {
                id: "c1".into(),
                name: "echo".into(),
                arguments: json!({"text": "x"}).to_string(),
            })),
            usage(100, 10),
            Ok(ProviderEvent::Finished(FinishReason::ToolCalls)),
        ]),
        Script::Reply(vec![
            Ok(ProviderEvent::TextDelta("done".into())),
            usage(150, 5),
            Ok(ProviderEvent::Finished(FinishReason::Stop)),
        ]),
    ]);
    let mut agent = agent(provider, Mode::Auto, Arc::new(NonInteractive), dir.path());
    let (_, events) = run(&mut agent, "go").await;
    let [
        AgentEvent::TurnStats {
            model,
            input_tokens,
            output_tokens,
            cached_tokens,
            time_to_first_token_ms,
            ..
        },
    ] = &stats(&events)[..]
    else {
        panic!("one TurnStats: {events:#?}");
    };
    assert_eq!(model, "mock/m1");
    assert_eq!(
        (*input_tokens, *output_tokens, *cached_tokens),
        (250, 15, 0)
    );
    assert!(time_to_first_token_ms.is_some());
}

#[tokio::test]
async fn a_turn_that_never_called_the_model_has_no_stats() {
    let dir = tempfile::tempdir().unwrap();
    let mut agent = agent(
        MockProvider::new(vec![Script::text("never asked")]),
        Mode::Auto,
        Arc::new(NonInteractive),
        dir.path(),
    );
    let cancel = tokio_util::sync::CancellationToken::new();
    cancel.cancel();
    let (_, events) = common::run_with(&mut agent, "stop", cancel).await;
    assert!(stats(&events).is_empty(), "{events:#?}");
}

#[tokio::test]
async fn context_usage_splits_the_next_request() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![Script::Reply(vec![
        Ok(ProviderEvent::TextDelta("hello".into())),
        Ok(ProviderEvent::Usage(Usage {
            input_tokens: 700,
            output_tokens: 2,
            cached_tokens: 0,
        })),
        Ok(ProviderEvent::Finished(FinishReason::Stop)),
    ])]);
    let mut agent = agent(provider, Mode::Auto, Arc::new(NonInteractive), dir.path());
    let before = agent.context_usage();
    assert_eq!(before.window, DEFAULT_CONTEXT_WINDOW);
    assert_eq!(before.system, 4); // "system prompt" is 13 bytes
    assert!(before.tools > 100, "{before:?}");
    assert_eq!(before.messages, 0);
    assert_eq!(before.total, before.system + before.tools);
    run(&mut agent, "x".repeat(400).as_str()).await;
    let after = agent.context_usage();
    assert!(after.messages >= 100, "{after:?}");
    // The provider reported 700 input tokens for the request; the reply is estimated.
    assert_eq!(after.total, 700 + 2 + 4);
}
