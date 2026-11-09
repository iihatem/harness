//! M3d part 2: a subagent that runs in Claude Code or Codex. The supervisor treats it like any
//! child (a place of its own class, a worktree when it can edit, a report), with a scripted
//! stand-in for the program (`harness_core::delegated::DelegateTurns`) and real git.

mod common;

use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use harness_core::{
    agent::{Agent, AgentConfig, NonInteractive},
    agents::{
        AgentFactory, AgentReport, AgentSpec, AgentStatus, ChildSetup, Endpoint, Limits,
        ParentContext, Supervisor,
    },
    delegated::{ApprovalGate, DelegateTurns, DelegatedOutcome, DelegatedTurn},
    engine::{EngineConfig, PermissionEngine, RuleSet},
    event::{AgentEvent, TurnEndReason},
    meter::AccountKind,
    permission::Mode,
    provider::{Provider, ProviderStream},
    testing::{MockProvider, Script},
    tool::{ToolContext, ToolRegistry},
    worktree::{
        Worktrees,
        testing::{git, init_repo},
    },
};
use tokio::sync::{Notify, mpsc};
use tokio_util::sync::CancellationToken;

const WAIT: Duration = Duration::from_secs(10);

/// What one run of the stand-in program does.
#[derive(Default, Clone)]
struct Step {
    /// Writes this file in the workspace it was started in (an edit by the program).
    writes: Option<(&'static str, &'static str)>,
    /// Waits for this before it ends.
    hold: Option<Arc<Notify>>,
    texts: Vec<&'static str>,
}

#[derive(Default)]
struct Program {
    steps: Mutex<VecDeque<Step>>,
    runs: Mutex<Vec<DelegatedTurn>>,
    running: Mutex<u32>,
    peak: Mutex<u32>,
}

impl Program {
    fn new(steps: Vec<Step>) -> Arc<Program> {
        Arc::new(Program {
            steps: Mutex::new(steps.into()),
            ..Program::default()
        })
    }

    fn runs(&self) -> Vec<DelegatedTurn> {
        self.runs.lock().unwrap().clone()
    }
}

#[async_trait]
impl DelegateTurns for Program {
    fn account(&self) -> AccountKind {
        AccountKind::Delegated
    }

    fn label(&self) -> String {
        "codex".into()
    }

    async fn prepare(&self, _gate: &dyn ApprovalGate) -> Result<(), String> {
        Ok(())
    }

    async fn run(
        &self,
        turn: DelegatedTurn,
        _gate: &dyn ApprovalGate,
        events: &mpsc::UnboundedSender<AgentEvent>,
        cancel: CancellationToken,
    ) -> DelegatedOutcome {
        let workspace = turn.workspace.clone();
        self.runs.lock().unwrap().push(turn);
        {
            let mut running = self.running.lock().unwrap();
            *running += 1;
            let mut peak = self.peak.lock().unwrap();
            *peak = (*peak).max(*running);
        }
        let step = self.steps.lock().unwrap().pop_front().unwrap_or_default();
        if let Some((path, text)) = step.writes {
            std::fs::write(workspace.join(path), text).unwrap();
        }
        for text in &step.texts {
            let _ = events.send(AgentEvent::TextDelta {
                text: (*text).into(),
            });
        }
        if let Some(hold) = &step.hold {
            tokio::select! {
                () = hold.notified() => {}
                () = cancel.cancelled() => {}
            }
        }
        *self.running.lock().unwrap() -= 1;
        let mut outcome = DelegatedOutcome::ended(TurnEndReason::Completed);
        outcome.texts = step.texts.iter().map(|t| t.to_string()).collect();
        outcome.tool_calls = u32::from(step.writes.is_some());
        outcome
    }
}

/// A provider whose model is run by the program.
struct ProgramProvider(Arc<Program>);

impl Provider for ProgramProvider {
    fn stream(&self, _request: harness_core::message::ChatRequest) -> ProviderStream {
        panic!("a delegated model sends no requests");
    }

    fn delegated(&self) -> Option<Arc<dyn DelegateTurns>> {
        Some(self.0.clone())
    }
}

