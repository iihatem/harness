mod common;

use std::sync::Arc;

use common::*;
use harness_core::agent::{Agent, NonInteractive};
use harness_core::compaction::{SUMMARY_PREFIX, SUMMARY_SYSTEM, request_tokens};
use harness_core::event::{AgentEvent, TurnEndReason};
use harness_core::message::{Message, RequestOptions, Usage};
use harness_core::permission::Mode;
use harness_core::provider::{FinishReason, ProviderError, ProviderEvent};
use harness_core::session::{RewindScope, Session};
use harness_core::testing::{MockProvider, Script};
use harness_core::turn::{TurnInput, TurnModel};
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
                ..Default::default()
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

// Review C, minor 3: `Agent::summarize` dropped the summary call's `Usage` event entirely
// (`_ => {}`), so compaction, often the largest request in a long session, never counted towards
// `/usage` or the status line's totals.
#[tokio::test]
async fn compaction_reports_the_summary_calls_usage() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::text("hi"),
        Script::Reply(vec![
            Ok(ProviderEvent::TextDelta("the summary".into())),
            Ok(ProviderEvent::Usage(Usage {
                input_tokens: 5_000,
                output_tokens: 40,
                cached_tokens: 0,
                ..Default::default()
            })),
            Ok(ProviderEvent::Finished(FinishReason::Stop)),
        ]),
    ]);
    let mut agent = agent(
        provider.clone(),
        Mode::Auto,
        Arc::new(NonInteractive),
        dir.path(),
    );
    run(&mut agent, "hello").await;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    agent
        .compact(None, &tx, CancellationToken::new())
        .await
        .unwrap();
    drop(tx);
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }
    let usage = events.iter().find_map(|e| match e {
        AgentEvent::Usage { model, usage } => Some((model.clone(), *usage)),
        _ => None,
    });
    assert_eq!(
        usage,
        Some((
            "mock/m1".into(),
            Usage {
                input_tokens: 5_000,
                output_tokens: 40,
                cached_tokens: 0,
                ..Default::default()
            }
        )),
        "{events:#?}"
    );
}

fn echo(id: &str, text: &str) -> Script {
    Script::tool_call(id, "echo", serde_json::json!({ "text": text }))
}

/// Whether `request` asks to summarize nothing but an earlier summary.
fn summarizes_only_a_summary(request: &harness_core::message::ChatRequest) -> bool {
    let text = first_user(&request.messages);
    let conversation = text.split("<conversation>\n").nth(1).unwrap_or_default();
    conversation.starts_with(&format!("User: {SUMMARY_PREFIX}"))
        && conversation.matches("\n\n").count() == 1
}

// Review F I1: in a long turn whose last step alone is over the keep budget, compaction must
// still make the next request fit, and must never summarize just the earlier summary.
#[tokio::test]
async fn a_long_turn_is_compacted_to_its_last_step_and_the_next_request_fits() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::text("ok"),
        echo("c1", &"a".repeat(4_000)),
        echo("c2", &"b".repeat(4_000)),
        Script::text("first summary"),
        echo("c3", &"c".repeat(4_000)),
        Script::text("second summary"),
        Script::text("done"),
    ]);
    let mut agent = agent(
        provider.clone(),
        Mode::Auto,
        Arc::new(NonInteractive),
        dir.path(),
    );
    let window = 4_000;
    agent.config_mut().context_window = window;
    run(&mut agent, "first").await;
    let (reason, events) = run(&mut agent, "do the work").await;
    assert_eq!(reason, TurnEndReason::Completed, "{events:?}");
    let done = compacted(&events);
    assert_eq!(done.len(), 2, "{events:?}");
    for (_, before, after) in &done {
        assert!(after < before, "{done:?}");
    }
    let requests = provider.requests();
    assert!(!requests.iter().any(summarizes_only_a_summary));
    for (i, request) in requests.iter().enumerate() {
        if i > 0 && is_summary_request(&requests[i - 1]) {
            let tokens = request_tokens(&request.system, &request.tools, &request.messages);
            assert!(tokens < window, "request {i} has {tokens} tokens");
        }
    }
    // The step that did not fit the budget is kept as it was, with its result.
    let last = &requests.last().unwrap().messages;
    assert!(first_user(last).starts_with(SUMMARY_PREFIX));
    assert!(matches!(&last[1], Message::Assistant { tool_calls, .. } if tool_calls[0].id == "c3"));
    assert!(matches!(&last[2], Message::Tool { call_id, .. } if call_id == "c3"));
}

