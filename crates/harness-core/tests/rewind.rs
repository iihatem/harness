mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use common::*;
use harness_core::agent::{Agent, NonInteractive, RewindError};
use harness_core::checkpoint::{CheckpointError, Checkpoints};
use harness_core::event::AgentEvent;
use harness_core::message::Message;
use harness_core::permission::Mode;
use harness_core::session::{EntryKind, RewindScope, Session};
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

/// The `bash` a slash command's shell part runs: `/bin/sh` in the workspace, for real.
struct WritingBash;
#[async_trait::async_trait]
impl harness_core::tool::Tool for WritingBash {
    fn spec(&self) -> harness_core::message::ToolSpec {
        harness_core::message::ToolSpec {
            name: "bash".into(),
            description: "runs a command".into(),
            parameters: json!({"type": "object", "properties": {"command": {"type": "string"}}, "required": ["command"]}),
        }
    }
    fn action(
        &self,
        args: &serde_json::Value,
        _ctx: &harness_core::tool::ToolContext,
    ) -> harness_core::permission::Action {
        harness_core::permission::Action::Bash(args["command"].as_str().unwrap_or_default().into())
    }
    async fn run(
        &self,
        args: serde_json::Value,
        ctx: &harness_core::tool::ToolContext,
    ) -> harness_core::tool::ToolOutput {
        let status = std::process::Command::new("/bin/sh")
            .args(["-c", args["command"].as_str().unwrap_or_default()])
            .current_dir(&ctx.workspace)
            .status()
            .unwrap();
        harness_core::tool::ToolOutput::ok(format!("exit code {}\n", status.code().unwrap_or(-1)))
    }
}

// Review E issue 5 (probe B): a slash command whose `!` shell part changes files takes the turn's
// checkpoint before its user message is recorded. Rewinding to that message restores it.
#[tokio::test]
async fn a_slash_commands_shell_part_is_rewound_with_its_turn() {
    use harness_core::turn::{InputPart, TurnInput};
    let f = fixture();
    f.write("a.txt", "original\n");
    let session = Session::create(&f.data.join("sessions"), &f.ws);
    let checkpoints =
        Checkpoints::open(&f.data.join("checkpoints.git"), &f.ws, session.id()).unwrap();
    let policy = Arc::new(harness_core::engine::PermissionEngine::new(
        harness_core::engine::EngineConfig {
            mode: Mode::Auto,
            workspace: f.ws.clone(),
            read_dirs: vec![],
            rules: Default::default(),
            sandbox_available: true,
            writes_need_approval: false,
        },
    ));
    let mut agent = Agent::new(
        MockProvider::new(vec![Script::text("formatted it")]),
        harness_core::tool::ToolRegistry::new(vec![Arc::new(WritingBash)]),
        policy,
        Arc::new(NonInteractive),
        harness_core::agent::AgentConfig::new("mock/m1", "m1", "system", f.ws.join(".spill")),
        harness_core::tool::ToolContext::new(&f.ws).with_sandbox(None, Mode::Auto.fs_access()),
    )
    .with_session(session)
    .with_checkpoints(Some(Arc::new(checkpoints)));
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let input = TurnInput {
        parts: vec![
            InputPart::Shell("echo formatted > a.txt".into()),
            InputPart::Text("Say what changed.".into()),
        ],
        display: Some("/fmt".into()),
        ..TurnInput::default()
    };
    agent
        .run_turn(input, &tx, tokio_util::sync::CancellationToken::new())
        .await;
    assert_eq!(f.read("a.txt").as_deref(), Some("formatted\n"));
    let target = point(&agent, "/fmt");
    agent
        .rewind(&target, RewindScope::CodeAndConversation)
        .await
        .unwrap();
    assert_eq!(f.read("a.txt").as_deref(), Some("original\n"));
}

