//! M3d: the spawn tool and worktrees: `isolation` in a definition or on an entry, read-only
//! agents never isolated, background agents that return at once. Real git, mock models.

mod common;

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use common::{Echo, Put};
use harness_core::{
    agent::{Agent, AgentConfig, NonInteractive},
    agents::{
        AgentDefinition, AgentFactory, AgentSpec, ChildSetup, Definitions, Endpoint, Limits,
        ParentContext, ParentModel, SpawnAgentsTool, SpawnSettings, Supervisor,
    },
    engine::{EngineConfig, PermissionEngine, RuleSet},
    permission::Mode,
    testing::{MockProvider, Script},
    tool::{Tool, ToolContext, ToolRegistry},
    worktree::{
        Worktrees,
        testing::{git, init_repo},
    },
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const WAIT: Duration = Duration::from_secs(10);

struct Factory {
    main: PathBuf,
    scripts: Mutex<std::collections::VecDeque<Vec<Script>>>,
    specs: Mutex<Vec<AgentSpec>>,
}

#[async_trait]
impl AgentFactory for Factory {
    fn endpoint(&self, _spec: &AgentSpec) -> Endpoint {
        Endpoint {
            key: "mock".into(),
            local: false,
            delegated: false,
        }
    }
    async fn build(&self, setup: ChildSetup) -> Result<Agent, String> {
        self.specs.lock().unwrap().push(setup.spec.clone());
        let script = self
            .scripts
            .lock()
            .unwrap()
            .pop_front()
            .ok_or("no script")?;
        let workspace = setup.spec.workspace.resolve(&self.main);
        let mode = setup.mode;
        Ok(Agent::new(
            MockProvider::new(script),
            ToolRegistry::new(vec![Arc::new(Echo), Arc::new(Put)]),
            setup.policy,
            setup.approver,
            AgentConfig::new("mock/m1", "m1", "prompt", workspace.join(".spill")),
            ToolContext::new(&workspace).with_sandbox(None, mode.fs_access()),
        )
        .with_session(setup.session))
    }
}

struct Rig {
    main: tempfile::TempDir,
    _home: tempfile::TempDir,
    factory: Arc<Factory>,
    tool: Arc<SpawnAgentsTool>,
    supervisor: Arc<Supervisor>,
    wts: Arc<Worktrees>,
    cancel: CancellationToken,
}

fn rig(
    defs: Vec<AgentDefinition>,
    scripts: Vec<Vec<Script>>,
    with_worktrees: bool,
    wait_for_background: bool,
) -> Rig {
    let main = tempfile::tempdir().unwrap();
    init_repo(main.path());
    let home = tempfile::tempdir().unwrap();
    let wts = Arc::new(Worktrees::new(main.path(), home.path().join("p")).unwrap());
    let factory = Arc::new(Factory {
        main: main.path().to_path_buf(),
        scripts: Mutex::new(scripts.into()),
        specs: Mutex::new(Vec::new()),
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
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let supervisor = Supervisor::new(
        Limits {
            per_local: 4,
            per_hosted: 4,
            per_delegated: 4,
            total: 4,
        },
        factory.clone(),
        ParentContext {
            session: "parent".into(),
            cancel: cancel.clone(),
            policy: Arc::new(PermissionEngine::new(config.clone())),
            engine: config,
            approver: Arc::new(NonInteractive),
            meter: None,
            sessions_dir: None,
            workspace: main.path().to_path_buf(),
        },
        tx,
    );
    if with_worktrees {
        supervisor.set_worktrees(wts.clone());
    }
    let tool = SpawnAgentsTool::new(
        &supervisor,
        Arc::new(Definitions::new(defs)),
        SpawnSettings {
            max_per_turn: 10,
            background_model: None,
            parent: ParentModel::default(),
            also_in_play: vec![],
            wait_for_background,
        },
    );
    Rig {
        main,
        _home: home,
        factory,
        tool,
        supervisor,
        wts,
        cancel,
    }
}

impl Rig {
    async fn call(&self, args: Value) -> harness_core::tool::ToolOutput {
        let mut ctx = ToolContext::new(self.main.path());
        ctx.cancel = self.cancel.clone();
        ctx.turn = 1;
        tokio::time::timeout(WAIT, self.tool.run(args, &ctx))
            .await
            .expect("the tool returned")
    }

    fn specs(&self) -> Vec<AgentSpec> {
        self.factory.specs.lock().unwrap().clone()
    }
}

fn editing(name: &str) -> AgentDefinition {
    AgentDefinition::new(name, "edits", "You edit.")
}

fn edits(path: &str) -> Vec<Script> {
    vec![
        Script::tool_call("call_h0", "put", json!({"path": path, "content": "x\n"})),
        Script::text("edited"),
    ]
}

#[tokio::test]
async fn a_definition_with_isolation_worktree_works_in_a_worktree_and_the_report_names_the_branch()
{
    let mut def = editing("builder");
    def.isolation = Some("worktree".into());
    let rig = rig(vec![def], vec![edits("src/new.rs")], true, true);
    let out = rig
        .call(json!({"agents": [{"agent": "builder", "task": "add a file"}]}))
        .await;
    assert!(!out.is_error, "{}", out.content);
    assert!(rig.specs()[0].isolation);
    assert!(
        out.content.contains("branch: harness/builder"),
        "{}",
        out.content
    );
    assert!(out.content.contains("src/new.rs"), "{}", out.content);
    assert!(out.content.contains("not applied"), "{}", out.content);
    assert!(!rig.main.path().join("src/new.rs").exists());
    assert_eq!(rig.wts.records().len(), 1);
}

#[tokio::test]
async fn an_entry_can_ask_for_a_worktree_but_cannot_decline_one_a_definition_requires() {
    let mut def = editing("builder");
    def.isolation = Some("worktree".into());
    let rig = rig(
        vec![def, editing("plain")],
        vec![edits("a.txt"), edits("b.txt"), edits("c.txt")],
        true,
        true,
    );
    let out = rig
        .call(json!({"agents": [
            {"agent": "plain", "task": "t", "isolation": "worktree"},
            {"agent": "plain", "task": "t"},
            {"agent": "builder", "task": "t", "isolation": "none"},
        ]}))
        .await;
    assert!(!out.is_error, "{}", out.content);
    let isolated: Vec<bool> = rig.specs().iter().map(|s| s.isolation).collect();
    // `none` does not take away what the definition requires: the entry that asked for one and
    // the `builder` have worktrees, the plain entry that did not has none.
    assert_eq!(isolated.len(), 3);
    assert_eq!(isolated.iter().filter(|i| **i).count(), 2, "{isolated:?}");
    let schema = rig.tool.spec().parameters;
    assert_eq!(
        schema["properties"]["agents"]["items"]["properties"]["isolation"]["enum"],
        json!(["worktree", "none"])
    );
}

#[tokio::test]
async fn a_read_only_agent_never_gets_a_worktree_and_the_model_is_told() {
    let rig = rig(vec![], vec![vec![Script::text("looked")]], true, true);
    let out = rig
        .call(json!({"agents": [{"agent": "explore", "task": "look", "isolation": "worktree"}]}))
        .await;
    assert!(!out.is_error, "{}", out.content);
    assert!(!rig.specs()[0].isolation);
    assert!(out.content.contains("read-only"), "{}", out.content);
    assert!(rig.wts.records().is_empty());
}

#[tokio::test]
async fn isolation_without_a_repository_refuses_the_whole_call() {
    let rig = rig(vec![], vec![edits("a.txt")], false, true);
    let out = rig
        .call(json!({"agents": [{"agent": "general", "task": "t", "isolation": "worktree"}]}))
        .await;
    assert!(out.is_error);
    assert!(out.content.contains("git repository"), "{}", out.content);
    assert!(
        out.content.contains("Nothing was started"),
        "{}",
        out.content
    );
    assert!(rig.specs().is_empty());
}

#[tokio::test]
async fn a_background_agent_is_started_and_the_call_returns_at_once() {
    let mut def = editing("worker");
    def.background = Some(true);
    let rig = rig(vec![def], vec![vec![Script::Hang(vec![])]], true, false);
    let out = rig
        .call(json!({"agents": [{"agent": "worker", "task": "long job"}]}))
        .await;
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("in the background"), "{}", out.content);
    assert!(out.content.contains("a1"), "{}", out.content);
    let info = rig.supervisor.snapshot().remove(0);
    assert!(info.background);
    assert_eq!(rig.supervisor.live_background(), 1);
    // An editing background agent works in a worktree.
    let deadline = tokio::time::Instant::now() + WAIT;
    while rig.specs().is_empty() {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(rig.specs()[0].isolation && rig.specs()[0].background);
    rig.supervisor.stop_all();
}

#[tokio::test]
async fn a_headless_run_waits_for_a_background_agent() {
    let mut def = editing("worker");
    def.background = Some(true);
    let rig = rig(vec![def], vec![edits("a.txt")], true, true);
    let out = rig
        .call(json!({"agents": [{"agent": "worker", "task": "job"}]}))
        .await;
    assert!(
        out.content.contains("branch: harness/worker"),
        "{}",
        out.content
    );
    assert!(
        !out.content.contains("in the background"),
        "{}",
        out.content
    );
    git(
        rig.main.path(),
        &["rev-parse", "--verify", &rig.wts.records()[0].branch],
    );
}

#[tokio::test]
async fn a_read_only_background_agent_runs_in_the_background_without_a_worktree() {
    let mut def = AgentDefinition::new("watcher", "reads", "You read.");
    def.mode = Some(Mode::ReadOnly);
    def.background = Some(true);
    let rig = rig(vec![def], vec![vec![Script::Hang(vec![])]], true, false);
    let out = rig
        .call(json!({"agents": [{"agent": "watcher", "task": "watch"}]}))
        .await;
    assert!(out.content.contains("in the background"), "{}", out.content);
    let deadline = tokio::time::Instant::now() + WAIT;
    while rig.specs().is_empty() {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!rig.specs()[0].isolation);
    assert!(rig.specs()[0].background);
    rig.supervisor.stop_all();
}
