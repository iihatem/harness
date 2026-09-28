mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use common::*;
use harness_core::agent::{Agent, NonInteractive, RewindError};
use harness_core::checkpoint::Checkpoints;
use harness_core::event::AgentEvent;
use harness_core::message::Message;
use harness_core::permission::Mode;
use harness_core::session::{RewindScope, Session};
use harness_core::testing::{MockProvider, Script};
use serde_json::json;

/// A workspace, and harness's data directory beside it.
struct Fixture {
    _dir: tempfile::TempDir,
    ws: PathBuf,
    data: PathBuf,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    std::fs::create_dir(base.join("ws")).unwrap();
    Fixture {
        ws: base.join("ws"),
        data: base.join("data"),
        _dir: dir,
    }
}

impl Fixture {
    /// An agent in `mode`, with a sandbox, whose session and checkpoints live in the data
    /// directory.
    fn agent(&self, provider: Arc<MockProvider>, mode: Mode) -> Agent {
        let session = Session::create(&self.data.join("sessions"), &self.ws);
        let checkpoints =
            Checkpoints::open(&self.data.join("checkpoints.git"), &self.ws, session.id()).unwrap();
        agent_with_sandbox(provider, mode, Arc::new(NonInteractive), &self.ws)
            .with_session(session)
            .with_checkpoints(Some(Arc::new(checkpoints)))
    }

    fn write(&self, path: &str, text: &str) {
        std::fs::write(self.ws.join(path), text).unwrap();
    }

    fn read(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(self.ws.join(path)).ok()
    }
}

fn put(id: &str, path: &str, content: &str) -> Script {
    Script::tool_call(id, "put", json!({"path": path, "content": content}))
}

fn user_texts(agent: &Agent) -> Vec<String> {
    agent
        .history()
        .iter()
        .filter_map(|m| match m {
            Message::User { content } => Some(content.clone()),
            _ => None,
        })
        .collect()
}

fn checkpoints_created(events: &[AgentEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, AgentEvent::CheckpointCreated { .. }))
        .count()
}

/// The rewind point of the user message `text`.
fn point(agent: &Agent, text: &str) -> String {
    agent
        .rewind_points()
        .into_iter()
        .find(|p| p.text == text)
        .unwrap()
        .entry
}

#[tokio::test]
async fn one_checkpoint_is_taken_before_the_first_change_of_each_turn() {
    let f = fixture();
    let provider = MockProvider::new(vec![
        Script::tool_call("c1", "echo", json!({"text": "look"})),
        put("c2", "a.txt", "one"),
        put("c3", "b.txt", "two"),
        Script::text("done"),
        Script::tool_call("c4", "echo", json!({"text": "look"})),
        Script::text("nothing to change"),
    ]);
    let mut agent = f.agent(provider, Mode::Auto);
    let (_, events) = run(&mut agent, "write two files").await;
    assert_eq!(checkpoints_created(&events), 1);
    let checkpoint = events
        .iter()
        .position(|e| matches!(e, AgentEvent::CheckpointCreated { .. }))
        .unwrap();
    let first_write = events
        .iter()
        .position(|e| matches!(e, AgentEvent::ToolCallFinished { id, .. } if id == "c2"))
        .unwrap();
    let read = events
        .iter()
        .position(|e| matches!(e, AgentEvent::ToolCallFinished { id, .. } if id == "c1"))
        .unwrap();
    assert!(read < checkpoint && checkpoint < first_write, "{events:?}");
    let (_, events) = run(&mut agent, "only look").await;
    assert_eq!(checkpoints_created(&events), 0);
}

#[tokio::test]
async fn read_only_modes_take_no_checkpoint_for_shell_commands() {
    let f = fixture();
    let provider = MockProvider::new(vec![
        Script::tool_call("c1", "sh", json!({"command": "ls"})),
        Script::text("done"),
    ]);
    let mut agent = f.agent(provider, Mode::ReadOnly);
    let (_, events) = run(&mut agent, "list").await;
    assert_eq!(checkpoints_created(&events), 0, "{events:?}");
}

