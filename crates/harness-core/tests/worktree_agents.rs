//! M3d: a child that asks for isolation works in a worktree harness makes for it; what it
//! changes is committed on its branch; a background child outlives the turn. Real git, mock
//! models.

mod common;

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use common::{Echo, Put, Sh};
use harness_core::{
    agent::{Agent, AgentConfig, NonInteractive},
    agents::{
        AgentEnvelope, AgentFactory, AgentReport, AgentSpec, AgentStatus, ChildSetup, Endpoint,
        Limits, ParentContext, Supervisor,
    },
    engine::{EngineConfig, PermissionEngine, RuleSet},
    event::AgentEvent,
    permission::Mode,
    testing::{MockProvider, Script},
    tool::{ToolContext, ToolRegistry},
    worktree::{
        Worktrees,
        testing::{git, init_repo, write},
    },
};
use serde_json::json;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const WAIT: Duration = Duration::from_secs(10);

struct Factory {
    main: PathBuf,
    scripts: Mutex<std::collections::VecDeque<Vec<Script>>>,
    built: Mutex<Vec<AgentSpec>>,
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
        self.built.lock().unwrap().push(setup.spec.clone());
        let script = self
            .scripts
            .lock()
            .unwrap()
            .pop_front()
            .ok_or("no script left")?;
        let workspace = setup.spec.workspace.resolve(&self.main);
        let config = AgentConfig::new("mock/m1", "m1", "prompt", workspace.join(".spill-out"));
        let tools = ToolRegistry::new(vec![Arc::new(Echo), Arc::new(Put), Arc::new(Sh)]);
        let mode = setup.mode;
        Ok(Agent::new(
            MockProvider::new(script),
            tools,
            setup.policy,
            setup.approver,
            config,
            ToolContext::new(&workspace).with_sandbox(None, mode.fs_access()),
        )
        .with_session(setup.session))
    }
}

struct Fixture {
    main: tempfile::TempDir,
    _home: tempfile::TempDir,
    wts: Arc<Worktrees>,
    factory: Arc<Factory>,
    sup: Arc<Supervisor>,
    rx: mpsc::UnboundedReceiver<AgentEnvelope>,
    cancel: CancellationToken,
}

fn fixture(scripts: Vec<Vec<Script>>, with_worktrees: bool) -> Fixture {
    let main = tempfile::tempdir().unwrap();
    init_repo(main.path());
    let home = tempfile::tempdir().unwrap();
    let wts = Arc::new(Worktrees::new(main.path(), home.path().join("p")).unwrap());
    let factory = Arc::new(Factory {
        main: main.path().to_path_buf(),
        scripts: Mutex::new(scripts.into()),
        built: Mutex::new(Vec::new()),
    });
    let config = EngineConfig {
        mode: Mode::Auto,
        workspace: main.path().to_path_buf(),
        read_dirs: vec![],
        rules: RuleSet::default(),
        sandbox_available: true,
        writes_need_approval: false,
    };
    let cancel = CancellationToken::new();
    let ctx = ParentContext {
        session: "parent".into(),
        cancel: cancel.clone(),
        policy: Arc::new(PermissionEngine::new(config.clone())),
        engine: config,
        approver: Arc::new(NonInteractive),
        meter: None,
        sessions_dir: None,
        workspace: main.path().to_path_buf(),
    };
    let (tx, rx) = mpsc::unbounded_channel();
    let sup = Supervisor::new(Limits::default(), factory.clone(), ctx, tx);
    if with_worktrees {
        sup.set_worktrees(wts.clone());
    }
    Fixture {
        main,
        _home: home,
        wts,
        factory,
        sup,
        rx,
        cancel,
    }
}

fn edits(path: &str, content: &str) -> Vec<Script> {
    vec![
        Script::tool_call("call_h0", "put", json!({"path": path, "content": content})),
        Script::text("edited"),
    ]
}

fn isolated(label: &str) -> AgentSpec {
    let mut spec = AgentSpec::new(label, format!("do the {label} task"));
    spec.isolation = true;
    spec
}

async fn wait(handle: harness_core::agents::AgentHandle) -> AgentReport {
    tokio::time::timeout(WAIT, handle.wait())
        .await
        .expect("the child ended")
}

fn branches(dir: &Path) -> String {
    git(dir, &["branch", "--list", "harness/*"])
}

