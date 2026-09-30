//! Leaving the session never hangs, and never leaves anything waiting or running: the user
//! quitting with an approval waiting, the terminal going away, and harness being asked to stop.

use std::{
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use harness_core::{
    agent::{Agent, AgentConfig, ApprovalDecision, ApprovalKind, ApprovalRequest},
    engine::{EngineConfig, PermissionEngine, RuleSet},
    permission::{Action, Mode},
    testing::{MockProvider, Script},
    tool::{Tool, ToolContext, ToolRegistry},
};
use harness_tui::{
    app::{Host, Options, Prepared},
    approval::{ChannelApprover, Requests},
    inline::InlineTerminal,
    notify::Notify,
    style::Theme,
    ui::{Ending, Flow, Ui},
};
use ratatui::{
    backend::TestBackend,
    crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers},
};
use serde_json::json;
use tokio::sync::{mpsc, oneshot};

struct NoCommands;

impl Host for NoCommands {
    fn is_command(&self, _name: &str) -> bool {
        false
    }
    fn prepare(&mut self, _typed: &str) -> Prepared {
        unreachable!()
    }
}

/// Passes on what it is asked to notify.
struct Forward(mpsc::UnboundedSender<String>);

impl Notify for Forward {
    fn notify(&mut self, text: &str) -> std::io::Result<()> {
        let _ = self.0.send(text.to_string());
        Ok(())
    }
}

/// A session in `mode` whose `bash` runs commands directly, as if sandboxed, with its approvals
/// from `approvals` when given, and its notifications to `notified`.
fn start(
    provider: Arc<MockProvider>,
    dir: &Path,
    mode: Mode,
    approvals: Option<Requests>,
    notified: Option<mpsc::UnboundedSender<String>>,
) -> Ui<TestBackend> {
    let policy = Arc::new(PermissionEngine::new(EngineConfig {
        mode,
        workspace: dir.to_path_buf(),
        read_dirs: Vec::new(),
        rules: RuleSet::default(),
        sandbox_available: true,
        writes_need_approval: false,
    }));
    let tools: Vec<Arc<dyn Tool>> = vec![harness_tools::builtin().get("bash").unwrap()];
    let (approver, agent_approvals) = ChannelApprover::new();
    let agent = Agent::new(
        provider,
        ToolRegistry::new(tools),
        policy,
        approver,
        AgentConfig::new("mock/m", "m", "system", dir.join("out")),
        ToolContext::new(dir),
    );
    let options = Options {
        theme: Theme::monochrome(),
        model: "mock/m".into(),
        mode,
        commands: Vec::new(),
        workspace: dir.to_path_buf(),
        history: Vec::new(),
        instruction_files: Vec::new(),
        window_note: None,
        default_mode: Mode::Auto,
        text_editor: None,
        notifier: notified.map(|n| Box::new(Forward(n)) as Box<dyn Notify>),
    };
    let term = InlineTerminal::new(TestBackend::new(80, 24), 0).unwrap();
    let mut ui = Ui::start(
        agent,
        Box::new(NoCommands),
        term,
        options,
        approvals.unwrap_or(agent_approvals),
    );
    ui.draw().unwrap();
    ui
}

fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
    Event::Key(KeyEvent::new(code, modifiers))
}

fn ctrl_c() -> Event {
    key(KeyCode::Char('c'), KeyModifiers::CONTROL)
}

fn typed(text: &str) -> Vec<Event> {
    let mut keys: Vec<Event> = text
        .chars()
        .map(|c| key(KeyCode::Char(c), KeyModifiers::NONE))
        .collect();
    keys.push(key(KeyCode::Enter, KeyModifiers::NONE));
    keys
}

fn request() -> ApprovalRequest {
    ApprovalRequest {
        call_id: "c1".into(),
        tool: "bash".into(),
        action: Action::Bash("cargo test".into()),
        reason: "run `cargo test`".into(),
        kind: ApprovalKind::Action,
        kept_for_session: true,
    }
}