// Review F I1: a compaction that could not bring the estimate below the threshold is not
// repeated at every step; automatic compaction waits until the estimate drops below it.
#[tokio::test]
async fn automatic_compaction_waits_until_the_estimate_drops_below_the_threshold() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::text("ok"),
        // One step alone over the threshold.
        echo("c1", &"z".repeat(14_000)),
        Script::text("summary"),
        echo("c2", "small"),
        echo("c3", "small"),
        Script::text("done"),
    ]);
    let mut agent = agent(
        provider.clone(),
        Mode::Auto,
        Arc::new(NonInteractive),
        dir.path(),
    );
    agent.config_mut().context_window = 4_000;
    run(&mut agent, "first").await;
    let (reason, events) = run(&mut agent, "go").await;
    assert_eq!(reason, TurnEndReason::Completed, "{events:?}");
    assert_eq!(compacted(&events).len(), 1, "{events:?}");
    assert_eq!(
        provider
            .requests()
            .iter()
            .filter(|r| is_summary_request(r))
            .count(),
        1
    );
}

// Review F minor 2: with nothing to compact yet (one large message), automatic compaction skips
// quietly, and still happens once there is something to compact.
#[tokio::test]
async fn nothing_to_compact_yet_is_skipped_quietly() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        echo("c1", "small"),
        Script::text("summary"),
        Script::text("done"),
    ]);
    let mut agent = agent(
        provider.clone(),
        Mode::Auto,
        Arc::new(NonInteractive),
        dir.path(),
    );
    agent.config_mut().context_window = 4_000;
    let (reason, events) = run(&mut agent, &"x".repeat(14_000)).await;
    assert_eq!(reason, TurnEndReason::Completed, "{events:?}");
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::Warning { .. })),
        "{events:?}"
    );
    assert_eq!(compacted(&events).len(), 1, "{events:?}");
}

// Review F minor 1: Ctrl-C while the overflow compaction runs is an interruption (exit 130),
// not the provider's overflow error (exit 1).
#[tokio::test]
async fn ctrl_c_during_an_overflow_compaction_interrupts_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let (mut agent, _provider) =
        after_a_long_turn(dir.path(), vec![overflow(), Script::Hang(vec![])]).await;
    let cancel = CancellationToken::new();
    let ctrl_c = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        ctrl_c.cancel();
    });
    let (reason, events) = run_with(&mut agent, "next", cancel).await;
    assert_eq!(reason, TurnEndReason::Interrupted, "{events:?}");
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::Error { .. } | AgentEvent::Warning { .. })),
        "{events:?}"
    );
}

/// An agent after three turns of 1,500-character messages, with a 2,000-token window.
async fn after_three_turns(
    dir: &std::path::Path,
    script: Vec<Script>,
) -> (Agent, Arc<MockProvider>) {
    let mut all = vec![Script::text("ok"), Script::text("ok"), Script::text("ok")];
    all.extend(script);
    let provider = MockProvider::new(all);
    let mut agent = agent(provider.clone(), Mode::Auto, Arc::new(NonInteractive), dir);
    for letter in ["a", "b", "c"] {
        run(&mut agent, &letter.repeat(1_500)).await;
    }
    agent.config_mut().context_window = 2_000;
    (agent, provider)
}