#[tokio::test]
async fn an_isolated_child_edits_its_worktree_and_its_result_is_committed_on_a_branch() {
    let mut f = fixture(vec![edits("src/new.rs", "pub fn two() {}\n")], true);
    let report = wait(f.sup.spawn(isolated("general"))).await;
    assert_eq!(report.status, AgentStatus::Done, "{:?}", report.error);
    // The main checkout was not touched.
    assert!(!f.main.path().join("src/new.rs").exists());
    assert_eq!(git(f.main.path(), &["status", "--porcelain"]).trim(), "");
    let outcome = report.worktree.clone().expect("it changed something");
    assert!(
        outcome.branch.starts_with("harness/general"),
        "{}",
        outcome.branch
    );
    assert_eq!(outcome.files, vec!["src/new.rs".to_string()]);
    assert!(branches(f.main.path()).contains(&outcome.branch));
    let shown = git(
        f.main.path(),
        &["show", &format!("{}:src/new.rs", outcome.branch)],
    );
    assert_eq!(shown, "pub fn two() {}\n");
    // The factory was told to build it in the worktree.
    let built = f.factory.built.lock().unwrap().clone();
    let workspace = built[0].workspace.resolve(f.main.path());
    assert!(
        workspace.starts_with(f.wts.home()),
        "{}",
        workspace.display()
    );
    // The record, the info and the finishing event all say so, and the lock is gone.
    let record = f.wts.records().remove(0);
    assert_eq!(record.status, AgentStatus::Done);
    assert_eq!(record.outcome.as_ref(), Some(&outcome));
    assert!(record.session.is_some());
    assert_eq!(record.summary.as_deref(), Some("edited"));
    let info = f.sup.snapshot().remove(0);
    assert_eq!(info.worktree.as_ref().unwrap().branch, outcome.branch);
    assert_eq!(
        info.worktree.as_ref().unwrap().outcome.as_ref(),
        Some(&outcome)
    );
    let mut finished = None;
    while let Ok(e) = f.rx.try_recv() {
        if let AgentEvent::AgentFinished { branch, .. } = e.event {
            finished = branch;
        }
    }
    assert_eq!(finished, Some(outcome.branch.clone()));
    assert!(!git(f.main.path(), &["worktree", "list", "--porcelain"]).contains("locked"));
}