fn denied(answer: Result<ApprovalDecision, oneshot::error::TryRecvError>) -> bool {
    matches!(answer, Ok(ApprovalDecision::Deny { .. }))
}

// Review D I2, probe 7: the prompt came after the first Ctrl+C; the second one leaves, and the
// approval is denied then, not left waiting.
#[tokio::test]
async fn quitting_denies_the_approval_waiting() {
    let dir = tempfile::tempdir().unwrap();
    let (requests, approvals) = mpsc::unbounded_channel();
    let mut ui = start(
        MockProvider::new(vec![]),
        dir.path(),
        Mode::Ask,
        Some(approvals),
        None,
    );
    assert_eq!(ui.handle(ctrl_c()).unwrap(), Flow::Continue);
    let (reply, mut answer) = oneshot::channel();
    requests.send((request(), reply)).unwrap();
    tokio::time::timeout(
        Duration::from_secs(10),
        ui.until(|app| app.prompt().is_some()),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(ui.handle(ctrl_c()).unwrap(), Flow::Quit);
    assert!(denied(answer.try_recv()));
    assert!(ui.app().prompt().is_none());
    ui.finish().await.unwrap();
}

// Review D I2, probe 5: ending the session denies what waits, shown or not yet.
#[tokio::test]
async fn finishing_denies_every_approval_waiting() {
    let dir = tempfile::tempdir().unwrap();
    let (requests, approvals) = mpsc::unbounded_channel();
    let mut ui = start(
        MockProvider::new(vec![]),
        dir.path(),
        Mode::Ask,
        Some(approvals),
        None,
    );
    let (shown, mut first) = oneshot::channel();
    requests.send((request(), shown)).unwrap();
    tokio::time::timeout(
        Duration::from_secs(10),
        ui.until(|app| app.prompt().is_some()),
    )
    .await
    .unwrap()
    .unwrap();
    let (queued, mut second) = oneshot::channel();
    requests.send((request(), queued)).unwrap();
    tokio::time::timeout(Duration::from_secs(3), ui.finish())
        .await
        .expect("finish returns")
        .unwrap();
    assert!(denied(first.try_recv()));
    // Never shown: its reply is dropped, which the agent's approver takes as a denial.
    assert!(matches!(
        second.try_recv(),
        Err(oneshot::error::TryRecvError::Closed)
    ));
    // Later requests are refused at once.
    let (late, mut third) = oneshot::channel();
    assert!(requests.send((request(), late)).is_err());
    assert!(third.try_recv().is_err());
}

/// Runs `ui` on keys from a channel until it ends, while `drive` sends them; how it ended.
async fn run_while<F>(
    ui: &mut Ui<TestBackend>,
    shutdown: impl std::future::Future<Output = Ending>,
    drive: impl FnOnce(Keys) -> F,
) -> Ending
where
    F: std::future::Future<Output = ()>,
{
    let (keys, input) = futures::channel::mpsc::unbounded();
    let (ending, ()) = tokio::join!(
        tokio::time::timeout(Duration::from_secs(10), ui.run(input, shutdown)),
        drive(keys),
    );
    ending.expect("the session ends promptly").unwrap()
}

type Keys = futures::channel::mpsc::UnboundedSender<std::io::Result<Event>>;

/// Keeps the keys' stream open, as a terminal that stays.
fn keep_open(keys: Keys) {
    tokio::spawn(async move {
        let _keys = keys;
        std::future::pending::<()>().await
    });
}

async fn approval_asked(notified: &mut mpsc::UnboundedReceiver<String>) {
    loop {
        let text = notified.recv().await.unwrap();
        if text.starts_with("approval needed") {
            return;
        }
    }
}

// Review D I2: Ctrl+C twice with an approval waiting leaves at once, and nothing ran.
#[tokio::test]
async fn ctrl_c_twice_with_an_approval_waiting_exits_promptly() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call("b1", "bash", json!({"command": "touch ran"})),
        Script::text("never asked"),
    ]);
    let (notify, mut notified) = mpsc::unbounded_channel();
    let mut ui = start(provider.clone(), dir.path(), Mode::Ask, None, Some(notify));
    let ending = run_while(&mut ui, std::future::pending(), |keys| async move {
        for event in typed("go") {
            keys.unbounded_send(Ok(event)).unwrap();
        }
        approval_asked(&mut notified).await;
        keys.unbounded_send(Ok(ctrl_c())).unwrap();
        keys.unbounded_send(Ok(ctrl_c())).unwrap();
        // Only the user leaves: the terminal stays.
        keep_open(keys);
    })
    .await;
    assert_eq!(ending, Ending::Quit);
    assert!(!dir.path().join("ran").exists());
    assert!(ui.app().prompt().is_none());
}

