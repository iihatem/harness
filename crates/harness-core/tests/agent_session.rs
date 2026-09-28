mod common;

use std::sync::Arc;

use common::*;
use harness_core::agent::NonInteractive;
use harness_core::event::AgentEvent;
use harness_core::message::Message;
use harness_core::permission::Mode;
use harness_core::session::{self, Session};
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
