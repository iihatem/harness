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
                ..Default::default()
            }),
            3 => ProviderEvent::Finished(FinishReason::Stop),
            _ => return None,
        };
        Some((Ok(event), step + 1))
    })
}

/// A provider whose first reply is a tool call: its arguments stream for two seconds (as a wire
/// parser buffers them, only surfacing `OutputStarted` early and the whole `ToolCall` once the
/// stream ends), then a second reply answers in text once the tool result comes back.
struct SlowToolCall(std::sync::atomic::AtomicUsize);

impl Provider for SlowToolCall {
    fn stream(&self, _request: ChatRequest) -> ProviderStream {
        if self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            Box::pin(futures::stream::unfold(0, |step| async move {
                let event = match step {
                    0 => {
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        ProviderEvent::OutputStarted
                    }
                    1 => {
                        tokio::time::sleep(Duration::from_millis(2_000)).await;
                        ProviderEvent::ToolCall(harness_core::message::ToolCall {
                            id: "c1".into(),
                            name: "echo".into(),
                            arguments: json!({"text": "x"}).to_string(),
                        })
                    }
                    2 => ProviderEvent::Usage(Usage {
                        input_tokens: 10,
                        output_tokens: 88,
                        cached_tokens: 0,
                        ..Default::default()
                    }),
                    3 => ProviderEvent::Finished(FinishReason::ToolCalls),
                    _ => return None,
                };
                Some((Ok(event), step + 1))
            }))
        } else {
            Box::pin(futures::stream::iter(vec![
                Ok(ProviderEvent::TextDelta("done".into())),
                Ok(ProviderEvent::Usage(Usage {
                    input_tokens: 5,
                    output_tokens: 5,
                    cached_tokens: 0,
                    ..Default::default()
                })),
                Ok(ProviderEvent::Finished(FinishReason::Stop)),
            ]))
        }
    }
}

// Review C, Important 2: a tool call's arguments stream for a while before the call arrives
// whole, but the reply's actual first byte is `OutputStarted`, well before that. Time to first
// token, and the generation window used for tokens/second, must count from there, not from
// whichever event happens to carry the tool call.
#[tokio::test(start_paused = true)]
async fn turn_stats_count_a_tool_calls_first_byte_not_its_whole_arrival() {
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
        Arc::new(SlowToolCall(std::sync::atomic::AtomicUsize::new(0))),
        ToolRegistry::new(vec![Arc::new(Echo)]),
        policy,
        Arc::new(NonInteractive),
        AgentConfig::new("mock/m1", "m1", "system prompt", dir.path().join("out")),
        ToolContext::new(dir.path()),
    );
    let (reason, events) = run(&mut agent, "hi").await;
    assert_eq!(reason, TurnEndReason::Completed);
    let [stat] = &stats(&events)[..] else {
        panic!("one TurnStats: {events:#?}");
    };
    let AgentEvent::TurnStats {
        time_to_first_token_ms,
        generation_ms,
        output_tokens,
        ..
    } = stat
    else {
        unreachable!()
    };
    // The real first byte was at 200ms (`OutputStarted`), not at 2.2s when the whole tool call
    // arrived.
    assert_eq!(*time_to_first_token_ms, Some(200));
    // Generation ran from 200ms to 2.2s (2.0s): the buggy version measured only from 2.2s to
    // 2.2s, near enough zero, which inflated tokens/second.
    assert_eq!(*generation_ms, 2_000);
    assert_eq!(*output_tokens, 93);
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
            ..Default::default()
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

// Review C, minor 2: a server that reports usage cumulatively, in every chunk, must not be
// counted once per chunk: `/usage` and the status line's session totals summed every `Usage`
// event, and this reply's real 20 output tokens would have shown as 25 (5 + 20).
#[tokio::test]
async fn a_reply_with_usage_in_every_chunk_is_counted_once_with_the_last_chunks_numbers() {
    let dir = tempfile::tempdir().unwrap();
    let usage = |input, output| {
        Ok(ProviderEvent::Usage(Usage {
            input_tokens: input,
            output_tokens: output,
            cached_tokens: 0,
            ..Default::default()
        }))
    };
    let provider = MockProvider::new(vec![Script::Reply(vec![
        Ok(ProviderEvent::TextDelta("Hel".into())),
        usage(300, 5),
        Ok(ProviderEvent::TextDelta("lo".into())),
        usage(1_000, 20),
        Ok(ProviderEvent::Finished(FinishReason::Stop)),
    ])]);
    let mut agent = agent(provider, Mode::Auto, Arc::new(NonInteractive), dir.path());
    let (_, events) = run(&mut agent, "go").await;
    let usages: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Usage { usage, .. } => Some(*usage),
            _ => None,
        })
        .collect();
    assert_eq!(
        usages,
        [Usage {
            input_tokens: 1_000,
            output_tokens: 20,
            cached_tokens: 0,
            ..Default::default()
        }],
        "{events:#?}"
    );
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
            ..Default::default()
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
