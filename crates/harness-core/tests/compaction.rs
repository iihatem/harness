mod common;

use std::sync::Arc;

use common::*;
use harness_core::agent::{Agent, NonInteractive};
use harness_core::compaction::{SUMMARY_PREFIX, SUMMARY_SYSTEM, request_tokens};
use harness_core::event::{AgentEvent, TurnEndReason};
use harness_core::message::{Message, Usage};
use harness_core::permission::Mode;
use harness_core::provider::{FinishReason, ProviderError, ProviderEvent};
use harness_core::session::{RewindScope, Session};
use harness_core::testing::{MockProvider, Script};
use tokio_util::sync::CancellationToken;

fn overflow() -> Script {
    Script::error(ProviderError::Http {
        status: 400,
        body: r#"{"error":{"message":"This model's maximum context length is 8192 tokens","code":"context_length_exceeded"}}"#.into(),
        retry_after: None,
    })
}

fn is_summary_request(request: &harness_core::message::ChatRequest) -> bool {
    request.system == SUMMARY_SYSTEM && request.tools.is_empty()
}

fn compacted(events: &[AgentEvent]) -> Vec<(String, u64, u64)> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Compacted {
                summary,
                tokens_before,
                tokens_after,
            } => Some((summary.clone(), *tokens_before, *tokens_after)),
            _ => None,
        })
        .collect()
}

fn first_user(messages: &[Message]) -> String {
    match &messages[0] {
        Message::User { content } => content.clone(),
        other => panic!("{other:?}"),
    }
}

/// An agent whose session is saved in `dir`, after one turn with a 2,000-token message.
async fn after_a_long_turn(
    dir: &std::path::Path,
    script: Vec<Script>,
) -> (Agent, Arc<MockProvider>) {
    let mut script = script;
    script.insert(0, Script::text("noted"));
    let provider = MockProvider::new(script);
    let mut agent = agent(provider.clone(), Mode::Auto, Arc::new(NonInteractive), dir)
        .with_session(Session::create(&dir.join("sessions"), dir));
    run(&mut agent, &"x".repeat(8_000)).await;
    (agent, provider)
}

// Spec: automatic compaction.
#[tokio::test]
async fn compaction_happens_before_the_model_call_that_would_reach_the_threshold() {
    let dir = tempfile::tempdir().unwrap();
    let (mut agent, provider) = after_a_long_turn(
        dir.path(),
        vec![
            Script::text("They pasted 8,000 x's."),
            Script::text("answer"),
        ],
    )
    .await;
    agent.config_mut().context_window = 2_500;
    let (reason, events) = run(&mut agent, "short question").await;
    assert_eq!(reason, TurnEndReason::Completed);
    let done = compacted(&events);
    assert_eq!(done.len(), 1, "{events:?}");
    assert_eq!(done[0].0, "They pasted 8,000 x's.");
    assert!(done[0].1 >= 2_000 && done[0].2 < 500, "{done:?}");
    let compaction = events
        .iter()
        .position(|e| matches!(e, AgentEvent::Compacted { .. }))
        .unwrap();
    let answer = events
        .iter()
        .position(
            |e| matches!(e, AgentEvent::AssistantMessage { content, .. } if content == "answer"),
        )
        .unwrap();
    assert!(compaction < answer);

    let requests = provider.requests();
    assert!(is_summary_request(&requests[1]));
    assert!(first_user(&requests[1].messages).contains("xxxx"));
    let next = &requests[2];
    assert!(first_user(&next.messages).starts_with(SUMMARY_PREFIX));
    assert!(request_tokens(&next.system, &next.tools, &next.messages) < 2_500);
    // The recent messages are kept as they were.
    assert_eq!(
        next.messages.last(),
        Some(&Message::User {
            content: "short question".into()
        })
    );
}