// Review E minor 6 (probe O): a turn that changed files while checkpoints were off leaves a gap no
// snapshot covers, so code is not restored across it; later turns can still be rewound.
#[tokio::test]
async fn code_is_not_restored_across_a_turn_without_a_checkpoint() {
    let f = fixture();
    f.write("a.txt", "original");
    f.write("b.txt", "original");
    let session = Session::create(&f.data.join("sessions"), &f.ws);
    let path = session.path().unwrap().to_path_buf();
    let provider = MockProvider::new(vec![put("c1", "a.txt", "one"), Script::text("done")]);
    let mut first = agent_with_sandbox(provider, Mode::Auto, Arc::new(NonInteractive), &f.ws)
        .with_session(session)
        .with_checkpoints(None);
    run(&mut first, "turn one").await;
    drop(first);
    let (session, _) = Session::open(&path).unwrap();
    let checkpoints =
        Checkpoints::open(&f.data.join("checkpoints.git"), &f.ws, session.id()).unwrap();
    let provider = MockProvider::new(vec![put("c2", "b.txt", "two"), Script::text("done")]);
    let mut second = agent_with_sandbox(provider, Mode::Auto, Arc::new(NonInteractive), &f.ws)
        .with_session(session)
        .with_checkpoints(Some(Arc::new(checkpoints)));
    run(&mut second, "turn two").await;
    let one = point(&second, "turn one");
    assert!(matches!(
        second.rewind(&one, RewindScope::CodeAndConversation).await,
        Err(RewindError::Unrecorded)
    ));
    assert_eq!(f.read("a.txt").as_deref(), Some("one"));
    assert_eq!(f.read("b.txt").as_deref(), Some("two"));
    let two = point(&second, "turn two");
    second.rewind(&two, RewindScope::Code).await.unwrap();
    assert_eq!(f.read("a.txt").as_deref(), Some("one"));
    assert_eq!(f.read("b.txt").as_deref(), Some("original"));
}

// Review E minor 11: undoing a rewind snapshots the workspace first, like a rewind, and the
// session keeps that snapshot: edits made between the rewind and the undo are not lost.
#[tokio::test]
async fn undoing_a_rewind_keeps_a_snapshot_of_what_it_replaced() {
    let f = fixture();
    f.write("a.txt", "original");
    let provider = MockProvider::new(vec![put("c1", "a.txt", "turn one"), Script::text("one")]);
    let mut agent = f.agent(provider, Mode::Auto);
    run(&mut agent, "first").await;
    let target = point(&agent, "first");
    agent.rewind(&target, RewindScope::Code).await.unwrap();
    f.write("a.txt", "edited in an editor after the rewind");
    agent.undo_rewind().await.unwrap();
    assert_eq!(f.read("a.txt").as_deref(), Some("turn one"));
    let Some(EntryKind::UndoRewind {
        snapshot: Some(snapshot),
        ..
    }) = agent.session().branch().last().map(|e| e.kind.clone())
    else {
        panic!("{:?}", agent.session().branch().last());
    };
    let checkpoints =
        Checkpoints::open(&f.data.join("checkpoints.git"), &f.ws, agent.session().id()).unwrap();
    checkpoints.restore(&snapshot).unwrap();
    assert_eq!(
        f.read("a.txt").as_deref(),
        Some("edited in an editor after the rewind")
    );
}

// Review E minor 11: a rewind whose restore fails partway is recorded, so undoing it puts the
// files back as they were before it began.
#[tokio::test]
async fn a_rewind_that_fails_partway_can_be_undone() {
    use std::os::unix::fs::PermissionsExt;
    if is_root() {
        return; // root writes into read-only directories
    }
    let f = fixture();
    let provider = MockProvider::new(vec![
        Script::text("hello"),
        put("c1", "ro/a.txt", "two"),
        put("c2", "b.txt", "two"),
        Script::text("done"),
    ]);
    let mut agent = f.agent(provider, Mode::Auto);
    run(&mut agent, "hi").await;
    run(&mut agent, "change them").await;
    let history = agent.history().to_vec();
    let ro = f.ws.join("ro");
    std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o555)).unwrap();
    let target = point(&agent, "change them");
    let failed = agent
        .rewind(&target, RewindScope::CodeAndConversation)
        .await;
    std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        matches!(
            failed,
            Err(RewindError::Restore(CheckpointError::Restore { .. }))
        ),
        "{failed:?}"
    );
    assert_eq!(agent.history(), history.as_slice());
    assert!(agent.can_undo_rewind());
    agent.undo_rewind().await.unwrap();
    assert_eq!(f.read("ro/a.txt").as_deref(), Some("two"));
    assert_eq!(f.read("b.txt").as_deref(), Some("two"));
}