async fn compact_now(agent: &mut Agent) -> (Result<(), String>, Vec<AgentEvent>) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let result = agent.compact(None, &tx, CancellationToken::new()).await;
    drop(tx);
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }
    (result, events)
}

// Review F minor 6: a summary cut off at the model's output limit lost its end, which says what
// remains to be done.
#[tokio::test]
async fn a_summary_cut_off_at_the_output_limit_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let (mut agent, _provider) = after_three_turns(
        dir.path(),
        vec![Script::Reply(vec![
            Ok(ProviderEvent::TextDelta("The user asked for".into())),
            Ok(ProviderEvent::Finished(FinishReason::Length)),
        ])],
    )
    .await;
    let (result, events) = compact_now(&mut agent).await;
    let error = result.unwrap_err();
    assert!(error.contains("output limit"), "{error}");
    assert!(compacted(&events).is_empty());
    assert_eq!(agent.history().len(), 6);
}

// Review F minor 7: the summary request is sized from an estimate, so it can itself be too long
// for the model: it is tried once more with half the transcript.
#[tokio::test]
async fn a_summary_request_that_overflows_is_retried_once_with_half_the_transcript() {
    let dir = tempfile::tempdir().unwrap();
    let (mut agent, provider) =
        after_three_turns(dir.path(), vec![overflow(), Script::text("summary")]).await;
    let (result, events) = compact_now(&mut agent).await;
    result.unwrap();
    assert_eq!(compacted(&events).len(), 1);
    let summaries: Vec<String> = provider
        .requests()
        .iter()
        .filter(|r| is_summary_request(r))
        .map(|r| first_user(&r.messages))
        .collect();
    assert_eq!(summaries.len(), 2);
    assert!(summaries[1].len() < summaries[0].len());
    assert!(summaries[1].contains("earlier messages left out"));

    // Only once.
    let dir = tempfile::tempdir().unwrap();
    let (mut agent, provider) = after_three_turns(
        dir.path(),
        vec![overflow(), overflow(), Script::text("never")],
    )
    .await;
    let (result, _) = compact_now(&mut agent).await;
    assert!(result.unwrap_err().contains("maximum context length"));
    assert_eq!(
        provider
            .requests()
            .iter()
            .filter(|r| is_summary_request(r))
            .count(),
        2
    );
}

// Review F I2: compacting again must not cut the end off the earlier summary, where it says
// what remains to be done.
#[tokio::test]
async fn compacting_again_keeps_the_end_of_the_earlier_summary() {
    let dir = tempfile::tempdir().unwrap();
    let long_summary = format!("{} TAIL-REMAINING-WORK", "s".repeat(6_000));
    let provider = MockProvider::new(vec![
        Script::text("noted"),
        Script::text(&long_summary),
        Script::text("answer"),
        Script::text("second summary"),
        Script::text("second answer"),
    ]);
    let mut agent = agent(
        provider.clone(),
        Mode::Auto,
        Arc::new(NonInteractive),
        dir.path(),
    );
    // Room for the 6,000-character summary in a summary request (half the window).
    agent.config_mut().context_window = 4_000;
    run(&mut agent, &"x".repeat(12_000)).await;
    let (_, events) = run(&mut agent, "short question").await;
    assert_eq!(compacted(&events).len(), 1, "{events:?}");
    let (_, events) = run(&mut agent, &"q".repeat(6_000)).await;
    assert_eq!(compacted(&events).len(), 1, "{events:?}");
    let summaries: Vec<_> = provider
        .requests()
        .into_iter()
        .filter(is_summary_request)
        .collect();
    assert_eq!(summaries.len(), 2);
    assert!(first_user(&summaries[1].messages).contains("TAIL-REMAINING-WORK"));
}

