mod common;

use std::sync::Arc;

use common::*;
use harness_core::agent::NonInteractive;
use harness_core::event::AgentEvent;
use harness_core::message::{Message, ToolCall};
use harness_core::permission::Mode;
use harness_core::session::{self, EntryKind, Session};
use harness_core::testing::{MockProvider, Script};
use harness_core::turn::TurnInput;
use serde_json::json;

fn lines(path: &std::path::Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

// Spec: entries survive a crash. Each message is in the file as soon as it is complete.
#[tokio::test]
async fn every_message_is_saved_as_soon_as_it_is_complete() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call("c1", "echo", json!({"text": "x"})),
        Script::text("first answer"),
        Script::text("second answer"),
    ]);
    let session = Session::create(&dir.path().join("sessions"), dir.path());
    let path = session.path().unwrap().to_path_buf();
    let mut agent =
        agent(provider, Mode::Auto, Arc::new(NonInteractive), dir.path()).with_session(session);
    run(&mut agent, "first").await;
    run(&mut agent, "second").await;
    let saved = lines(&path);
    let kinds: Vec<&str> = saved
        .iter()
        .map(|l| l["message"]["role"].as_str().unwrap_or("header"))
        .collect();
    assert_eq!(
        kinds,
        [
            "header",
            "user",
            "assistant",
            "tool",
            "assistant",
            "user",
            "assistant"
        ]
    );
    assert_eq!(saved[4]["message"]["model"], "mock/m1");
    // Each entry's parent is the one before it.
    for pair in saved.windows(2) {
        assert_eq!(pair[1]["parent_id"], pair[0]["id"]);
    }
    std::mem::forget(agent);
}

#[tokio::test]
async fn a_resumed_session_continues_its_conversation() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let first = MockProvider::new(vec![Script::text("the answer is 4")]);
    let session = Session::create(&sessions, dir.path());
    let path = session.path().unwrap().to_path_buf();
    let mut earlier =
        agent(first, Mode::Auto, Arc::new(NonInteractive), dir.path()).with_session(session);
    run(&mut earlier, "what is 2+2?").await;
    drop(earlier);

    let (session, warnings) = Session::open(&path).unwrap();
    assert!(warnings.is_empty());
    let second = MockProvider::new(vec![Script::text("8")]);
    let mut resumed = agent(
        second.clone(),
        Mode::Auto,
        Arc::new(NonInteractive),
        dir.path(),
    )
    .with_session(session);
    assert_eq!(resumed.history().len(), 2);
    run(&mut resumed, "and doubled?").await;
    let messages = &second.requests()[0].messages;
    assert_eq!(messages.len(), 3);
    assert_eq!(
        messages[1],
        Message::Assistant {
            content: "the answer is 4".into(),
            tool_calls: vec![],
            model: "mock/m1".into()
        }
    );
    assert_eq!(session::list(&sessions).len(), 1);
}

#[tokio::test]
async fn notes_and_typed_commands_are_saved_as_such() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let provider = MockProvider::new(vec![Script::text("ok")]);
    let session = Session::create(&sessions, dir.path());
    let path = session.path().unwrap().to_path_buf();
    let mut agent =
        agent(provider, Mode::Auto, Arc::new(NonInteractive), dir.path()).with_session(session);
    agent.set_mode(Mode::Plan);
    let input = TurnInput {
        display: Some("/review src/lib.rs".into()),
        ..TurnInput::from("Review src/lib.rs carefully.")
    };
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    agent
        .run_turn(input, &tx, tokio_util::sync::CancellationToken::new())
        .await;
    let saved = lines(&path);
    assert_eq!(saved[1]["note"], true);
    assert_eq!(saved[2]["display"], "/review src/lib.rs");
    assert_eq!(
        saved[2]["message"]["content"],
        "Review src/lib.rs carefully."
    );
    assert_eq!(
        session::list(&sessions)[0].first_message.as_deref(),
        Some("/review src/lib.rs")
    );
}

#[tokio::test]
async fn a_session_that_cannot_be_saved_warns_once() {
    let dir = tempfile::tempdir().unwrap();
    let blocked = dir.path().join("sessions");
    std::fs::write(&blocked, "").unwrap();
    let provider = MockProvider::new(vec![Script::text("one"), Script::text("two")]);
    let mut agent = agent(provider, Mode::Auto, Arc::new(NonInteractive), dir.path())
        .with_session(Session::create(&blocked, dir.path()));
    let warnings = |events: &[AgentEvent]| {
        events
            .iter()
            .filter(|e| matches!(e, AgentEvent::Warning { message } if message.contains("cannot save the session")))
            .count()
    };
    let (_, events) = run(&mut agent, "first").await;
    assert_eq!(warnings(&events), 1, "{events:?}");
    let (_, events) = run(&mut agent, "second").await;
    assert_eq!(warnings(&events), 0, "{events:?}");
    assert_eq!(agent.history().len(), 4);
}

fn message(message: Message) -> EntryKind {
    EntryKind::Message {
        message,
        display: None,
        note: false,
        plan: None,
    }
}

fn calls(ids: &[&str], arguments: &str) -> Message {
    Message::Assistant {
        content: String::new(),
        tool_calls: ids
            .iter()
            .map(|id| ToolCall {
                id: id.to_string(),
                name: "echo".into(),
                arguments: arguments.into(),
            })
            .collect(),
        model: "mock/m1".into(),
    }
}

fn result(call_id: &str, content: &str) -> Message {
    Message::Tool {
        call_id: call_id.into(),
        content: content.into(),
        is_error: false,
    }
}

const STOPPED: &str = "harness stopped before this tool call finished; its effects are unknown";