// Re-review E, nit a: an undo whose restore fails partway is recorded like a rewind of code, so it
// can be undone in turn; after that, the rewind can be undone again.
#[tokio::test]
async fn an_undo_that_fails_partway_can_be_undone_and_then_retried() {
    use std::os::unix::fs::PermissionsExt;
    if is_root() {
        return; // root writes into read-only directories
    }
    let f = fixture();
    let provider = MockProvider::new(vec![
        put("c1", "ro/a.txt", "one"),
        Script::text("done"),
        put("c2", "ro/a.txt", "two"),
        Script::text("done"),
    ]);
    let mut agent = f.agent(provider, Mode::Auto);
    run(&mut agent, "first").await;
    run(&mut agent, "second").await;
    let target = point(&agent, "second");
    agent
        .rewind(&target, RewindScope::CodeAndConversation)
        .await
        .unwrap();
    assert_eq!(f.read("ro/a.txt").as_deref(), Some("one"));
    let rewound = agent.history().to_vec();
    let ro = f.ws.join("ro");
    std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o555)).unwrap();
    let failed = agent.undo_rewind().await;
    std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        matches!(
            failed,
            Err(RewindError::Restore(CheckpointError::Restore { .. }))
        ),
        "{failed:?}"
    );
    assert_eq!(agent.history(), rewound.as_slice());
    // Undoing the failed undo: the files as they were just before it.
    assert!(agent.can_undo_rewind());
    agent.undo_rewind().await.unwrap();
    assert_eq!(f.read("ro/a.txt").as_deref(), Some("one"));
    assert_eq!(agent.history(), rewound.as_slice());
    // And the rewind itself can still be undone.
    assert!(agent.can_undo_rewind());
    agent.undo_rewind().await.unwrap();
    assert_eq!(f.read("ro/a.txt").as_deref(), Some("two"));
    assert_eq!(user_texts(&agent), ["first", "second"]);
    assert!(!agent.can_undo_rewind());
}

// Undoing a rewind made right after another one returns to just after the first, where nothing
// has happened since it: that one can be undone too.
#[tokio::test]
async fn undoing_a_second_rewind_leaves_the_first_undoable() {
    let f = fixture();
    f.write("a.txt", "original");
    let provider = MockProvider::new(vec![
        put("c1", "a.txt", "one"),
        Script::text("done"),
        put("c2", "a.txt", "two"),
        Script::text("done"),
    ]);
    let mut agent = f.agent(provider, Mode::Auto);
    run(&mut agent, "first").await;
    run(&mut agent, "second").await;
    let second = point(&agent, "second");
    agent.rewind(&second, RewindScope::Code).await.unwrap();
    assert_eq!(f.read("a.txt").as_deref(), Some("one"));
    let first = point(&agent, "first");
    agent.rewind(&first, RewindScope::Code).await.unwrap();
    assert_eq!(f.read("a.txt").as_deref(), Some("original"));
    agent.undo_rewind().await.unwrap();
    assert_eq!(f.read("a.txt").as_deref(), Some("one"));
    assert!(agent.can_undo_rewind());
    agent.undo_rewind().await.unwrap();
    assert_eq!(f.read("a.txt").as_deref(), Some("two"));
    assert!(!agent.can_undo_rewind());
}

/// Whether the tests run as root, which permissions do not stop.
fn is_root() -> bool {
    // SAFETY: `geteuid` cannot fail.
    unsafe { libc::geteuid() == 0 }
}

// Review E minor 10: the text the rewind list shows names everything a rewind leaves alone.
#[test]
fn the_rewind_limits_name_what_a_rewind_leaves_alone() {
    for words in [
        "network calls",
        "databases",
        "pushed commits",
        "outside the workspace",
        "nested git repositories",
        "submodules",
        "git-ignored",
        "10 MB",
    ] {
        assert!(
            harness_core::agent::REWIND_LIMITS.contains(words),
            "{words}"
        );
    }
}
