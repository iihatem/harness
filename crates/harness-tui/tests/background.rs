//! M3d: background agents. `/bg` starts one in its own worktree and it outlives the turn; the
//! panel says what each is doing; an approval it needs waits in the queue and is never answered
//! for the user; the terminal is told when one needs input or ends; quitting asks what to do
//! with the ones still running; and what finished is kept for review.

mod common;

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use common::*;
use harness_core::{
    agent::{Agent, AgentConfig, NonInteractive},
    agents::{
        AgentFactory, AgentSpec, AgentStatus, ChildSetup, Definitions, Endpoint, Limits,
        ParentContext, Supervisor,
    },
    engine::{EngineConfig, PermissionEngine, RuleSet},
    permission::Mode,
    testing::{MockProvider, Script},
    tool::{ToolContext, ToolRegistry},
    worktree::{
        Worktrees,
        testing::{git, init_repo},
    },
};
use harness_tui::{
    app::{Host, Prepared},
    approval::ChannelApprover,
    inline::InlineTerminal,
    notify::Notify,
    ui::{Flow, Ui},
};
use ratatui::{
    backend::TestBackend,
    crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers},
};
use serde_json::json;
use tokio_util::sync::CancellationToken;

struct NoCommands;

impl Host for NoCommands {
    fn is_command(&self, _name: &str) -> bool {
        false
    }
    fn prepare(&mut self, _typed: &str) -> Prepared {
        unreachable!()
    }
}

/// Builds each child from the next script, in the workspace the supervisor chose for it, with
/// the real file tools.
struct Factory {
    main: std::path::PathBuf,
    scripts: std::sync::Mutex<std::collections::VecDeque<Vec<Script>>>,
}

#[async_trait]
impl AgentFactory for Factory {
    fn endpoint(&self, _spec: &AgentSpec) -> Endpoint {
        Endpoint {
            key: "mock".into(),
            local: false,
        }
    }
    async fn build(&self, setup: ChildSetup) -> Result<Agent, String> {
        let script = self
            .scripts
            .lock()
            .unwrap()
            .pop_front()
            .ok_or("no script")?;
        let workspace = setup.spec.workspace.resolve(&self.main);
        let tools = ["read", "write"]
            .into_iter()
            .map(|n| harness_tools::builtin().get(n).unwrap())
            .collect::<Vec<_>>();
        let mode = setup.mode;
        Ok(Agent::new(
            MockProvider::new(script),
            ToolRegistry::new(tools),
            setup.policy,
            setup.approver,
            AgentConfig::new("mock/m1", "m1", "prompt", workspace.join(".spill-out")),
            ToolContext::new(&workspace).with_sandbox(None, mode.fs_access()),
        )
        .with_session(setup.session))
    }
}

/// Keeps what it was asked to send to the terminal.
#[derive(Clone, Default)]
struct Recording(Arc<std::sync::Mutex<Vec<String>>>);

impl Notify for Recording {
    fn notify(&mut self, text: &str) -> std::io::Result<()> {
        self.0.lock().unwrap().push(text.to_string());
        Ok(())
    }
}

struct Rig {
    told: Recording,
    main: tempfile::TempDir,
    _home: tempfile::TempDir,
    _dir: tempfile::TempDir,
    wts: Arc<Worktrees>,
    sup: Arc<Supervisor>,
    ui: Ui<TestBackend>,
}

