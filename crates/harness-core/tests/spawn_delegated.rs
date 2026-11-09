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
    agent::{
        Agent, AgentConfig, ApprovalDecision, ApprovalKind, ApprovalRequest, Approver,
        NonInteractive,
    },
    agents::{
        AgentFactory, AgentReport, AgentSpec, AgentStatus, ChildSetup, Endpoint, Limits,
        ParentContext, Supervisor,
    },
    delegated::{ApprovalGate, DelegateTurns, DelegatedOutcome, DelegatedTurn},
    engine::{EngineConfig, PermissionEngine, RuleSet},
    event::{AgentEvent, TurnEndReason},
    message::Usage,
    meter::{AccountKind, Avoided, BudgetStatus, Meter, RequestCost, RequestRecord},
    permission::{Mode, PermissionPolicy},
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
    tool_calls: u32,
    commands: Vec<&'static str>,
    changed: Vec<&'static str>,
    usage: Usage,
    /// Asks this plain question through the gate, and records the answer.
    ask: Option<ApprovalKind>,
}

#[derive(Default)]
struct Program {
    steps: Mutex<VecDeque<Step>>,
    runs: Mutex<Vec<DelegatedTurn>>,
    running: Mutex<u32>,
    peak: Mutex<u32>,
    answers: Mutex<Vec<Option<bool>>>,
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
        gate: &dyn ApprovalGate,
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
        if let Some(kind) = step.ask {
            let answer = gate.confirm(kind, "may it?".into()).await;
            self.answers.lock().unwrap().push(answer);
        }
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
        outcome.tool_calls = step.tool_calls + u32::from(step.writes.is_some());
        outcome.commands = step.commands.iter().map(|c| c.to_string()).collect();
        outcome.changed = step.changed.iter().map(|c| c.to_string()).collect();
        outcome.usage = step.usage;
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
    /// The model a spec that names none runs on (the parent's, which `/model` can change).
    parent_model: Mutex<String>,
    /// `endpoint` says "not a program" whatever the model is, as a stale answer would.
    stale_endpoint: std::sync::atomic::AtomicBool,
    /// Runs inside `build`, for a change that happens while a child is being built.
    during_build: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl Factory {
    fn model_of(&self, spec: &AgentSpec) -> String {
        spec.model
            .clone()
            .unwrap_or_else(|| self.parent_model.lock().unwrap().clone())
    }
}

fn is_delegated(model: &str) -> bool {
    harness_core::agents::is_delegated_model(model)
}

#[async_trait]
impl AgentFactory for Factory {
    fn endpoint(&self, spec: &AgentSpec) -> Endpoint {
        let model = self.model_of(spec);
        Endpoint {
            key: model.split('/').next().unwrap().into(),
            local: false,
            delegated: is_delegated(&model)
                && !self
                    .stale_endpoint
                    .load(std::sync::atomic::Ordering::SeqCst),
        }
    }