// Spec: the originals stay in the file, so the user can rewind to before the compaction.
#[tokio::test]
async fn the_summarized_messages_stay_in_the_session_and_can_be_rewound_to() {
    let dir = tempfile::tempdir().unwrap();
    let (mut agent, _provider) = after_a_long_turn(
        dir.path(),
        vec![Script::text("summary"), Script::text("answer")],
    )
    .await;
    agent.config_mut().context_window = 2_500;
    run(&mut agent, "short question").await;
    let saved = std::fs::read_to_string(agent.session().path().unwrap()).unwrap();
    assert!(saved.contains(r#""type":"compaction""#) && saved.contains("xxxxxxxx"));
    let points: Vec<String> = agent.rewind_points().into_iter().map(|p| p.text).collect();
    assert_eq!(points.len(), 2);
    assert_eq!(points[1], "short question");
    let target = agent.rewind_points()[1].entry.clone();
    agent
        .rewind(&target, RewindScope::Conversation)
        .await
        .unwrap();
    assert_eq!(agent.history().len(), 2);
    assert!(first_user(agent.history()).starts_with("xxxx"));
}

#[tokio::test]
async fn compact_on_demand_passes_the_focus_and_summarizes_everything() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![Script::text("hi"), Script::text("the summary")]);
    let mut agent = agent(
        provider.clone(),
        Mode::Auto,
        Arc::new(NonInteractive),
        dir.path(),
    );
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    assert_eq!(
        agent
            .compact(None, &tx, CancellationToken::new())
            .await
            .unwrap_err(),
        "there is nothing to compact yet"
    );
    run(&mut agent, "hello").await;
    agent
        .compact(Some("the database schema"), &tx, CancellationToken::new())
        .await
        .unwrap();
    drop(tx);
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }
    assert_eq!(compacted(&events)[0].0, "the summary");
    let request = provider.requests().pop().unwrap();
    assert!(first_user(&request.messages).contains("Focus especially on: the database schema"));
    assert_eq!(agent.history().len(), 1);
}

// Spec: provider reports context overflow.
#[tokio::test]
async fn a_context_overflow_compacts_and_retries_once() {
    let dir = tempfile::tempdir().unwrap();
    let (mut agent, provider) = after_a_long_turn(
        dir.path(),
        vec![overflow(), Script::text("summary"), Script::text("answer")],
    )
    .await;
    let (reason, events) = run(&mut agent, "next").await;
    assert_eq!(reason, TurnEndReason::Completed, "{events:?}");
    assert_eq!(compacted(&events).len(), 1);
    let requests = provider.requests();
    assert_eq!(requests.len(), 4);
    assert!(is_summary_request(&requests[2]));
    assert!(first_user(&requests[3].messages).starts_with(SUMMARY_PREFIX));
}

#[tokio::test]
async fn a_second_overflow_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let (mut agent, _provider) = after_a_long_turn(
        dir.path(),
        vec![overflow(), Script::text("summary"), overflow()],
    )
    .await;
    let (reason, events) = run(&mut agent, "next").await;
    assert_eq!(reason, TurnEndReason::Error);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Error { message, .. } if message.contains("maximum context length")))
    );
    assert_eq!(compacted(&events).len(), 1);
}

#[tokio::test]
async fn reported_usage_counts_toward_the_threshold() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::Reply(vec![
            Ok(ProviderEvent::TextDelta("short".into())),
            Ok(ProviderEvent::Usage(Usage {
                input_tokens: 3_900,
                output_tokens: 10,
                cached_tokens: 0,
            })),
            Ok(ProviderEvent::Finished(FinishReason::Stop)),
        ]),
        Script::text("summary"),
        Script::text("answer"),
    ]);
    let mut agent = agent(
        provider.clone(),
        Mode::Auto,
        Arc::new(NonInteractive),
        dir.path(),
    );
    agent.config_mut().context_window = 4_000;
    run(&mut agent, "hi").await;
    let (_, events) = run(&mut agent, "again").await;
    assert_eq!(compacted(&events).len(), 1, "{events:?}");
}

#[test]
fn overflow_errors_are_recognised_by_their_wording() {
    let http = |status: u16, body: &str| ProviderError::Http {
        status,
        body: body.into(),
        retry_after: None,
    };
    for body in [
        r#"{"error":{"code":"context_length_exceeded"}}"#,
        "the request exceeds the available context size, try increasing it",
        "prompt is too long: 210000 tokens > 200000 maximum",
        "Trying to keep the first 9000 tokens when context length is 8192",
    ] {
        assert!(http(400, body).is_context_overflow(), "{body}");
    }
    assert!(!http(400, "invalid tool schema").is_context_overflow());
    assert!(!http(500, "context length").is_context_overflow());
    assert!(
        ProviderError::Protocol("provider error: context_length_exceeded".into())
            .is_context_overflow()
    );
}