struct Factory {
    main: PathBuf,
    program: Arc<Program>,
    specs: Mutex<Vec<AgentSpec>>,
}

fn is_delegated(spec: &AgentSpec) -> bool {
    spec.model
        .as_deref()
        .is_some_and(harness_core::agents::is_delegated_model)
}

#[async_trait]
impl AgentFactory for Factory {
    fn endpoint(&self, spec: &AgentSpec) -> Endpoint {
        let model = spec.model.as_deref().unwrap_or("mock/m1");
        Endpoint {
            key: model.split('/').next().unwrap().into(),
            local: false,
            delegated: is_delegated(spec),
        }
    }

    async fn build(&self, setup: ChildSetup) -> Result<Agent, String> {
        self.specs.lock().unwrap().push(setup.spec.clone());
        let workspace = setup.spec.workspace.resolve(&self.main);
        let model = setup.spec.model.clone().unwrap_or("mock/m1".into());
        let provider: Arc<dyn Provider> = if is_delegated(&setup.spec) {
            Arc::new(ProgramProvider(self.program.clone()))
        } else {
            MockProvider::new(vec![Script::text("in-process answer")])
        };
        let name = model.split('/').nth(1).unwrap_or("m1").to_string();
        let mode = setup.mode;
        Ok(Agent::new(
            provider,
            ToolRegistry::new(vec![]),
            setup.policy,
            setup.approver,
            AgentConfig::new(model, name, "prompt", workspace.join(".spill")),
            ToolContext::new(&workspace).with_sandbox(None, mode.fs_access()),
        )
        .with_session(setup.session))
    }
}

struct Rig {
    main: tempfile::TempDir,
    _home: tempfile::TempDir,
    factory: Arc<Factory>,
    supervisor: Arc<Supervisor>,
    program: Arc<Program>,
}