    async fn build(&self, setup: ChildSetup) -> Result<Agent, String> {
        self.specs.lock().unwrap().push(setup.spec.clone());
        let workspace = setup.spec.workspace.resolve(&self.main);
        if let Some(change) = self.during_build.lock().unwrap().take() {
            change();
        }
        let model = self.model_of(&setup.spec);
        let provider: Arc<dyn Provider> = if is_delegated(&model) {
            Arc::new(ProgramProvider(self.program.clone()))
        } else {
            MockProvider::new(vec![Script::text("in-process answer")])
        };
        let name = model.split('/').nth(1).unwrap_or("m1").to_string();
        let mode = setup.mode;
        let mut config = AgentConfig::new(model, name, "prompt", workspace.join(".spill"));
        // What `CliFactory` does: the definition's text goes to the program with the task.
        config.brief = setup.spec.instructions.clone();
        config.instructions = Some("PROJECT-INSTRUCTIONS".into());
        let mut agent = Agent::new(
            provider,
            ToolRegistry::new(vec![]),
            setup.policy,
            setup.approver,
            config,
            ToolContext::new(&workspace).with_sandbox(None, mode.fs_access()),
        )
        .with_session(setup.session);
        if let Some(meter) = setup.meter {
            agent = agent.with_meter(meter);
        }
        Ok(agent)
    }
}

struct Rig {
    main: tempfile::TempDir,
    _home: tempfile::TempDir,
    factory: Arc<Factory>,
    parent_policy: Arc<PermissionEngine>,
    supervisor: Arc<Supervisor>,
    program: Arc<Program>,
}

fn rig(steps: Vec<Step>, limits: Limits, with_worktrees: bool) -> Rig {
    rig_with(
        steps,
        limits,
        with_worktrees,
        Arc::new(NonInteractive),
        None,
    )
}

fn rig_with(
    steps: Vec<Step>,
    limits: Limits,
    with_worktrees: bool,
    approver: Arc<dyn Approver>,
    meter: Option<Arc<dyn Meter>>,
) -> Rig {
    let main = tempfile::tempdir().unwrap();
    init_repo(main.path());
    let home = tempfile::tempdir().unwrap();
    let program = Program::new(steps);
    let factory = Arc::new(Factory {
        main: main.path().to_path_buf(),
        program: program.clone(),
        specs: Mutex::new(Vec::new()),
        parent_model: Mutex::new("mock/m1".into()),
        stale_endpoint: std::sync::atomic::AtomicBool::new(false),
        during_build: Mutex::new(None),
    });
    let config = EngineConfig {
        mode: Mode::Auto,
        workspace: main.path().to_path_buf(),
        read_dirs: vec![],
        rules: RuleSet::default(),
        sandbox_available: true,
        writes_need_approval: false,
    };
    let parent_policy = Arc::new(PermissionEngine::new(config.clone()));
    let (tx, _rx) = mpsc::unbounded_channel();
    let supervisor = Supervisor::new(
        limits,
        factory.clone(),
        ParentContext {
            session: "parent".into(),
            cancel: CancellationToken::new(),
            policy: parent_policy.clone(),
            engine: config,
            approver,
            meter,
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
        parent_policy,
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

async fn report_of(rig: &Rig, spec: AgentSpec) -> AgentReport {
    report(rig, spec).await
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

/// Where the program ran, and whether that is a worktree of the repository (never the checkout).
fn ran_in_a_worktree(rig: &Rig, at: usize) -> bool {
    let runs = rig.program.runs();
    // A worktree that changed nothing is removed when the child ends: its path no longer resolves.
    let at = &runs[at].workspace;
    at.canonicalize().unwrap_or_else(|_| at.clone()) != rig.main.path().canonicalize().unwrap()
}

#[tokio::test]
async fn a_child_whose_own_mode_is_plan_or_read_only_is_never_isolated() {
    for mode in [Mode::Plan, Mode::ReadOnly] {
        let rig = rig(vec![Step::default()], limits(), true);
        let mut spec = spec("codex/gpt-5", "look around");
        spec.mode = Some(mode);
        let report = report(&rig, spec).await;
        assert_eq!(report.status, AgentStatus::Done, "{mode:?}: {report:?}");
        assert!(!ran_in_a_worktree(&rig, 0), "{mode:?}");
        assert_eq!(rig.program.runs()[0].mode, mode);
        assert_eq!(rig.supervisor.worktrees().unwrap().records().len(), 0);
    }
}

#[tokio::test]
async fn a_child_with_no_mode_of_its_own_gets_a_worktree_though_its_parent_is_in_plan_when_it_starts()
 {
    // The parent's mode can widen while the child runs, so what the parent is in at the start
    // decides nothing.
    let rig = rig(vec![Step::default()], limits(), true);
    rig.parent_policy.set_mode(Mode::Plan);
    let policy = rig.parent_policy.clone();
    // The parent widens while the child is being built, and the program starts after.
    *rig.factory.during_build.lock().unwrap() = Some(Box::new(move || policy.set_mode(Mode::Auto)));
    let report = report(&rig, spec("codex/gpt-5", "edit")).await;
    assert_eq!(report.status, AgentStatus::Done, "{report:?}");
    assert!(ran_in_a_worktree(&rig, 0), "it ran in the checkout");
}

#[tokio::test]
async fn a_child_whose_inherited_model_became_a_program_while_it_waited_is_isolated() {
    let hold = Arc::new(Notify::new());
    let rig = rig(
        vec![
            Step {
                hold: Some(hold.clone()),
                ..Step::default()
            },
            Step::default(),
        ],
        Limits {
            total: 1,
            ..limits()
        },
        true,
    );
    let mut first = spec("codex/gpt-5", "occupy the place");
    first.mode = Some(Mode::ReadOnly);
    let first = rig.supervisor.spawn(first);
    // Queued behind it, naming no model: the parent's, which is not a program yet.
    let second = rig.supervisor.spawn(AgentSpec::new("builder", "edit"));
    tokio::time::sleep(Duration::from_millis(100)).await;
    *rig.factory.parent_model.lock().unwrap() = "codex/gpt-5".into();
    hold.notify_one();
    let report = tokio::time::timeout(WAIT, second.wait()).await.unwrap();
    first.wait().await;
    assert_eq!(report.status, AgentStatus::Done, "{report:?}");
    let runs = rig.program.runs();
    assert_eq!(runs.len(), 2, "both ran on the program");
    assert!(ran_in_a_worktree(&rig, 1), "it ran in the checkout");
}

#[tokio::test]
async fn a_background_child_on_the_parents_program_model_works_in_a_worktree() {
    // What `/bg` makes: the general agent, the parent's model, in a worktree, in the background.
    let rig = rig(vec![Step::default()], limits(), true);
    *rig.factory.parent_model.lock().unwrap() = "claude-code/sonnet".into();
    let mut spec = AgentSpec::new("general", "edit in the background");
    spec.isolation = true;
    spec.background = true;
    let report = report(&rig, spec).await;
    assert_eq!(report.status, AgentStatus::Done, "{report:?}");
    assert!(ran_in_a_worktree(&rig, 0), "it ran in the checkout");
}

#[tokio::test]
async fn a_child_built_on_a_program_the_decision_did_not_expect_is_refused_before_the_program_starts()
 {
    // `endpoint` said it is not a program (a stale answer), and the built child is one: with no
    // worktree and nothing fixing its mode, it must not start.
    let rig = rig(vec![Step::default()], limits(), true);
    rig.factory
        .stale_endpoint
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let report = report(&rig, spec("codex/gpt-5", "edit")).await;
    assert_eq!(report.status, AgentStatus::Failed, "{report:?}");
    let error = report.error.expect("it says why");
    assert!(error.contains("worktree"), "{error}");
    assert!(rig.program.runs().is_empty(), "the program started");
    // One whose own mode is fixed read-only is fine in the checkout.
    let mut read = spec("codex/gpt-5", "look");
    read.mode = Some(Mode::ReadOnly);
    let report = report_of(&rig, read).await;
    assert_eq!(report.status, AgentStatus::Done, "{report:?}");
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

// ---- what the program is told, and what its report says --------------------------------------

#[tokio::test]
async fn the_program_is_given_the_definitions_text_the_project_instructions_and_the_task_only() {
    let rig = rig(vec![Step::default()], limits(), true);
    let mut spec = spec("codex/gpt-5", "TASK-TEXT: fix the parser");
    spec.mode = Some(Mode::ReadOnly);
    spec.instructions = Some("BODY-TEXT: you review code".into());
    report(&rig, spec).await;
    let runs = rig.program.runs();
    let task = &runs[0].task;
    assert!(task.contains("BODY-TEXT: you review code"), "{task}");
    assert!(task.contains("TASK-TEXT: fix the parser"), "{task}");
    assert!(
        task.find("BODY-TEXT").unwrap() < task.find("TASK-TEXT").unwrap(),
        "{task}"
    );
    // The project's instructions keep their own route (a file for Claude, AGENTS.md for Codex).
    assert_eq!(
        runs[0].instructions.as_deref(),
        Some("PROJECT-INSTRUCTIONS")
    );
    // Nothing of any conversation: a fresh child has no earlier messages to summarise.
    assert!(!task.contains("earlier conversation"), "{task}");
}

#[tokio::test]
async fn the_report_is_made_from_the_programs_tool_events_and_flags_a_done_without_any() {
    let rig = rig(
        vec![
            Step {
                tool_calls: 3,
                commands: vec!["cargo test"],
                changed: vec!["src/new.rs"],
                texts: vec!["did it"],
                ..Step::default()
            },
            Step {
                texts: vec!["trust me"],
                ..Step::default()
            },
        ],
        limits(),
        true,
    );
    let mut first = spec("codex/gpt-5", "do it");
    first.mode = Some(Mode::ReadOnly);
    let a = report(&rig, first).await;
    assert_eq!(a.tool_calls, 3, "{a:?}");
    assert_eq!(a.commands_run, ["cargo test"]);
    assert_eq!(a.files_changed, ["src/new.rs"]);
    assert!(a.flags.is_empty(), "{a:?}");
    let mut second = spec("codex/gpt-5", "say it");
    second.mode = Some(Mode::ReadOnly);
    let b = report(&rig, second).await;
    assert_eq!(b.tool_calls, 0);
    assert!(
        b.flags
            .contains(&harness_core::agents::ReportFlag::NoToolCalls),
        "{b:?}"
    );
}

#[tokio::test]
async fn a_file_the_program_changed_in_its_worktree_is_listed_once() {
    let rig = rig(
        vec![Step {
            writes: Some(("src/new.rs", "x\n")),
            tool_calls: 1,
            changed: vec!["src/new.rs"],
            ..Step::default()
        }],
        limits(),
        true,
    );
    let report = report(&rig, spec("codex/gpt-5", "add")).await;
    assert_eq!(report.files_changed, ["src/new.rs"], "{report:?}");
}

struct Billed;

impl Meter for Billed {
    fn record_request(&self, _r: &RequestRecord) -> RequestCost {
        RequestCost {
            account: AccountKind::ApiKey,
            billed_usd: Some(0.5),
            list_usd: Some(0.5),
            avoided: Avoided::NotApplicable,
        }
    }
    fn check_budget(&self, _s: &str, _a: AccountKind) -> BudgetStatus {
        BudgetStatus::default()
    }
}

#[tokio::test]
async fn what_the_program_used_is_in_the_childs_report() {
    let rig = rig_with(
        vec![Step {
            usage: Usage {
                input_tokens: 100,
                output_tokens: 20,
                ..Usage::default()
            },
            ..Step::default()
        }],
        limits(),
        true,
        Arc::new(NonInteractive),
        Some(Arc::new(Billed)),
    );
    let mut spec = spec("codex/gpt-5", "look");
    spec.mode = Some(Mode::ReadOnly);
    let usage = report(&rig, spec).await.usage.expect("it used something");
    assert_eq!(
        (usage.requests, usage.input_tokens, usage.output_tokens),
        (1, 100, 20)
    );
    assert_eq!(usage.billed_usd, Some(0.5));
}

// ---- questions a child puts -------------------------------------------------------------------

/// Answers a question with `decision`; records who asked.
struct Answers {
    decision: ApprovalDecision,
    asked: Mutex<Vec<ApprovalRequest>>,
}

#[async_trait]
impl Approver for Answers {
    async fn decide(&self, request: &ApprovalRequest) -> ApprovalDecision {
        self.asked.lock().unwrap().push(request.clone());
        self.decision.clone()
    }
}

#[tokio::test]
async fn a_childs_question_reaches_the_queue_labelled_with_the_child() {
    let approver = Arc::new(Answers {
        decision: ApprovalDecision::Approve,
        asked: Mutex::new(Vec::new()),
    });
    let rig = rig_with(
        vec![Step {
            ask: Some(ApprovalKind::DelegateOptIn),
            ..Step::default()
        }],
        limits(),
        true,
        approver.clone(),
        None,
    );
    let mut spec = spec("claude-code/sonnet", "look");
    spec.mode = Some(Mode::ReadOnly);
    let report = report(&rig, spec).await;
    assert_eq!(report.status, AgentStatus::Done, "{report:?}");
    let asked = approver.asked.lock().unwrap();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].kind, ApprovalKind::DelegateOptIn);
    assert_eq!(asked[0].agent.as_ref().unwrap().label, "builder");
    assert_eq!(rig.program.answers.lock().unwrap().as_slice(), [Some(true)]);
}

#[tokio::test]
async fn a_question_nobody_can_answer_does_not_fail_the_child_the_program_decides() {
    // Headless: the program's own rule (the API key, or an error saying how to opt in) applies.
    let rig = rig(
        vec![Step {
            ask: Some(ApprovalKind::DelegateOptIn),
            ..Step::default()
        }],
        limits(),
        true,
    );
    let mut spec = spec("claude-code/sonnet", "look");
    spec.mode = Some(Mode::ReadOnly);
    let report = report(&rig, spec).await;
    assert_eq!(report.status, AgentStatus::Done, "{report:?}");
    assert_eq!(rig.program.answers.lock().unwrap().as_slice(), [None]);
}

#[tokio::test]
async fn esc_on_the_opt_in_leaves_it_undecided_and_fails_that_child_with_the_reason() {
    let approver = Arc::new(Answers {
        decision: ApprovalDecision::Deny {
            feedback: Some(harness_core::agent::STOPPED_BY_USER.into()),
        },
        asked: Mutex::new(Vec::new()),
    });
    let rig = rig_with(
        vec![Step {
            ask: Some(ApprovalKind::DelegateOptIn),
            ..Step::default()
        }],
        limits(),
        true,
        approver,
        None,
    );
    let mut spec = spec("claude-code/sonnet", "look");
    spec.mode = Some(Mode::ReadOnly);
    let report = report(&rig, spec).await;
    assert_eq!(report.status, AgentStatus::Failed, "{report:?}");
    let error = report.error.unwrap();
    assert!(
        error.contains("subscription") && error.contains("unanswered"),
        "{error}"
    );
}

#[tokio::test]
async fn esc_on_a_childs_full_access_confirmation_fails_that_child_with_the_reason() {
    let approver = Arc::new(Answers {
        decision: ApprovalDecision::Deny {
            feedback: Some(harness_core::agent::STOPPED_BY_USER.into()),
        },
        asked: Mutex::new(Vec::new()),
    });
    let rig = rig_with(
        vec![Step {
            ask: Some(ApprovalKind::DelegateFullAccess),
            ..Step::default()
        }],
        limits(),
        true,
        approver,
        None,
    );
    let mut spec = spec("codex/gpt-5", "look");
    spec.mode = Some(Mode::ReadOnly);
    let report = report(&rig, spec).await;
    assert_eq!(report.status, AgentStatus::Failed, "{report:?}");
    let error = report.error.expect("it says why");
    assert!(
        error.contains("full-access") && error.contains("unanswered"),
        "{error}"
    );
}