// Review D I2: the terminal's input ending while an approval waits ends the session, and nothing
// ran.
#[tokio::test]
async fn input_ending_during_an_approval_ends_the_session() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call("b1", "bash", json!({"command": "touch ran"})),
        Script::text("never asked"),
    ]);
    let (notify, mut notified) = mpsc::unbounded_channel();
    let mut ui = start(provider.clone(), dir.path(), Mode::Ask, None, Some(notify));
    let ending = run_while(&mut ui, std::future::pending(), |keys| async move {
        for event in typed("go") {
            keys.unbounded_send(Ok(event)).unwrap();
        }
        approval_asked(&mut notified).await;
        drop(keys);
    })
    .await;
    assert_eq!(ending, Ending::Hangup);
    assert!(!dir.path().join("ran").exists());
    assert!(ui.app().prompt().is_none());
}

// Review C I1: an input error is the terminal going away too.
#[tokio::test]
async fn an_input_error_ends_the_session_as_a_hangup() {
    let dir = tempfile::tempdir().unwrap();
    let mut ui = start(MockProvider::new(vec![]), dir.path(), Mode::Ask, None, None);
    let ending = run_while(&mut ui, std::future::pending(), |keys| async move {
        keys.unbounded_send(Err(std::io::Error::other("input/output error")))
            .unwrap();
        keep_open(keys);
    })
    .await;
    assert_eq!(ending, Ending::Hangup);
}

/// Whether process group `group` still has a member.
fn group_alive(group: i32) -> bool {
    // SAFETY: signal 0 only checks that the group exists; `group` is above 1, never harness's.
    assert!(group > 1);
    unsafe { libc::kill(-group, 0) == 0 }
}

// Review C I1: asked to stop (SIGTERM, or a hangup), the session stops the running command's
// processes and ends, saying how.
#[tokio::test]
async fn a_shutdown_stops_the_running_command_and_ends_the_session() {
    for asked in [Ending::Terminated, Ending::Hangup] {
        let dir = tempfile::tempdir().unwrap();
        let provider = MockProvider::new(vec![
            Script::tool_call(
                "b1",
                "bash",
                json!({"command": "echo $$ > group; sleep 30; touch survived"}),
            ),
            Script::text("never asked"),
        ]);
        let mut ui = start(provider.clone(), dir.path(), Mode::Auto, None, None);
        let (stop, stopped) = oneshot::channel::<()>();
        let shutdown = async move {
            let _ = stopped.await;
            asked
        };
        let group_file = dir.path().join("group");
        let started = Instant::now();
        let ending = run_while(&mut ui, shutdown, |keys| async move {
            for event in typed("go") {
                keys.unbounded_send(Ok(event)).unwrap();
            }
            while std::fs::read_to_string(&group_file)
                .map(|g| !g.ends_with('\n'))
                .unwrap_or(true)
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let _ = stop.send(());
            keep_open(keys);
        })
        .await;
        assert_eq!(ending, asked);
        assert!(started.elapsed() < Duration::from_secs(10));
        let group: i32 = std::fs::read_to_string(dir.path().join("group"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(!group_alive(group), "the command's processes still run");
        assert!(!dir.path().join("survived").exists());
    }
}