fn stopped(call_id: &str) -> Message {
    Message::Tool {
        call_id: call_id.into(),
        content: STOPPED.into(),
        is_error: true,
    }
}

// Review D I1: a run killed during a tool call (SIGKILL, SIGTERM, a closed terminal) leaves a
// call without a result, which strict providers reject in every later request.
#[tokio::test]
async fn a_run_killed_during_a_tool_call_is_continued_with_a_result_for_every_call() {
    let dir = tempfile::tempdir().unwrap();
    let mut killed = Session::create(&dir.path().join("sessions"), dir.path());
    killed.append(message(Message::User {
        content: "run both".into(),
    }));
    killed.append(message(calls(&["c1", "c2"], r#"{"text":"x"}"#)));
    killed.append(message(result("c1", "x")));
    let path = killed.path().unwrap().to_path_buf();
    drop(killed);

    let (session, _) = Session::open(&path).unwrap();
    let provider = MockProvider::new(vec![Script::text("c2 did not finish")]);
    let mut agent = agent(
        provider.clone(),
        Mode::Auto,
        Arc::new(NonInteractive),
        dir.path(),
    )
    .with_session(session);
    let (_, events) = run(&mut agent, "what happened?").await;
    let expected = vec![
        Message::User {
            content: "run both".into(),
        },
        calls(&["c1", "c2"], r#"{"text":"x"}"#),
        result("c1", "x"),
        stopped("c2"),
        Message::User {
            content: "what happened?".into(),
        },
    ];
    assert_eq!(provider.requests()[0].messages, expected);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Warning { message } if message.contains("stopped"))),
        "{events:?}"
    );
    // The result was saved, so the file is consistent again.
    drop(agent);
    let (session, _) = Session::open(&path).unwrap();
    let saved: Vec<Message> = session.messages().into_iter().map(|(_, m)| m).collect();
    assert_eq!(saved[..4], expected[..4]);
}

// A call left without a result further back (a file an earlier version continued) is answered
// in memory, and compaction still cuts at the right entry.
#[tokio::test]
async fn a_call_without_a_result_earlier_on_is_answered_and_compaction_keeps_the_right_part() {
    let dir = tempfile::tempdir().unwrap();
    let mut earlier = Session::create(&dir.path().join("sessions"), dir.path());
    earlier.append(message(Message::User {
        content: "a".repeat(400),
    }));
    // Large arguments, so the call does not fit the part compaction keeps.
    earlier.append(message(calls(
        &["c1"],
        &format!(r#"{{"text":"{}"}}"#, "y".repeat(4_000)),
    )));
    earlier.append(message(Message::User {
        content: "B".into(),
    }));
    earlier.append(message(Message::Assistant {
        content: "ok".into(),
        tool_calls: vec![],
        model: "mock/m1".into(),
    }));
    let path = earlier.path().unwrap().to_path_buf();
    drop(earlier);

    let (session, _) = Session::open(&path).unwrap();
    let provider = MockProvider::new(vec![Script::text("answer C"), Script::text("summary")]);
    let mut agent = agent(
        provider.clone(),
        Mode::Auto,
        Arc::new(NonInteractive),
        dir.path(),
    )
    .with_session(session);
    run(&mut agent, "C").await;
    let sent = &provider.requests()[0].messages;
    assert_eq!(sent[2], stopped("c1"));
    assert_eq!(
        sent[3],
        Message::User {
            content: "B".into()
        }
    );
    // Only answered in memory: the file is left as it was.
    assert!(!std::fs::read_to_string(&path).unwrap().contains(STOPPED));

    agent.config_mut().context_window = 500;
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    agent
        .compact(None, &tx, tokio_util::sync::CancellationToken::new())
        .await
        .unwrap();
    let kept: Vec<&Message> = agent.history().iter().skip(1).collect();
    assert_eq!(
        kept.first(),
        Some(&&Message::User {
            content: "B".into()
        }),
        "{:?}",
        agent.history()
    );
    assert_eq!(kept.len(), 4, "{:?}", agent.history());
}

// `/new` and `/resume`: the agent continues in another session, with the same model, mode and
// system prompt, and the session it leaves is free for another process.
#[tokio::test]
async fn the_agent_can_continue_in_another_session() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let provider = MockProvider::new(vec![
        Script::text("first answer"),
        Script::text("fresh answer"),
        Script::text("back again"),
    ]);
    let first = Session::create(&sessions, dir.path());
    let first_path = first.path().unwrap().to_path_buf();
    let mut agent = agent(
        provider.clone(),
        Mode::Auto,
        Arc::new(NonInteractive),
        dir.path(),
    )
    .with_session(first);
    run(&mut agent, "first question").await;
    agent.start_session(Session::create(&sessions, dir.path()), None);
    assert!(agent.history().is_empty());
    assert!(agent.rewind_points().is_empty());
    assert!(
        Session::open(&first_path).is_ok(),
        "the first session is free"
    );
    run(&mut agent, "fresh question").await;
    let sent = provider.requests().last().unwrap().messages.clone();
    assert_eq!(
        sent,
        [Message::User {
            content: "fresh question".into()
        }]
    );
    let (first, _) = Session::open(&first_path).unwrap();
    agent.start_session(first, None);
    let points: Vec<String> = agent.rewind_points().into_iter().map(|p| p.text).collect();
    assert_eq!(points, ["first question"]);
    run(&mut agent, "and again").await;
    let sent = provider.requests().last().unwrap().messages.clone();
    assert_eq!(
        sent.first(),
        Some(&Message::User {
            content: "first question".into()
        })
    );
    assert!(
        !sent
            .iter()
            .any(|m| matches!(m, Message::User { content } if content.contains("fresh question")))
    );
    assert_eq!(session::list(&sessions).len(), 2);
}