/// A session in `mode` over a git repository, with one child script per `/bg`.
fn rig(mode: Mode, scripts: Vec<Vec<Script>>, with_worktrees: bool) -> Rig {
    let main = tempfile::tempdir().unwrap();
    init_repo(main.path());
    let home = tempfile::tempdir().unwrap();
    let wts = Arc::new(Worktrees::new(main.path(), home.path().join("p")).unwrap());
    let dir = tempfile::tempdir().unwrap();
    let parent = agent(MockProvider::new(vec![]), dir.path(), mode);
    let (approver, requests) = ChannelApprover::new();
    let term = InlineTerminal::new(TestBackend::new(100, 24), 0)
        .unwrap()
        .with_alt_screen(Box::new(harness_tui::testing::TestAltScreen::new().0));
    let told = Recording::default();
    let mut opts = options(dir.path(), mode);
    opts.notifier = Some(Box::new(told.clone()));
    let mut ui = Ui::start(parent, Box::new(NoCommands), term, opts, requests);
    ui.draw().unwrap();
    let config = EngineConfig {
        mode,
        workspace: main.path().to_path_buf(),
        read_dirs: vec![],
        rules: RuleSet::default(),
        sandbox_available: true,
        writes_need_approval: mode == Mode::Ask,
    };
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let sup = Supervisor::new(
        Limits::default(),
        Arc::new(Factory {
            main: main.path().to_path_buf(),
            scripts: std::sync::Mutex::new(scripts.into()),
        }),
        ParentContext {
            session: "s".into(),
            cancel: CancellationToken::new(),
            policy: Arc::new(PermissionEngine::new(config.clone())),
            engine: config,
            approver,
            meter: None,
            sessions_dir: None,
            workspace: main.path().to_path_buf(),
        },
        tx,
    );
    if with_worktrees {
        sup.set_worktrees(wts.clone());
    }
    ui.watch_agents(sup.clone());
    ui.app_mut()
        .set_agent_definitions(Definitions::new(Vec::new()));
    Rig {
        told,
        main,
        _home: home,
        _dir: dir,
        wts,
        sup,
        ui,
    }
}

fn key(ui: &mut Ui<TestBackend>, code: KeyCode, modifiers: KeyModifiers) -> Flow {
    ui.handle(Event::Key(KeyEvent::new(code, modifiers)))
        .unwrap()
}

fn writes(path: &str) -> Vec<Script> {
    vec![
        Script::tool_call("c1", "write", json!({"path": path, "content": "x\n"})),
        Script::text("wrote it"),
    ]
}