#[tokio::test]
async fn the_worktree_is_locked_while_the_child_runs() {
    let hold = vec![Script::Hang(vec![])];
    let f = fixture(vec![hold], true);
    let handle = f.sup.spawn(isolated("general"));
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let porcelain = git(f.main.path(), &["worktree", "list", "--porcelain"]);
        if porcelain.contains("locked harness pid") {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "never locked");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    while f.wts.records().is_empty() {
        assert!(tokio::time::Instant::now() < deadline, "never recorded");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(f.wts.records()[0].status, AgentStatus::Working);
    f.cancel.cancel();
    let report = wait(handle).await;
    assert_eq!(report.status, AgentStatus::Cancelled);
    assert!(!git(f.main.path(), &["worktree", "list", "--porcelain"]).contains("locked"));
}

#[tokio::test]
async fn a_child_that_committed_itself_leaves_a_result_and_its_worktree_and_branch() {
    let f = fixture(
        vec![vec![
            Script::tool_call(
                "call_h0",
                "sh",
                json!({"command": "echo 'pub fn two() {}' > two.rs && git add -A && git commit -q -m mine"}),
            ),
            Script::text("committed"),
        ]],
        true,
    );
    let report = wait(f.sup.spawn(isolated("general"))).await;
    assert_eq!(report.status, AgentStatus::Done, "{:?}", report.error);
    let outcome = report
        .worktree
        .clone()
        .expect("commits it made itself are its result");
    assert_eq!(outcome.files, vec!["two.rs".to_string()]);
    assert!(branches(f.main.path()).contains(&outcome.branch));
    let record = f.wts.records().remove(0);
    assert_eq!(record.outcome.as_ref(), Some(&outcome));
    assert!(record.path.exists());
}

#[tokio::test]
async fn a_child_that_changed_nothing_leaves_no_worktree_and_no_branch() {
    let f = fixture(vec![vec![Script::text("nothing to do")]], true);
    let report = wait(f.sup.spawn(isolated("general"))).await;
    assert_eq!(report.status, AgentStatus::Done);
    assert!(report.worktree.is_none());
    assert!(branches(f.main.path()).trim().is_empty());
    assert!(f.wts.records().is_empty());
    assert!(!f.wts.home().join(&report.label).exists());
}

#[tokio::test]
async fn a_child_that_cannot_be_built_leaves_nothing_behind() {
    let f = fixture(vec![], true);
    let report = wait(f.sup.spawn(isolated("general"))).await;
    assert_eq!(report.status, AgentStatus::Failed);
    assert!(branches(f.main.path()).trim().is_empty());
    assert!(f.wts.records().is_empty());
}

#[tokio::test]
async fn a_read_only_child_never_gets_a_worktree() {
    let f = fixture(vec![vec![Script::text("looked")]], true);
    let mut spec = isolated("explore");
    spec.mode = Some(Mode::ReadOnly);
    let report = wait(f.sup.spawn(spec)).await;
    assert_eq!(report.status, AgentStatus::Done);
    let built = f.factory.built.lock().unwrap().clone();
    assert_eq!(
        built[0].workspace.resolve(f.main.path()),
        f.main.path().to_path_buf()
    );
    assert!(f.wts.records().is_empty());
}

#[tokio::test]
async fn without_worktrees_an_isolated_child_fails_and_says_why() {
    let f = fixture(vec![edits("a.txt", "x")], false);
    let report = wait(f.sup.spawn(isolated("general"))).await;
    assert_eq!(report.status, AgentStatus::Failed);
    assert!(report.error.unwrap().contains("git repository"));
    assert!(
        f.factory.built.lock().unwrap().is_empty(),
        "nothing was built"
    );
    assert!(!f.main.path().join("a.txt").exists());
}

#[tokio::test]
async fn a_cancelled_child_keeps_what_it_changed_for_review() {
    let script = vec![
        Script::tool_call("call_h0", "put", json!({"path": "a.txt", "content": "x\n"})),
        Script::Hang(vec![]),
    ];
    let f = fixture(vec![script], true);
    let handle = f.sup.spawn(isolated("general"));
    let deadline = tokio::time::Instant::now() + WAIT;
    while f
        .wts
        .records()
        .first()
        .is_none_or(|r| !r.path.join("a.txt").exists())
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the edit never happened"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    f.cancel.cancel();
    let report = wait(handle).await;
    assert_eq!(report.status, AgentStatus::Cancelled);
    let outcome = report.worktree.expect("its edit is committed");
    assert_eq!(outcome.files, vec!["a.txt".to_string()]);
    assert_eq!(f.wts.records()[0].status, AgentStatus::Cancelled);
}

#[tokio::test]
async fn the_child_is_told_it_works_in_a_worktree_and_does_not_commit() {
    let f = fixture(vec![vec![Script::text("ok")]], true);
    wait(f.sup.spawn(isolated("general"))).await;
    let built = f.factory.built.lock().unwrap().clone();
    let said = built[0].instructions.clone().unwrap_or_default();
    assert!(said.contains("worktree"), "{said}");
    assert!(said.contains("harness/"), "{said}");
    assert!(
        said.contains("do not commit") || said.contains("Do not commit"),
        "{said}"
    );
    assert!(said.contains("HARNESS_PORT_OFFSET"), "{said}");
}

#[tokio::test]
async fn work_not_committed_in_the_main_checkout_is_said_once() {
    let mut f = fixture(vec![vec![Script::text("a")], vec![Script::text("b")]], true);
    write(f.main.path(), "README.md", "edited, not committed\n");
    wait(f.sup.spawn(isolated("one"))).await;
    wait(f.sup.spawn(isolated("two"))).await;
    let notices = f.sup.take_notices();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(
        notices[0].contains("not committed") || notices[0].contains("uncommitted"),
        "{notices:?}"
    );
    assert!(f.sup.take_notices().is_empty(), "taken");
    let mut warned = 0;
    while let Ok(e) = f.rx.try_recv() {
        if e.agent.is_none() && matches!(e.event, AgentEvent::Warning { .. }) {
            warned += 1;
        }
    }
    assert_eq!(warned, 1);
}

#[tokio::test]
async fn two_isolated_children_work_in_separate_worktrees() {
    let f = fixture(vec![edits("one.txt", "1\n"), edits("two.txt", "2\n")], true);
    let (a, b) = (
        f.sup.spawn(isolated("general")),
        f.sup.spawn(isolated("general")),
    );
    let (a, b) = (wait(a).await, wait(b).await);
    let (a, b) = (a.worktree.unwrap(), b.worktree.unwrap());
    assert_ne!(a.branch, b.branch);
    assert_eq!(f.wts.records().len(), 2);
    let offsets: Vec<u16> = f.wts.records().iter().map(|r| r.port_offset).collect();
    assert_ne!(offsets[0], offsets[1]);
}

#[tokio::test]
async fn a_background_child_survives_the_foreground_turn_and_stops_only_when_told() {
    let f = fixture(vec![vec![Script::Hang(vec![])]], true);
    let mut spec = isolated("general");
    spec.background = true;
    let handle = f.sup.spawn(spec);
    let deadline = tokio::time::Instant::now() + WAIT;
    while f.sup.snapshot()[0].status != AgentStatus::Working {
        assert!(tokio::time::Instant::now() < deadline, "never started");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(f.sup.snapshot()[0].background);
    // The turn ends, or is interrupted: the foreground's agents stop, this one does not.
    f.cancel.cancel();
    f.sup.cancel_all();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(f.sup.snapshot()[0].status, AgentStatus::Working);
    assert_eq!(f.sup.live_background(), 1);
    f.sup.stop_all();
    let report = wait(handle).await;
    assert_eq!(report.status, AgentStatus::Cancelled);
    assert_eq!(f.sup.live_background(), 0);
}