// Spec: undo a bad refactor.
#[tokio::test]
async fn rewinding_code_and_conversation_undoes_a_bad_refactor() {
    let f = fixture();
    f.write("a.rs", "a\n");
    f.write("b.rs", "b\n");
    f.write("c.rs", "c\n");
    f.write("old.rs", "old\n");
    let provider = MockProvider::new(vec![
        Script::text("hello"),
        put("c1", "a.rs", "a2\n"),
        put("c2", "b.rs", "b2\n"),
        put("c3", "new.rs", "new\n"),
        Script::tool_call(
            "c4",
            "sh",
            json!({"command": "echo formatted > c.rs && rm old.rs"}),
        ),
        Script::text("refactored"),
        Script::text("fresh start"),
    ]);
    let mut agent = f.agent(provider.clone(), Mode::Auto);
    run(&mut agent, "hi").await;
    run(&mut agent, "refactor everything").await;
    assert_eq!(f.read("c.rs").as_deref(), Some("formatted\n"));

    let target = point(&agent, "refactor everything");
    agent
        .rewind(&target, RewindScope::CodeAndConversation)
        .await
        .unwrap();
    assert_eq!(f.read("a.rs").as_deref(), Some("a\n"));
    assert_eq!(f.read("b.rs").as_deref(), Some("b\n"));
    assert_eq!(f.read("c.rs").as_deref(), Some("c\n"));
    assert_eq!(f.read("old.rs").as_deref(), Some("old\n"));
    assert_eq!(f.read("new.rs"), None);
    assert_eq!(user_texts(&agent), ["hi"]);
    run(&mut agent, "try again").await;
    let last = provider.requests().pop().unwrap();
    assert_eq!(last.messages.len(), 3, "{:?}", last.messages);
}

// Spec: conversation only.
#[tokio::test]
async fn rewinding_the_conversation_only_leaves_the_files() {
    let f = fixture();
    let provider = MockProvider::new(vec![
        put("c1", "a.txt", "changed"),
        Script::text("done"),
        Script::text("again"),
    ]);
    let mut agent = f.agent(provider, Mode::Auto);
    run(&mut agent, "change it").await;
    let target = point(&agent, "change it");
    agent
        .rewind(&target, RewindScope::Conversation)
        .await
        .unwrap();
    assert_eq!(f.read("a.txt").as_deref(), Some("changed"));
    assert!(agent.history().is_empty());
    run(&mut agent, "next").await;
    assert_eq!(user_texts(&agent), ["next"]);
}

#[tokio::test]
async fn rewinding_code_only_keeps_the_conversation() {
    let f = fixture();
    f.write("a.txt", "original");
    let provider = MockProvider::new(vec![put("c1", "a.txt", "changed"), Script::text("done")]);
    let mut agent = f.agent(provider, Mode::Auto);
    run(&mut agent, "change it").await;
    let history = agent.history().to_vec();
    let target = point(&agent, "change it");
    agent.rewind(&target, RewindScope::Code).await.unwrap();
    assert_eq!(f.read("a.txt").as_deref(), Some("original"));
    assert_eq!(agent.history(), history.as_slice());
}

// Spec: rewind by mistake.
#[tokio::test]
async fn the_last_rewind_can_be_undone() {
    let f = fixture();
    f.write("a.txt", "original");
    let provider = MockProvider::new(vec![
        put("c1", "a.txt", "turn one"),
        Script::text("one"),
        put("c2", "a.txt", "turn two"),
        Script::text("two"),
    ]);
    let mut agent = f.agent(provider, Mode::Auto);
    run(&mut agent, "first").await;
    run(&mut agent, "second").await;
    assert!(!agent.can_undo_rewind());
    let history = agent.history().to_vec();
    let target = point(&agent, "first");
    agent
        .rewind(&target, RewindScope::CodeAndConversation)
        .await
        .unwrap();
    assert_eq!(f.read("a.txt").as_deref(), Some("original"));
    assert!(agent.can_undo_rewind());
    agent.undo_rewind().await.unwrap();
    assert_eq!(f.read("a.txt").as_deref(), Some("turn two"));
    assert_eq!(agent.history(), history.as_slice());
    assert!(matches!(
        agent.undo_rewind().await,
        Err(RewindError::NothingToUndo)
    ));
}