async fn until_background(rig: &mut Rig, what: &str, done: impl Fn(&Rig) -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !done(rig) {
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        let _ = tokio::time::timeout(Duration::from_millis(50), rig.ui.next()).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn bg_starts_an_agent_in_a_worktree_and_the_panel_shows_it() {
    let mut rig = rig(Mode::Auto, vec![vec![Script::Hang(vec![])]], true);
    send(&mut rig.ui, "/bg fix the parser");
    until_background(&mut rig, "the agent started", |r| {
        r.sup
            .snapshot()
            .first()
            .is_some_and(|i| i.status == AgentStatus::Working)
    })
    .await;
    until_seen(&mut rig, 1).await;
    rig.ui.draw().unwrap();
    assert!(
        shows_wrapped(&rig.ui, "in the background"),
        "{:?}",
        everything(&rig.ui)
    );
    assert!(shows_wrapped(&rig.ui, "/results"));
    let panel = screen(&rig.ui).join("\n");
    assert!(panel.contains("a1 general"), "{panel}");
    assert!(panel.contains("working"), "{panel}");
    assert!(panel.contains("background"), "{panel}");
    assert!(rig.sup.snapshot()[0].background);
    until_background(&mut rig, "its worktree", |r| r.wts.records().len() == 1).await;
    rig.sup.stop_all();
}

#[tokio::test(flavor = "multi_thread")]
async fn bg_says_what_it_needs_instead_of_starting_wrongly() {
    // No task.
    let mut r = rig(Mode::Auto, vec![], true);
    send(&mut r.ui, "/bg");
    assert!(
        shows_wrapped(&r.ui, "takes the task"),
        "{:?}",
        everything(&r.ui)
    );
    // A mode in which an agent cannot edit.
    let mut r = rig(Mode::Plan, vec![], true);
    send(&mut r.ui, "/bg do things");
    assert!(shows_wrapped(&r.ui, "plan"), "{:?}", everything(&r.ui));
    assert!(r.sup.snapshot().is_empty());
    // No repository.
    let mut r = rig(Mode::Auto, vec![], false);
    send(&mut r.ui, "/bg do things");
    assert!(
        shows_wrapped(&r.ui, "git repository"),
        "{:?}",
        everything(&r.ui)
    );
    assert!(r.sup.snapshot().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_approval_a_background_agent_needs_waits_in_the_queue_and_is_never_answered_for_the_user()
 {
    let mut rig = rig(Mode::Ask, vec![writes("a.txt")], true);
    send(&mut rig.ui, "/bg write a file");
    until(&mut rig.ui, |app| app.pending_approvals() == 1).await;
    // It is labelled with the agent, the agent needs input, and the terminal was told.
    let screen_text = screen(&rig.ui).join("\n");
    assert!(screen_text.contains("a1"), "{screen_text}");
    let told = rig.told.0.lock().unwrap().clone();
    assert!(
        told.iter()
            .any(|n| n.contains("approval needed") && n.contains("a1 (general)")),
        "{told:?}"
    );
    until_background(&mut rig, "needs input", |r| {
        r.sup.snapshot()[0].status == AgentStatus::NeedsInput
    })
    .await;
    until(&mut rig.ui, |app| {
        app.children()
            .iter()
            .any(|c| c.status == AgentStatus::NeedsInput)
    })
    .await;
    rig.ui.draw().unwrap();
    let shown = screen(&rig.ui).join("\n");
    // The approval is on the screen, labelled with the agent that asks.
    assert!(shown.contains("a1 (general) approve?"), "{shown}");
    // Time passes: nobody answers for the user.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let _ = tokio::time::timeout(Duration::from_millis(50), rig.ui.next()).await;
    assert_eq!(rig.ui.app().pending_approvals(), 1);
    assert_eq!(rig.sup.snapshot()[0].status, AgentStatus::NeedsInput);
    // The user answers, and it goes on to the end.
    until_armed(&rig.ui).await;
    press(&mut rig.ui, KeyCode::Char('y'));
    until_background(&mut rig, "it finished", |r| {
        r.sup.snapshot()[0].status.is_final()
    })
    .await;
    assert_eq!(rig.sup.snapshot()[0].status, AgentStatus::Done);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_finished_background_agent_is_announced_and_waits_for_review() {
    let mut rig = rig(Mode::Auto, vec![writes("a.txt")], true);
    send(&mut rig.ui, "/bg write a file");
    until_background(&mut rig, "ready for review", |r| {
        r.ui.app().results().iter().any(|x| x.outcome.is_some())
    })
    .await;
    rig.ui.draw().unwrap();
    let told = rig.told.0.lock().unwrap().clone();
    assert!(
        told.iter()
            .any(|n| n.contains("a1") && n.contains("finished") && n.contains("review")),
        "{told:?}"
    );
    let panel = screen(&rig.ui).join("\n");
    assert!(panel.contains("ready for review"), "{panel}");
    // The main checkout is untouched; the branch has the file.
    assert!(!rig.main.path().join("a.txt").exists());
    let branch = rig.wts.records()[0].branch.clone();
    assert_eq!(
        git(rig.main.path(), &["show", &format!("{branch}:a.txt")]),
        "x\n"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_background_agent_that_failed_is_announced_and_stays_on_the_panel_until_seen() {
    // A script that runs out: the model request fails.
    let mut rig = rig(Mode::Auto, vec![vec![]], true);
    send(&mut rig.ui, "/bg do it");
    until_background(&mut rig, "failed", |r| {
        r.sup
            .snapshot()
            .first()
            .is_some_and(|i| i.status == AgentStatus::Failed)
    })
    .await;
    rig.ui.draw().unwrap();
    let told = rig.told.0.lock().unwrap().clone();
    assert!(
        told.iter()
            .any(|n| n.contains("a1") && n.contains("failed")),
        "{told:?}"
    );
    assert!(screen(&rig.ui).join("\n").contains("failed"));
    // Looking at /agents is seeing it.
    send(&mut rig.ui, "/agents");
    assert!(
        everything(&rig.ui)
            .iter()
            .any(|r| r.contains("a1") && r.contains("failed"))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn quitting_with_a_background_agent_running_asks_and_stopping_ends_it() {
    let mut rig = rig(Mode::Auto, vec![vec![Script::Hang(vec![])]], true);
    send(&mut rig.ui, "/bg long job");
    until_background(&mut rig, "working", rig_working).await;
    until_seen(&mut rig, 1).await;
    let flow = key(&mut rig.ui, KeyCode::Char('d'), KeyModifiers::CONTROL);
    assert_eq!(flow, Flow::Continue, "it asks first");
    assert!(
        shows_wrapped(&rig.ui, "still running"),
        "{:?}",
        screen(&rig.ui)
    );
    until_armed(&rig.ui).await;
    type_text(&mut rig.ui, "stop");
    let flow = key(&mut rig.ui, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(flow, Flow::Quit);
    rig.ui.finish().await.unwrap();
    until_status(&rig, AgentStatus::Cancelled).await;
    assert_eq!(rig.sup.live_background(), 0);
}

/// Takes in the supervisor's changes until the session shows as many live background agents.
async fn until_seen(rig: &mut Rig, live: usize) {
    until(&mut rig.ui, |app| app.live_background() == live).await;
}

fn rig_working(r: &Rig) -> bool {
    r.sup
        .snapshot()
        .first()
        .is_some_and(|i| i.status == AgentStatus::Working)
}

async fn until_status(rig: &Rig, status: AgentStatus) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while rig.sup.snapshot()[0].status != status {
        assert!(tokio::time::Instant::now() < deadline, "never {status:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn waiting_keeps_harness_open_until_the_agents_end_and_then_quits() {
    let mut rig = rig(Mode::Auto, vec![vec![Script::Hang(vec![])]], true);
    send(&mut rig.ui, "/bg long job");
    until_background(&mut rig, "working", rig_working).await;
    until_seen(&mut rig, 1).await;
    key(&mut rig.ui, KeyCode::Char('d'), KeyModifiers::CONTROL);
    until_armed(&rig.ui).await;
    type_text(&mut rig.ui, "wait");
    assert_eq!(
        key(&mut rig.ui, KeyCode::Enter, KeyModifiers::NONE),
        Flow::Continue
    );
    assert_eq!(rig.sup.live_background(), 1, "still running");
    assert!(
        shows_wrapped(&rig.ui, "waiting for"),
        "{:?}",
        screen(&rig.ui)
    );
    // The agent ends (here, stopped from outside): harness leaves.
    rig.sup.stop_all();
    let flow = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if rig.ui.next().await.unwrap() == Flow::Quit {
                break Flow::Quit;
            }
        }
    })
    .await
    .expect("it quit");
    assert_eq!(flow, Flow::Quit);
}

#[tokio::test(flavor = "multi_thread")]
async fn keep_working_leaves_everything_as_it_was_and_no_agents_means_no_question() {
    let mut rig = rig(Mode::Auto, vec![vec![Script::Hang(vec![])]], true);
    send(&mut rig.ui, "/bg long job");
    until_background(&mut rig, "working", rig_working).await;
    until_seen(&mut rig, 1).await;
    key(&mut rig.ui, KeyCode::Char('d'), KeyModifiers::CONTROL);
    until_armed(&rig.ui).await;
    type_text(&mut rig.ui, "keep");
    assert_eq!(
        key(&mut rig.ui, KeyCode::Enter, KeyModifiers::NONE),
        Flow::Continue
    );
    assert!(rig.ui.app().picker().is_none());
    assert_eq!(rig.sup.live_background(), 1);
    rig.sup.stop_all();
    // With none running, Ctrl+D leaves at once.
    let mut idle = crate::rig(Mode::Auto, vec![], true);
    assert_eq!(
        key(&mut idle.ui, KeyCode::Char('d'), KeyModifiers::CONTROL),
        Flow::Quit
    );
}

#[allow(dead_code)]
fn unused(_: NonInteractive) {}