// Review A I3: the session's model writes the summary under its profile's request options, so a
// model whose output the profile limits can still be compacted.
#[tokio::test]
async fn the_summary_request_carries_the_configured_request_options() {
    let dir = tempfile::tempdir().unwrap();
    let (mut agent, provider) =
        after_three_turns(dir.path(), vec![Script::text("the summary")]).await;
    let options = RequestOptions {
        max_output_tokens: Some(1000),
        temperature: Some(0.3),
        reasoning_effort: Some("low".into()),
        ..RequestOptions::default()
    };
    agent.config_mut().request = options.clone();
    let (result, _) = compact_now(&mut agent).await;
    result.unwrap();
    let request = provider.requests().pop().unwrap();
    assert!(is_summary_request(&request));
    assert_eq!(request.options, options);
    // Review A M5: and within the room the window leaves.
    let input = request_tokens(&request.system, &request.tools, &request.messages);
    assert_eq!(request.output_room, Some(2_000 - input));
}

// Spec "Compaction on the background role": a slash command's model answers its turn, but the
// summary is written by the model of the `background` role, which is `main` when unset: no other
// model sees the older conversation for it.
#[tokio::test]
async fn a_turn_models_turn_does_not_write_the_summary() {
    let dir = tempfile::tempdir().unwrap();
    let (mut agent, session_model) =
        after_three_turns(dir.path(), vec![Script::text("summary")]).await;
    agent.config_mut().request = RequestOptions {
        max_output_tokens: Some(1000),
        ..RequestOptions::default()
    };
    let command_model = MockProvider::new(vec![Script::text("answer")]);
    let input = TurnInput {
        model: Some(TurnModel {
            provider: command_model.clone(),
            id: "mock/m2".into(),
            name: "m2".into(),
            local: false,
            tools: None,
            edit_section: None,
            context_window: None,
            request: None,
            text_tool_calls: false,
        }),
        ..TurnInput::from("d".repeat(1_500))
    };
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let reason = agent.run_turn(input, &tx, CancellationToken::new()).await;
    drop(tx);
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }
    assert_eq!(reason, TurnEndReason::Completed);
    assert_eq!(compacted(&events).len(), 1, "{events:?}");
    let requests = command_model.requests();
    assert_eq!(requests.len(), 1, "only the turn itself");
    assert!(!is_summary_request(&requests[0]));
    assert_eq!(requests[0].options, RequestOptions::default());
    // Main wrote the summary, with its own options.
    let main = session_model.requests();
    assert_eq!(main.len(), 4);
    assert!(is_summary_request(&main[3]));
    assert_eq!(main[3].options.max_output_tokens, Some(1000));
}

// Review F I3: output tokens, reasoning included, are not sent back to the model, so only the
// reported input counts; the reply itself is estimated like any other message.
#[tokio::test]
async fn reported_output_tokens_do_not_count_toward_the_threshold() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::Reply(vec![
            Ok(ProviderEvent::ReasoningDelta("thinking".into())),
            Ok(ProviderEvent::TextDelta("short".into())),
            Ok(ProviderEvent::Usage(Usage {
                input_tokens: 1_000,
                output_tokens: 25_500,
                cached_tokens: 0,
                ..Default::default()
            })),
            Ok(ProviderEvent::Finished(FinishReason::Stop)),
        ]),
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
    assert!(compacted(&events).is_empty(), "{events:?}");
    assert!(!provider.requests().iter().any(is_summary_request));
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
    // An error the provider reports inside the stream.
    assert!(
        ProviderError::InStream(r#"{"code":"context_length_exceeded"}"#.into())
            .is_context_overflow()
    );
}