// Spec: branch after rewind.
#[tokio::test]
async fn a_rewound_branch_stays_in_the_session_file() {
    let f = fixture();
    let provider = MockProvider::new(vec![
        Script::text("1"),
        Script::text("2"),
        Script::text("3"),
        Script::text("3b"),
    ]);
    let mut agent = f.agent(provider.clone(), Mode::Auto);
    run(&mut agent, "one").await;
    run(&mut agent, "two").await;
    run(&mut agent, "three").await;
    let target = point(&agent, "three");
    agent
        .rewind(&target, RewindScope::Conversation)
        .await
        .unwrap();
    run(&mut agent, "three, differently").await;
    let saved = std::fs::read_to_string(agent.session().path().unwrap()).unwrap();
    assert!(saved.contains("\"three\"") && saved.contains("\"3\""));
    let seen: Vec<String> = provider
        .requests()
        .pop()
        .unwrap()
        .messages
        .iter()
        .filter_map(|m| match m {
            Message::User { content } => Some(content.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(seen, ["one", "two", "three, differently"]);
    // The rewind survives a restart.
    let path = agent.session().path().unwrap().to_path_buf();
    drop(agent);
    let (session, _) = Session::open(&path).unwrap();
    let resumed = agent_in(&f.ws, session);
    assert_eq!(user_texts(&resumed), ["one", "two", "three, differently"]);
}

fn agent_in(ws: &Path, session: Session) -> Agent {
    agent(
        MockProvider::new(vec![]),
        Mode::Auto,
        Arc::new(NonInteractive),
        ws,
    )
    .with_session(session)
}

// Spec: a snapshot that takes too long disables checkpoints instead of blocking the turn.
#[tokio::test]
async fn a_failing_snapshot_disables_checkpoints_with_one_warning() {
    let f = fixture();
    let provider = MockProvider::new(vec![
        put("c1", "a.txt", "one"),
        Script::text("done"),
        put("c2", "b.txt", "two"),
        Script::text("done"),
    ]);
    let session = Session::create(&f.data.join("sessions"), &f.ws);
    let slow = Checkpoints::open(&f.data.join("checkpoints.git"), &f.ws, session.id())
        .unwrap()
        .with_timeout(Duration::from_millis(1));
    let mut agent = agent(provider, Mode::Auto, Arc::new(NonInteractive), &f.ws)
        .with_session(session)
        .with_checkpoints(Some(Arc::new(slow)));
    let warned = |events: &[AgentEvent]| {
        events
            .iter()
            .filter(|e| matches!(e, AgentEvent::Warning { message } if message.starts_with("checkpoints are disabled for this session")))
            .count()
    };
    let (_, events) = run(&mut agent, "first").await;
    assert_eq!(warned(&events), 1, "{events:?}");
    assert_eq!(f.read("a.txt").as_deref(), Some("one"));
    let (_, events) = run(&mut agent, "second").await;
    assert_eq!(warned(&events), 0);
    assert_eq!(f.read("b.txt").as_deref(), Some("two"));
    let target = point(&agent, "first");
    assert!(matches!(
        agent.rewind(&target, RewindScope::Code).await,
        Err(RewindError::NoCheckpoints)
    ));
    agent
        .rewind(&target, RewindScope::Conversation)
        .await
        .unwrap();
}

#[tokio::test]
async fn only_user_messages_are_rewind_points() {
    let f = fixture();
    let provider = MockProvider::new(vec![Script::text("ok")]);
    let mut agent = f.agent(provider, Mode::Auto);
    agent.set_mode(Mode::Plan);
    run(&mut agent, "plan it").await;
    let points = agent.rewind_points();
    assert_eq!(points.len(), 1);
    assert_eq!(points[0].text, "plan it");
    assert!(matches!(
        agent
            .rewind("not-an-entry", RewindScope::Conversation)
            .await,
        Err(RewindError::UnknownPoint(_))
    ));
}

// Review E issue 4 (probe C): a session continued with `-c` from a subdirectory of the project
// keeps its checkpoints, but restores only those taken for the directory it now runs in.
#[tokio::test]
async fn checkpoints_taken_in_another_directory_are_not_restored() {
    let f = fixture();
    f.write("a.txt", "original");
    std::fs::create_dir(f.ws.join("sub")).unwrap();
    f.write("sub/x.txt", "x");
    let provider = MockProvider::new(vec![put("c1", "a.txt", "changed"), Script::text("done")]);
    let mut first = f.agent(provider, Mode::Auto);
    run(&mut first, "change it").await;
    let path = first.session().path().unwrap().to_path_buf();
    drop(first);
    let (session, _) = Session::open(&path).unwrap();
    let sub = f.ws.join("sub");
    let checkpoints =
        Checkpoints::open(&f.data.join("checkpoints.git"), &sub, session.id()).unwrap();
    let mut resumed = agent_with_sandbox(
        MockProvider::new(vec![]),
        Mode::Auto,
        Arc::new(NonInteractive),
        &sub,
    )
    .with_session(session)
    .with_checkpoints(Some(Arc::new(checkpoints)));
    let target = point(&resumed, "change it");
    match resumed.rewind(&target, RewindScope::Code).await {
        Err(RewindError::OtherWorkspace(taken)) => assert_eq!(taken, f.ws),
        other => panic!("{other:?}"),
    }
    assert_eq!(f.read("a.txt").as_deref(), Some("changed"));
    assert!(!sub.join("a.txt").exists());
    resumed
        .rewind(&target, RewindScope::Conversation)
        .await
        .unwrap();
}
