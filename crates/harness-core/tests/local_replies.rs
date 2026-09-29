//! What local models do that hosted ones rarely do: write tool calls as text, and run out of
//! output tokens in the middle of a reply.

mod common;

use std::sync::Arc;

use common::*;
use harness_core::agent::NonInteractive;
use harness_core::event::{AgentEvent, TurnEndReason};
use harness_core::message::{Message, ToolCall};
use harness_core::permission::Mode;
use harness_core::provider::{FinishReason, ProviderEvent};
use harness_core::testing::{MockProvider, Script};
use harness_core::textcalls::recover;
use serde_json::json;

fn known(name: &str) -> bool {
    matches!(name, "read" | "echo")
}

fn call(name: &str, arguments: &str) -> ToolCall {
    ToolCall {
        id: String::new(),
        name: name.into(),
        arguments: arguments.into(),
    }
}

#[test]
fn tagged_calls_are_recovered_whole() {
    assert_eq!(
        recover(
            r#"<tool_call>{"name":"read","arguments":{"path":"src/lib.rs"}}</tool_call>"#,
            known
        ),
        Some(vec![call("read", r#"{"path":"src/lib.rs"}"#)])
    );
    let two = "\n<tool_call>\n{\"name\": \"echo\", \"arguments\": {\"text\": \"a\"}}\n</tool_call>\n\n<tool_call>{\"name\": \"echo\", \"arguments\": {\"text\": \"b\"}}</tool_call>\n";
    assert_eq!(
        recover(two, known),
        Some(vec![
            call("echo", r#"{"text":"a"}"#),
            call("echo", r#"{"text":"b"}"#)
        ])
    );
}

#[test]
fn a_message_that_is_only_a_call_object_is_recovered() {
    assert_eq!(
        recover(r#" {"name": "read", "arguments": {"path": "a"}} "#, known),
        Some(vec![call("read", r#"{"path":"a"}"#)])
    );
    // Llama 3.1 writes `parameters`.
    assert_eq!(
        recover(r#"{"name": "read", "parameters": {"path": "a"}}"#, known),
        Some(vec![call("read", r#"{"path":"a"}"#)])
    );
    // Arguments already written as JSON text are kept as they are.
    assert_eq!(
        recover(
            r#"{"name": "read", "arguments": "{\"path\": \"a\"}"}"#,
            known
        ),
        Some(vec![call("read", r#"{"path": "a"}"#)])
    );
}

// Spec: "Example code in prose". Review Focus: a reply that explains or quotes the format is not
// a call.
#[test]
fn text_around_a_call_or_a_quoted_call_is_not_recovered() {
    for text in [
        r#"To read a file, send {"name": "read", "arguments": {"path": "a"}} and wait."#,
        "Here is the call:\n<tool_call>{\"name\":\"read\",\"arguments\":{\"path\":\"a\"}}</tool_call>",
        "<tool_call>{\"name\":\"read\",\"arguments\":{\"path\":\"a\"}}</tool_call>\nDone.",
        "```json\n{\"name\": \"read\", \"arguments\": {\"path\": \"a\"}}\n```",
        "<tool_call>{\"name\":\"read\",\"arguments\":{\"path\":\"a\"}}",
        r#"{"name": "read"}"#,
        r#"{"name": "deploy", "arguments": {}}"#,
        r#"[{"name": "read", "arguments": {"path": "a"}}]"#,
        "",
    ] {
        assert_eq!(recover(text, known), None, "{text:?}");
    }
}

fn local_agent(provider: Arc<MockProvider>, dir: &std::path::Path) -> harness_core::agent::Agent {
    let mut agent = agent(provider, Mode::Auto, Arc::new(NonInteractive), dir);
    agent.config_mut().text_tool_calls = true;
    agent
}

// Spec: "Local model emits a tagged tool call as text".
#[tokio::test]
async fn a_tagged_call_in_a_reply_runs_like_a_native_one() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::text(r#"<tool_call>{"name":"echo","arguments":{"text":"ran"}}</tool_call>"#),
        Script::text("done"),
    ]);
    let mut agent = local_agent(provider.clone(), dir.path());
    let (reason, events) = run(&mut agent, "go").await;
    assert_eq!(reason, TurnEndReason::Completed);
    assert_eq!(finished_outputs(&events), [("ran".to_string(), false)]);
    // The call is kept as a call, so the model sees it in its native form next time.
    match &agent.history()[1] {
        Message::Assistant {
            content,
            tool_calls,
            ..
        } => {
            assert!(content.is_empty(), "{content}");
            assert_eq!(tool_calls[0].name, "echo");
            assert!(!tool_calls[0].id.is_empty());
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        provider.requests()[1].messages.last(),
        Some(Message::Tool { .. })
    ));
}

#[tokio::test]
async fn text_calls_are_left_alone_unless_the_profile_turns_them_on() {
    let dir = tempfile::tempdir().unwrap();
    let text = r#"<tool_call>{"name":"echo","arguments":{"text":"ran"}}</tool_call>"#;
    let provider = MockProvider::new(vec![Script::text(text)]);
    let mut agent = agent(
        provider.clone(),
        Mode::Auto,
        Arc::new(NonInteractive),
        dir.path(),
    );
    let (reason, events) = run(&mut agent, "go").await;
    assert_eq!(reason, TurnEndReason::Completed);
    assert!(finished_outputs(&events).is_empty());
}

#[tokio::test]
async fn a_recovered_call_is_validated_like_any_other() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::text(r#"<tool_call>{"name":"echo","arguments":{"txt":"x"}}</tool_call>"#),
        Script::text("sorry"),
    ]);
    let mut agent = local_agent(provider, dir.path());
    let (_, events) = run(&mut agent, "go").await;
    let (output, is_error) = &finished_outputs(&events)[0];
    assert!(
        *is_error && output.contains("invalid arguments"),
        "{output}"
    );
    assert_eq!(agent.invalid_calls_this_turn(), 1);
}

// Spec: "Write call cut off".
#[tokio::test]
async fn a_call_cut_off_by_the_output_limit_is_not_run() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::Reply(vec![
            Ok(ProviderEvent::ToolCall(ToolCall {
                id: "c1".into(),
                name: "touch".into(),
                arguments: json!({"path": "cut.txt"}).to_string(),
            })),
            Ok(ProviderEvent::Finished(FinishReason::Length)),
        ]),
        Script::text("I will write it in parts."),
    ]);
    let mut agent = local_agent(provider.clone(), dir.path());
    let (reason, events) = run(&mut agent, "go").await;
    assert_eq!(reason, TurnEndReason::Completed);
    assert!(!dir.path().join("cut.txt").exists());
    let requests = provider.requests();
    match requests[1].messages.last() {
        Some(Message::Tool {
            call_id,
            content,
            is_error,
        }) => {
            assert_eq!(call_id, "c1");
            assert!(*is_error);
            assert!(content.contains("cut off"), "{content}");
            assert!(content.contains("smaller steps"), "{content}");
        }
        other => panic!("{other:?}"),
    }
    assert!(events.iter().any(|e| matches!(e,
        AgentEvent::Warning { message } if message.contains("cut off"))));
}

// Spec: "Answer cut off".
#[tokio::test]
async fn an_answer_cut_off_is_kept_and_continued() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::Reply(vec![
            Ok(ProviderEvent::TextDelta("The first half".into())),
            Ok(ProviderEvent::Finished(FinishReason::Length)),
        ]),
        Script::text(" and the second half."),
    ]);
    let mut agent = local_agent(provider.clone(), dir.path());
    let (reason, _) = run(&mut agent, "go").await;
    assert_eq!(reason, TurnEndReason::Completed);
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    match requests[1].messages.last() {
        Some(Message::User { content }) => {
            assert!(content.starts_with("[harness]"), "{content}");
            assert!(content.contains("cut off"), "{content}");
        }
        other => panic!("{other:?}"),
    }
    let answers: Vec<&str> = agent
        .history()
        .iter()
        .filter_map(|m| match m {
            Message::Assistant { content, .. } => Some(content.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(answers, ["The first half", " and the second half."]);
}

// A cut-off text call is not recovered: its end is missing.
#[tokio::test]
async fn a_text_call_cut_off_is_not_recovered() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::Reply(vec![
            Ok(ProviderEvent::TextDelta(
                r#"<tool_call>{"name":"echo","arguments":{"text":"x"}}</tool_call>"#.into(),
            )),
            Ok(ProviderEvent::Finished(FinishReason::Length)),
        ]),
        Script::text("ok"),
    ]);
    let mut agent = local_agent(provider, dir.path());
    let (_, events) = run(&mut agent, "go").await;
    assert!(finished_outputs(&events).is_empty());
}