// Review F minor 3: other providers' wording.
#[test]
fn other_providers_overflow_errors_are_recognised() {
    let http = |status: u16, body: &str| ProviderError::Http {
        status,
        body: body.into(),
        retry_after: None,
    };
    for (status, body) in [
        // xAI
        (
            400,
            "This model's maximum prompt length is 131072 but the request contains 140000 tokens.",
        ),
        // Gemini
        (
            400,
            "The input token count (1234567) exceeds the maximum number of tokens allowed (1048576).",
        ),
        // TGI, Together
        (
            422,
            "Input validation error: `inputs` tokens + `max_new_tokens` must be <= 4096. Given: 4000 `inputs` tokens and 200 `max_new_tokens`",
        ),
        // Bedrock
        (400, "Input is too long for requested model."),
        // Anthropic
        (
            400,
            "input length and `max_tokens` exceed context limit: 188240 + 21333 > 200000, decrease input length or `max_tokens` and try again",
        ),
    ] {
        assert!(http(status, body).is_context_overflow(), "{body}");
    }
}

// Review F minor 4: a response harness could not parse quotes the raw chunk, which can hold the
// model's own text; only error responses and errors the provider reports count.
#[test]
fn a_parse_error_quoting_model_text_is_not_an_overflow() {
    let chunk = r#"{"choices":[{"delta":{"content":"Your context window is 8k"}}]"#;
    assert!(
        !ProviderError::Protocol(format!("EOF while parsing an object in chunk: {chunk}"))
            .is_context_overflow()
    );
}

// Final review, I-1: the summary loop retries as a turn does: a local server's first-data
// timeout not at all, a hosted provider's once.
#[tokio::test(start_paused = true)]
async fn a_summary_request_retries_first_data_timeouts_as_a_turn_does() {
    let no_start = |local| {
        Script::error(ProviderError::NoStart {
            message: "the server did not start its reply".into(),
            local,
        })
    };
    let summaries = |provider: &MockProvider| {
        provider
            .requests()
            .iter()
            .filter(|r| is_summary_request(r))
            .count()
    };
    let dir = tempfile::tempdir().unwrap();
    let (mut agent, provider) =
        after_three_turns(dir.path(), vec![no_start(true), Script::text("summary")]).await;
    let (result, events) = compact_now(&mut agent).await;
    let error = result.unwrap_err();
    assert!(error.contains("local server"), "{error}");
    assert!(compacted(&events).is_empty());
    assert_eq!(summaries(&provider), 1);

    let dir = tempfile::tempdir().unwrap();
    let (mut agent, provider) = after_three_turns(
        dir.path(),
        vec![no_start(false), no_start(false), Script::text("summary")],
    )
    .await;
    let (result, _) = compact_now(&mut agent).await;
    assert!(result.is_err());
    assert_eq!(summaries(&provider), 2);

    let dir = tempfile::tempdir().unwrap();
    let (mut agent, provider) =
        after_three_turns(dir.path(), vec![no_start(false), Script::text("summary")]).await;
    let (result, events) = compact_now(&mut agent).await;
    result.unwrap();
    assert_eq!(compacted(&events).len(), 1);
    assert_eq!(summaries(&provider), 2);
}

// Final review, I-1: a slash command's model on a local server gets a local server's wait for
// its turn; the summary is main's to write (see above), and carries main's options.
#[tokio::test]
async fn a_local_turn_models_requests_say_it_is_local() {
    let dir = tempfile::tempdir().unwrap();
    let (mut agent, main) = after_three_turns(dir.path(), vec![Script::text("summary")]).await;
    let command_model = MockProvider::new(vec![Script::text("answer")]);
    let input = TurnInput {
        model: Some(TurnModel {
            provider: command_model.clone(),
            id: "ollama/m2".into(),
            name: "m2".into(),
            local: true,
            tools: None,
            edit_section: None,
            context_window: None,
            request: None,
            text_tool_calls: false,
        }),
        ..TurnInput::from("d".repeat(1_500))
    };
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let reason = agent.run_turn(input, &tx, CancellationToken::new()).await;
    assert_eq!(reason, TurnEndReason::Completed);
    let requests = command_model.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].options,
        RequestOptions {
            local: true,
            ..RequestOptions::default()
        }
    );
    let summary = main.requests().pop().unwrap();
    assert!(is_summary_request(&summary));
    assert!(!summary.options.local);
}