fn rig(steps: Vec<Step>, limits: Limits, with_worktrees: bool) -> Rig {
    let main = tempfile::tempdir().unwrap();
    init_repo(main.path());
    let home = tempfile::tempdir().unwrap();
    let program = Program::new(steps);
    let factory = Arc::new(Factory {
        main: main.path().to_path_buf(),
        program: program.clone(),
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
    let (tx, _rx) = mpsc::unbounded_channel();
    let supervisor = Supervisor::new(
        limits,
        factory.clone(),
        ParentContext {
            session: "parent".into(),
            cancel: CancellationToken::new(),
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
        supervisor.set_worktrees(Arc::new(
            Worktrees::new(main.path(), home.path().join("p")).unwrap(),
        ));
    }
    Rig {
        main,
        _home: home,
        factory,
        supervisor,
        program,
    }
}

fn limits() -> Limits {
    Limits {
        per_local: 4,
        per_hosted: 4,
        per_delegated: 2,
        total: 4,
    }
}

fn spec(model: &str, task: &str) -> AgentSpec {
    let mut spec = AgentSpec::new("builder", task);
    spec.model = Some(model.into());
    spec
}

async fn report(rig: &Rig, spec: AgentSpec) -> AgentReport {
    tokio::time::timeout(WAIT, rig.supervisor.spawn(spec).wait())
        .await
        .expect("the child ended")
}

#[tokio::test]
async fn a_delegated_child_that_can_edit_works_in_a_worktree_even_when_its_spec_asks_for_none() {
    let rig = rig(
        vec![Step {
            writes: Some(("src/new.rs", "pub fn x() {}\n")),
            texts: vec!["added it"],
            ..Step::default()
        }],
        limits(),
        true,
    );
    // No `isolation` on the spec: the program is outside harness's sandbox, so harness decides.
    let mut spec = spec("codex/gpt-5", "add a file");
    spec.isolation = false;
    let report = report(&rig, spec).await;
    assert_eq!(report.status, AgentStatus::Done, "{report:?}");
    let outcome = report
        .worktree
        .expect("what it left is committed on a branch");
    assert!(outcome.branch.starts_with("harness/builder"), "{outcome:?}");
    assert_eq!(outcome.files, ["src/new.rs"]);
    // It ran in the worktree: never in the main checkout.
    let runs = rig.program.runs();
    assert_eq!(runs.len(), 1);
    assert_ne!(runs[0].workspace, rig.main.path().canonicalize().unwrap());
    let listed = git(rig.main.path(), &["worktree", "list", "--porcelain"]);
    assert!(
        listed.contains(&*runs[0].workspace.to_string_lossy()),
        "{:?} is not a worktree of the repository: {listed}",
        runs[0].workspace
    );
    assert!(!rig.main.path().join("src/new.rs").exists());
    let shown = git(
        rig.main.path(),
        &["show", &format!("{}:src/new.rs", outcome.branch)],
    );
    assert_eq!(shown, "pub fn x() {}\n");
}

#[tokio::test]
async fn a_delegated_child_in_read_only_mode_reads_the_parents_workspace() {
    let rig = rig(vec![Step::default()], limits(), true);
    let mut spec = spec("codex/gpt-5", "look around");
    spec.mode = Some(Mode::ReadOnly);
    let report = report(&rig, spec).await;
    assert_eq!(report.status, AgentStatus::Done, "{report:?}");
    assert!(report.worktree.is_none());
    let runs = rig.program.runs();
    assert_eq!(
        runs[0].workspace.canonicalize().unwrap(),
        rig.main.path().canonicalize().unwrap()
    );
    assert_eq!(runs[0].mode, Mode::ReadOnly);
    assert_eq!(rig.supervisor.worktrees().unwrap().records().len(), 0);
}

#[tokio::test]
async fn a_delegated_child_that_can_edit_does_not_start_without_a_repository_to_isolate_it_in() {
    let rig = rig(vec![Step::default()], limits(), false);
    let report = report(&rig, spec("claude-code/sonnet", "edit")).await;
    assert_eq!(report.status, AgentStatus::Failed, "{report:?}");
    let error = report.error.expect("it says why");
    assert!(
        error.contains("worktree") && error.contains("git repository"),
        "{error}"
    );
    // The program never ran: it could have edited the checkout.
    assert!(rig.program.runs().is_empty());
}

#[tokio::test]
async fn an_in_process_child_is_not_given_a_worktree_it_did_not_ask_for() {
    let rig = rig(vec![], limits(), true);
    let report = report(&rig, spec("mock/m1", "do it")).await;
    assert_eq!(report.status, AgentStatus::Done, "{report:?}");
    assert!(report.worktree.is_none());
    assert_eq!(rig.supervisor.worktrees().unwrap().records().len(), 0);
}

#[tokio::test]
async fn a_program_runs_at_most_its_limit_of_children_at_once_and_the_rest_wait() {
    let hold = Arc::new(Notify::new());
    let step = Step {
        hold: Some(hold.clone()),
        ..Step::default()
    };
    let rig = rig(
        vec![step.clone(), step.clone(), step],
        Limits {
            per_delegated: 2,
            ..limits()
        },
        true,
    );
    let mut handles = Vec::new();
    for n in 0..3 {
        let mut spec = spec("codex/gpt-5", &format!("look {n}"));
        spec.mode = Some(Mode::ReadOnly);
        handles.push(rig.supervisor.spawn(spec));
    }
    // Two have a place and run; the third is queued behind the program's limit, though the total
    // (4) and the hosted limit (4) would allow it.
    let settled = async {
        loop {
            let info = rig.supervisor.snapshot();
            let working = info
                .iter()
                .filter(|i| i.status == AgentStatus::Working)
                .count();
            let queued = info
                .iter()
                .filter(|i| i.status == AgentStatus::Queued)
                .count();
            if working == 2 && queued == 1 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    tokio::time::timeout(WAIT, settled)
        .await
        .expect("two run and one waits");
    for _ in 0..3 {
        hold.notify_one();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    for handle in handles {
        let report = tokio::time::timeout(WAIT, handle.wait()).await.unwrap();
        assert_eq!(report.status, AgentStatus::Done, "{report:?}");
    }
    assert_eq!(*rig.program.peak.lock().unwrap(), 2);
    let _ = &rig.factory;
}
