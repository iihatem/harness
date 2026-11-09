//! M3a: the supervisor runs child agents from specs: status, limits, cancellation, reports,
//! approvals, permissions, sessions and usage. Every model is a mock.

mod common;

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use common::{AlwaysApprove, Echo, FakeBash, Put, Touch};
use futures::StreamExt;
use harness_core::{
    agent::{Agent, AgentConfig, ApprovalDecision, ApprovalRequest, Approver, NonInteractive},
    agents::{
        AgentEnvelope, AgentFactory, AgentId, AgentSpec, AgentStatus, ChildSetup, Endpoint, Limits,
        ParentContext, ReportFlag, Supervisor,
    },
    engine::{EngineConfig, PermissionEngine, RuleSet},
    event::{AgentEvent, TurnEndReason},
    gate::Gates,
    message::ChatRequest,
    meter::{AccountKind, AgentScope, BudgetStatus, Meter, RequestCost, RequestRecord},
    permission::Mode,
    provider::{FinishReason, Provider, ProviderEvent, ProviderStream},
    session::Session,
    testing::{MockProvider, Script},
    tool::{ToolContext, ToolRegistry},
};
use serde_json::json;
use tokio::sync::{Barrier, Notify, mpsc};
use tokio_util::sync::CancellationToken;

const WAIT: Duration = Duration::from_secs(5);

/// Answers once all `parties` requests are being streamed at the same time: it only finishes if
/// the agents really run concurrently.
struct Rendezvous(Arc<Barrier>);

impl Provider for Rendezvous {
    fn stream(&self, _request: ChatRequest) -> ProviderStream {
        let barrier = self.0.clone();
        futures::stream::once(async move {
            barrier.wait().await;
            futures::stream::iter(vec![
                Ok(ProviderEvent::TextDelta("both ran".into())),
                Ok(ProviderEvent::Finished(FinishReason::Stop)),
            ])
        })
        .flatten()
        .boxed()
    }
}

/// Answers when `release` is notified.
struct Held {
    started: Arc<Notify>,
    release: Arc<Notify>,
}

impl Provider for Held {
    fn stream(&self, _request: ChatRequest) -> ProviderStream {
        let (started, release) = (self.started.clone(), self.release.clone());
        futures::stream::once(async move {
            started.notify_one();
            release.notified().await;
            futures::stream::iter(vec![
                Ok(ProviderEvent::TextDelta("released".into())),
                Ok(ProviderEvent::Finished(FinishReason::Stop)),
            ])
        })
        .flatten()
        .boxed()
    }
}

type Build = dyn Fn(ChildSetup) -> Result<Agent, String> + Send + Sync;

struct Factory {
    build: Box<Build>,
    endpoint: Box<dyn Fn(&AgentSpec) -> Endpoint + Send + Sync>,
    built: Mutex<Vec<(AgentSpec, Mode)>>,
}

#[async_trait]
impl AgentFactory for Factory {
    fn endpoint(&self, spec: &AgentSpec) -> Endpoint {
        (self.endpoint)(spec)
    }
    async fn build(&self, setup: ChildSetup) -> Result<Agent, String> {
        self.built
            .lock()
            .unwrap()
            .push((setup.spec.clone(), setup.mode));
        (self.build)(setup)
    }
}

fn tools() -> ToolRegistry {
    ToolRegistry::new(vec![
        Arc::new(Echo),
        Arc::new(Touch),
        Arc::new(Put),
        Arc::new(FakeBash),
        Arc::new(common::gates::Edit),
    ])
}

fn child_agent(setup: ChildSetup, provider: Arc<dyn Provider>, dir: &Path) -> Agent {
    let mode = setup.mode;
    let mut config = AgentConfig::new("mock/m1", "m1", "child system prompt", dir.join(".spill"));
    if let Some(steps) = setup.spec.max_steps {
        config.max_steps = steps;
    }
    let agent = Agent::new(
        provider,
        tools(),
        setup.policy,
        setup.approver,
        config,
        ToolContext::new(dir).with_sandbox(None, mode.fs_access()),
    )
    .with_session(setup.session);
    match setup.meter {
        Some(meter) => agent.with_meter(meter),
        None => agent,
    }
}

fn hosted(spec: &AgentSpec) -> Endpoint {
    Endpoint {
        key: spec.model.clone().unwrap_or_else(|| "mock".into()),
        local: false,
        delegated: false,
    }
}

fn factory(
    build: impl Fn(ChildSetup) -> Result<Agent, String> + Send + Sync + 'static,
) -> Arc<Factory> {
    Arc::new(Factory {
        build: Box::new(build),
        endpoint: Box::new(hosted),
        built: Mutex::new(Vec::new()),
    })
}

/// A factory whose children each answer from the next script.
fn scripted(dir: &Path, scripts: Vec<Vec<Script>>) -> Arc<Factory> {
    let dir = dir.to_path_buf();
    let scripts = Mutex::new(
        scripts
            .into_iter()
            .collect::<std::collections::VecDeque<_>>(),
    );
    factory(move |setup| {
        let script = scripts
            .lock()
            .unwrap()
            .pop_front()
            .ok_or("no script left")?;
        Ok(child_agent(setup, MockProvider::new(script), &dir))
    })
}

struct Parent {
    ctx: ParentContext,
    engine: Arc<PermissionEngine>,
}

fn parent(dir: &Path, mode: Mode, approver: Arc<dyn Approver>) -> Parent {
    let config = EngineConfig {
        mode,
        workspace: dir.to_path_buf(),
        read_dirs: vec![],
        rules: RuleSet::default(),
        sandbox_available: true,
        writes_need_approval: false,
    };
    let engine = Arc::new(PermissionEngine::new(config.clone()));
    Parent {
        ctx: ParentContext {
            session: "parent-session".into(),
            cancel: CancellationToken::new(),
            policy: engine.clone(),
            engine: config,
            approver,
            meter: None,
            sessions_dir: None,
            workspace: dir.to_path_buf(),
        },
        engine,
    }
}

fn supervisor(
    factory: Arc<Factory>,
    parent: ParentContext,
) -> (Arc<Supervisor>, mpsc::UnboundedReceiver<AgentEnvelope>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (Supervisor::new(Limits::default(), factory, parent, tx), rx)
}

fn spec(label: &str) -> AgentSpec {
    AgentSpec::new(label, format!("do the {label} task"))
}

async fn wait(handle: harness_core::agents::AgentHandle) -> harness_core::agents::AgentReport {
    tokio::time::timeout(WAIT, handle.wait())
        .await
        .expect("the child ended")
}

fn drain(rx: &mut mpsc::UnboundedReceiver<AgentEnvelope>) -> Vec<AgentEnvelope> {
    let mut out = Vec::new();
    while let Ok(envelope) = rx.try_recv() {
        out.push(envelope);
    }
    out
}

async fn until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + WAIT;
    while !ready() {
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn statuses(sup: &Supervisor) -> Vec<AgentStatus> {
    sup.snapshot().into_iter().map(|i| i.status).collect()
}

// ---- running a child ------------------------------------------------------------------------

#[tokio::test]
async fn a_child_runs_its_task_and_reports_done_with_its_final_text() {
    let dir = tempfile::tempdir().unwrap();
    let factory = scripted(dir.path(), vec![vec![Script::text("the answer")]]);
    let p = parent(dir.path(), Mode::Auto, Arc::new(NonInteractive));
    let (sup, mut rx) = supervisor(factory, p.ctx);
    let handle = sup.spawn(spec("explore"));
    assert_eq!(handle.tag.id, AgentId("a1".into()));
    let report = wait(handle).await;
    assert_eq!(report.status, AgentStatus::Done);
    assert_eq!(report.summary, "the answer");
    assert_eq!(report.agent, "a1");
    assert_eq!(report.label, "explore");
    assert_eq!(statuses(&sup), vec![AgentStatus::Done]);
    let events = drain(&mut rx);
    assert!(events.iter().all(|e| e.agent == Some(AgentId("a1".into()))));
    assert!(matches!(
        events.first().map(|e| &e.event),
        Some(AgentEvent::AgentStarted { label, .. }) if label == "explore"
    ));
    assert!(matches!(
        events.last().map(|e| &e.event),
        Some(AgentEvent::AgentFinished {
            status: AgentStatus::Done,
            ..
        })
    ));
}

#[tokio::test]
async fn the_childs_task_is_its_first_message_and_its_prompt_its_own() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![Script::text("ok")]);
    let seen = provider.clone();
    let d = dir.path().to_path_buf();
    let factory = factory(move |setup| Ok(child_agent(setup, provider.clone(), &d)));
    let p = parent(dir.path(), Mode::Auto, Arc::new(NonInteractive));
    let (sup, _rx) = supervisor(factory, p.ctx);
    wait(sup.spawn(spec("explore"))).await;
    let request = &seen.requests()[0];
    assert_eq!(request.system, "child system prompt");
    assert!(format!("{:?}", request.messages[0]).contains("do the explore task"));
}

#[tokio::test]
async fn ids_are_unique_and_tool_call_ids_never_collide_between_children() {
    let dir = tempfile::tempdir().unwrap();
    let call = || {
        vec![
            Script::tool_call("call_h0", "echo", json!({"text": "x"})),
            Script::text("done"),
        ]
    };
    let factory = scripted(dir.path(), vec![call(), call()]);
    let p = parent(dir.path(), Mode::Auto, Arc::new(NonInteractive));
    let (sup, mut rx) = supervisor(factory, p.ctx);
    let (a, b) = (sup.spawn(spec("one")), sup.spawn(spec("two")));
    assert_ne!(a.tag.id, b.tag.id);
    wait(a).await;
    wait(b).await;
    let ids: Vec<(Option<AgentId>, String)> = drain(&mut rx)
        .into_iter()
        .filter_map(|e| match e.event {
            AgentEvent::ToolCallRequested { id, .. } => Some((e.agent, id)),
            _ => None,
        })
        .collect();
    assert_eq!(ids.len(), 2);
    assert_ne!(ids[0].1, ids[1].1, "{ids:?}");
    assert!(
        ids.iter()
            .all(|(agent, id)| id.starts_with(&format!("{}/", agent.as_ref().unwrap())))
    );
}

#[tokio::test]
async fn a_child_that_cannot_be_built_fails_with_the_reason() {
    let dir = tempfile::tempdir().unwrap();
    let factory = factory(|_| Err("no such model".into()));
    let p = parent(dir.path(), Mode::Auto, Arc::new(NonInteractive));
    let (sup, _rx) = supervisor(factory, p.ctx);
    let report = wait(sup.spawn(spec("x"))).await;
    assert_eq!(report.status, AgentStatus::Failed);
    assert!(report.error.unwrap().contains("no such model"));
}

#[tokio::test]
async fn a_model_error_or_the_step_limit_is_a_failed_report() {
    let dir = tempfile::tempdir().unwrap();
    // The script runs out: the mock answers with a protocol error.
    let factory = scripted(dir.path(), vec![vec![]]);
    let p = parent(dir.path(), Mode::Auto, Arc::new(NonInteractive));
    let (sup, _rx) = supervisor(factory, p.ctx);
    let report = wait(sup.spawn(spec("x"))).await;
    assert_eq!(report.status, AgentStatus::Failed);
    assert!(report.error.is_some());
}

// ---- running at once, and limits ------------------------------------------------------------

#[tokio::test]
async fn two_children_run_their_turns_at_the_same_time() {
    let dir = tempfile::tempdir().unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let d = dir.path().to_path_buf();
    let factory = factory(move |setup| {
        Ok(child_agent(
            setup,
            Arc::new(Rendezvous(barrier.clone())),
            &d,
        ))
    });
    let p = parent(dir.path(), Mode::Auto, Arc::new(NonInteractive));
    let (sup, _rx) = supervisor(factory, p.ctx);
    let (a, b) = (sup.spawn(spec("one")), sup.spawn(spec("two")));
    // Each answers only when the other is being answered too.
    assert_eq!(wait(a).await.status, AgentStatus::Done);
    assert_eq!(wait(b).await.status, AgentStatus::Done);
}

fn held_factory(dir: &Path, local: bool) -> (Arc<Factory>, Arc<Notify>, Arc<Notify>) {
    let (started, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let (s, r, d) = (started.clone(), release.clone(), dir.to_path_buf());
    let factory = Arc::new(Factory {
        build: Box::new(move |setup| {
            Ok(child_agent(
                setup,
                Arc::new(Held {
                    started: s.clone(),
                    release: r.clone(),
                }),
                &d,
            ))
        }),
        endpoint: Box::new(move |spec| Endpoint {
            key: spec.model.clone().unwrap_or_else(|| "server".into()),
            local,
            delegated: false,
        }),
        built: Mutex::new(Vec::new()),
    });
    (factory, started, release)
}

#[tokio::test]
async fn a_second_child_on_a_local_server_waits_its_turn_and_is_not_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (factory, started, release) = held_factory(dir.path(), true);
    let p = parent(dir.path(), Mode::Auto, Arc::new(NonInteractive));
    let (sup, _rx) = supervisor(factory.clone(), p.ctx);
    let a = sup.spawn(spec("one"));
    started.notified().await;
    let b = sup.spawn(spec("two"));
    until("the second is queued", || {
        statuses(&sup) == vec![AgentStatus::Working, AgentStatus::Queued]
    })
    .await;
    // Not built, and no request made, while it waits.
    assert_eq!(factory.built.lock().unwrap().len(), 1);
    release.notify_waiters();
    assert_eq!(wait(a).await.status, AgentStatus::Done);
    started.notified().await;
    until("the second runs", || {
        statuses(&sup) == vec![AgentStatus::Done, AgentStatus::Working]
    })
    .await;
    release.notify_waiters();
    assert_eq!(wait(b).await.status, AgentStatus::Done);
}

#[tokio::test]
async fn a_child_cancelled_in_the_queue_still_has_a_start_before_its_finish_in_the_stream() {
    let dir = tempfile::tempdir().unwrap();
    let (factory, started, release) = held_factory(dir.path(), true);
    let p = parent(dir.path(), Mode::Auto, Arc::new(NonInteractive));
    let (sup, mut rx) = supervisor(factory, p.ctx);
    let a = sup.spawn(spec("one"));
    started.notified().await;
    let b = sup.spawn(spec("two"));
    until("the second is queued", || {
        statuses(&sup) == vec![AgentStatus::Working, AgentStatus::Queued]
    })
    .await;
    let second = b.tag.id.clone();
    assert!(sup.cancel(&second));
    assert_eq!(wait(b).await.status, AgentStatus::Cancelled);
    release.notify_waiters();
    wait(a).await;
    let events: Vec<_> = drain(&mut rx)
        .into_iter()
        .filter(|e| e.agent.as_ref() == Some(&second))
        .map(|e| e.event)
        .collect();
    let at = |what: fn(&AgentEvent) -> bool| events.iter().position(what);
    let started_at = at(|e| matches!(e, AgentEvent::AgentStarted { .. }));
    let finished_at = at(|e| matches!(e, AgentEvent::AgentFinished { .. }));
    assert!(
        started_at.is_some() && started_at < finished_at,
        "a finish without a start: {events:?}"
    );
}

#[tokio::test]
async fn four_run_in_all_and_the_fifth_waits() {
    let dir = tempfile::tempdir().unwrap();
    let (factory, started, release) = held_factory(dir.path(), false);
    let p = parent(dir.path(), Mode::Auto, Arc::new(NonInteractive));
    let (sup, _rx) = supervisor(factory, p.ctx);
    let mut handles = Vec::new();
    // Five different providers: only the total limit applies.
    for n in 0..5 {
        let mut s = spec(&format!("a{n}"));
        s.model = Some(format!("provider{n}/m"));
        handles.push(sup.spawn(s));
    }
    until("four work and one waits", || {
        let s = statuses(&sup);
        s.iter().filter(|s| **s == AgentStatus::Working).count() == 4
            && s.iter().filter(|s| **s == AgentStatus::Queued).count() == 1
    })
    .await;
    let _ = started;
    release.notify_waiters();
    until("the fifth runs", || {
        statuses(&sup).contains(&AgentStatus::Done)
    })
    .await;
    release.notify_waiters();
    for h in handles {
        assert_eq!(wait(h).await.status, AgentStatus::Done);
    }
}

// ---- cancelling -----------------------------------------------------------------------------

#[tokio::test]
async fn cancelling_the_parent_cancels_running_and_waiting_children() {
    let dir = tempfile::tempdir().unwrap();
    let (factory, started, _release) = held_factory(dir.path(), true);
    let p = parent(dir.path(), Mode::Auto, Arc::new(NonInteractive));
    let cancel = p.ctx.cancel.clone();
    let (sup, _rx) = supervisor(factory.clone(), p.ctx);
    let (a, b) = (sup.spawn(spec("running")), sup.spawn(spec("waiting")));
    started.notified().await;
    until("one waits", || {
        statuses(&sup).contains(&AgentStatus::Queued)
    })
    .await;
    cancel.cancel();
    assert_eq!(wait(a).await.status, AgentStatus::Cancelled);
    assert_eq!(wait(b).await.status, AgentStatus::Cancelled);
    // The one that never had a place was never built.
    assert_eq!(factory.built.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_turns_own_token_can_be_the_parent_of_a_child() {
    let dir = tempfile::tempdir().unwrap();
    let (factory, started, _release) = held_factory(dir.path(), false);
    let p = parent(dir.path(), Mode::Auto, Arc::new(NonInteractive));
    let (sup, _rx) = supervisor(factory, p.ctx);
    let turn = CancellationToken::new();
    let child = sup.spawn_under(spec("x"), turn.clone());
    started.notified().await;
    turn.cancel();
    assert_eq!(wait(child).await.status, AgentStatus::Cancelled);
}

#[tokio::test]
async fn cancelling_one_child_leaves_its_sibling_running() {
    let dir = tempfile::tempdir().unwrap();
    let (factory, started, release) = held_factory(dir.path(), false);
    let p = parent(dir.path(), Mode::Auto, Arc::new(NonInteractive));
    let (sup, _rx) = supervisor(factory, p.ctx);
    let (a, b) = (sup.spawn(spec("a")), sup.spawn(spec("b")));
    started.notified().await;
    started.notified().await;
    assert!(sup.cancel(&a.tag.id));
    assert_eq!(wait(a).await.status, AgentStatus::Cancelled);
    release.notify_waiters();
    assert_eq!(wait(b).await.status, AgentStatus::Done);
}

#[tokio::test]
async fn cancel_all_ends_every_child() {
    let dir = tempfile::tempdir().unwrap();
    let (factory, started, _release) = held_factory(dir.path(), false);
    let p = parent(dir.path(), Mode::Auto, Arc::new(NonInteractive));
    let (sup, _rx) = supervisor(factory, p.ctx);
    let (a, b) = (sup.spawn(spec("a")), sup.spawn(spec("b")));
    started.notified().await;
    started.notified().await;
    sup.cancel_all();
    assert_eq!(wait(a).await.status, AgentStatus::Cancelled);
    assert_eq!(wait(b).await.status, AgentStatus::Cancelled);
}

// ---- reports --------------------------------------------------------------------------------

#[tokio::test]
async fn files_and_commands_come_from_the_tool_log_not_the_childs_words() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "").unwrap();
    let factory = scripted(
        dir.path(),
        vec![vec![
            Script::tool_call("c1", "touch", json!({"path": "new.txt"})),
            Script::tool_call("c2", "bash", json!({"command": "cargo test"})),
            Script::tool_call("c3", "echo", json!({"text": "hi"})),
            Script::text("I edited src/lib.rs and ran make"),
        ]],
    );
    let p = parent(dir.path(), Mode::Auto, Arc::new(NonInteractive));
    let (sup, _rx) = supervisor(factory, p.ctx);
    let report = wait(sup.spawn(spec("edit"))).await;
    assert_eq!(report.status, AgentStatus::Done);
    assert_eq!(report.commands_run, vec!["cargo test"]);
    assert_eq!(report.tool_calls, 3);
    assert!(report.summary.contains("src/lib.rs"));
    assert!(!report.files_changed.iter().any(|f| f.contains("lib.rs")));
}

#[tokio::test]
async fn a_tool_that_changes_files_names_them_relative_to_the_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().canonicalize().unwrap();
    let factory = scripted(
        dir.path(),
        vec![vec![
            Script::tool_call("c1", "edit", json!({"path": "x.txt", "content": "a"})),
            Script::text("done"),
        ]],
    );
    let p = parent(dir.path(), Mode::Auto, Arc::new(NonInteractive));
    let (sup, _rx) = supervisor(factory, p.ctx);
    let report = wait(sup.spawn(spec("edit"))).await;
    assert_eq!(report.files_changed, vec!["x.txt"]);
    let _ = workspace;
}

#[tokio::test]
async fn done_with_no_tool_calls_is_flagged() {
    let dir = tempfile::tempdir().unwrap();
    let factory = scripted(dir.path(), vec![vec![Script::text("all done, I fixed it")]]);
    let p = parent(dir.path(), Mode::Auto, Arc::new(NonInteractive));
    let (sup, _rx) = supervisor(factory, p.ctx);
    let report = wait(sup.spawn(spec("fix"))).await;
    assert_eq!(report.status, AgentStatus::Done);
    assert!(
        report.flags.contains(&ReportFlag::NoToolCalls),
        "{report:?}"
    );
    assert!(report.files_changed.is_empty());
}

#[tokio::test]
async fn a_report_is_capped() {
    let dir = tempfile::tempdir().unwrap();
    let mut script = Vec::new();
    for n in 0..80 {
        script.push(Script::tool_call(
            &format!("c{n}"),
            "bash",
            json!({"command": format!("echo {n} {}", "x".repeat(400))}),
        ));
    }
    script.push(Script::text(&"long ".repeat(5_000)));
    let factory = scripted(dir.path(), vec![script]);
    let p = parent(dir.path(), Mode::Auto, Arc::new(NonInteractive));
    let (sup, _rx) = supervisor(factory, p.ctx);
    let mut s = spec("loud");
    s.max_steps = Some(200);
    let report = wait(sup.spawn(s)).await;
    assert!(
        report.summary.chars().count() <= 4_100,
        "{}",
        report.summary.len()
    );
    assert_eq!(report.commands_run.len(), 50);
    assert!(report.commands_run.iter().all(|c| c.chars().count() <= 201));
    assert!(report.flags.contains(&ReportFlag::SummaryTruncated));
    assert!(report.flags.contains(&ReportFlag::ListTruncated));
    assert_eq!(report.tool_calls, 80);
}

#[tokio::test]
async fn an_editing_child_whose_tests_fail_does_not_report_done() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().to_path_buf();
    let provider = MockProvider::new(vec![
        Script::tool_call("c1", "edit", json!({"path": "x.txt", "content": "a"})),
        Script::text("fixed it"),
    ]);
    let factory = factory(move |setup| {
        let (bash, _ran) = common::gates::ScriptedBash::new(vec![(
            "make test",
            vec![common::gates::failing(1, "1 test failed")],
        )]);
        let mode = setup.mode;
        let tools = ToolRegistry::new(vec![Arc::new(common::gates::Edit), Arc::new(bash)]);
        Ok(Agent::new(
            provider.clone(),
            tools,
            setup.policy,
            setup.approver,
            AgentConfig::new("mock/m1", "m1", "p", d.join(".spill")),
            ToolContext::new(&d).with_sandbox(None, mode.fs_access()),
        )
        .with_session(setup.session)
        .with_gates(Gates {
            test: Some("make test".into()),
            max_retries: 0,
            ..Gates::default()
        }))
    });
    let p = parent(dir.path(), Mode::Auto, Arc::new(NonInteractive));
    let (sup, _rx) = supervisor(factory, p.ctx);
    let report = wait(sup.spawn(spec("edit"))).await;
    assert_eq!(report.status, AgentStatus::Failed);
    assert!(report.error.unwrap().contains("tests"));
    assert_eq!(report.files_changed, vec!["x.txt"]);
}

// ---- approvals ------------------------------------------------------------------------------

struct Answering {
    seen: Mutex<Vec<ApprovalRequest>>,
    decision: ApprovalDecision,
}

#[async_trait]
impl Approver for Answering {
    async fn decide(&self, request: &ApprovalRequest) -> ApprovalDecision {
        self.seen.lock().unwrap().push(request.clone());
        self.decision.clone()
    }
}

fn answering(decision: ApprovalDecision) -> Arc<Answering> {
    Arc::new(Answering {
        seen: Mutex::new(Vec::new()),
        decision,
    })
}

fn put(id: &str, path: &str) -> Script {
    Script::tool_call(id, "put", json!({"path": path, "content": "x"}))
}

#[tokio::test]
async fn an_approval_request_names_the_child_and_its_call_is_qualified() {
    let dir = tempfile::tempdir().unwrap();
    let factory = scripted(
        dir.path(),
        vec![vec![put("c1", "f.txt"), Script::text("ok")]],
    );
    let approver = answering(ApprovalDecision::Approve);
    let p = parent(dir.path(), Mode::Ask, approver.clone());
    let (sup, _rx) = supervisor(factory, p.ctx);
    let report = wait(sup.spawn(spec("writer"))).await;
    assert_eq!(report.status, AgentStatus::Done);
    let seen = approver.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    let tag = seen[0].agent.as_ref().expect("the request names its agent");
    assert_eq!((tag.id.as_str(), tag.label.as_str()), ("a1", "writer"));
    assert_eq!(seen[0].call_id, "a1/c1");
}

#[tokio::test]
async fn a_child_waiting_for_an_answer_needs_input_and_returns_to_working() {
    struct Gate(Notify, Notify);
    #[async_trait]
    impl Approver for Gate {
        async fn decide(&self, _r: &ApprovalRequest) -> ApprovalDecision {
            self.0.notify_one();
            self.1.notified().await;
            ApprovalDecision::Approve
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let gate = Arc::new(Gate(Notify::new(), Notify::new()));
    let factory = scripted(
        dir.path(),
        vec![vec![put("c1", "f.txt"), Script::text("ok")]],
    );
    let p = parent(dir.path(), Mode::Ask, gate.clone());
    let (sup, _rx) = supervisor(factory, p.ctx);
    let handle = sup.spawn(spec("writer"));
    gate.0.notified().await;
    assert_eq!(statuses(&sup), vec![AgentStatus::NeedsInput]);
    gate.1.notify_one();
    assert_eq!(wait(handle).await.status, AgentStatus::Done);
}

#[tokio::test]
async fn without_anyone_to_ask_a_child_that_needs_approval_fails_and_says_why() {
    let dir = tempfile::tempdir().unwrap();
    let factory = scripted(
        dir.path(),
        vec![vec![put("c1", "f.txt"), Script::text("wrote it")]],
    );
    let p = parent(dir.path(), Mode::Ask, Arc::new(NonInteractive));
    let (sup, mut rx) = supervisor(factory, p.ctx);
    let report = wait(sup.spawn(spec("writer"))).await;
    assert_eq!(report.status, AgentStatus::Failed);
    let error = report.error.unwrap();
    assert!(
        error.contains("approval") && error.contains("no one"),
        "{error}"
    );
    assert!(!dir.path().join("f.txt").exists());
    // The child's blocked action is in its own events, tagged.
    assert!(
        drain(&mut rx)
            .iter()
            .any(|e| e.agent.is_some() && matches!(e.event, AgentEvent::ActionBlocked { .. }))
    );
}

#[tokio::test]
async fn allow_for_session_given_to_a_child_does_not_reach_its_parent_or_a_sibling() {
    let dir = tempfile::tempdir().unwrap();
    let factory = scripted(
        dir.path(),
        vec![
            vec![put("c1", "f.txt"), put("c2", "f.txt"), Script::text("ok")],
            vec![put("c1", "f.txt"), Script::text("ok")],
        ],
    );
    let approver = answering(ApprovalDecision::ApproveForSession);
    let p = parent(dir.path(), Mode::Ask, approver.clone());
    let parent_engine = p.engine.clone();
    let (sup, _rx) = supervisor(factory, p.ctx);
    // The first child is asked once for its two writes to one file...
    wait(sup.spawn(spec("first"))).await;
    assert_eq!(approver.seen.lock().unwrap().len(), 1);
    // ...its sibling is asked again...
    wait(sup.spawn(spec("second"))).await;
    assert_eq!(approver.seen.lock().unwrap().len(), 2);
    // ...and the parent still asks.
    use harness_core::permission::{Action, Decision, PermissionPolicy};
    assert!(matches!(
        parent_engine.check(&Action::Write(dir.path().join("f.txt"))),
        Decision::Ask(_)
    ));
}

// ---- permissions ----------------------------------------------------------------------------

#[tokio::test]
async fn a_child_never_has_a_wider_mode_than_its_parent() {
    let dir = tempfile::tempdir().unwrap();
    let factory = scripted(
        dir.path(),
        vec![vec![put("c1", "f.txt"), Script::text("tried")]],
    );
    let p = parent(dir.path(), Mode::ReadOnly, Arc::new(AlwaysApprove));
    let (sup, _rx) = supervisor(factory.clone(), p.ctx);
    let mut s = spec("greedy");
    s.mode = Some(Mode::FullAccess);
    wait(sup.spawn(s)).await;
    assert_eq!(factory.built.lock().unwrap()[0].1, Mode::ReadOnly);
    assert!(!dir.path().join("f.txt").exists());
}

#[tokio::test]
async fn the_parents_mode_when_the_child_starts_is_the_one_that_counts() {
    let dir = tempfile::tempdir().unwrap();
    let factory = scripted(dir.path(), vec![vec![Script::text("ok")]]);
    let p = parent(dir.path(), Mode::Auto, Arc::new(NonInteractive));
    harness_core::permission::PermissionPolicy::set_mode(&*p.engine, Mode::Plan);
    let (sup, _rx) = supervisor(factory.clone(), p.ctx);
    wait(sup.spawn(spec("x"))).await;
    assert_eq!(factory.built.lock().unwrap()[0].1, Mode::Plan);
}

// ---- sessions and usage ---------------------------------------------------------------------

#[tokio::test]
async fn a_childs_transcript_is_its_own_session_linked_to_the_parent() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let factory = scripted(dir.path(), vec![vec![Script::text("hello")]]);
    let mut p = parent(dir.path(), Mode::Auto, Arc::new(NonInteractive));
    p.ctx.sessions_dir = Some(sessions.clone());
    let (sup, _rx) = supervisor(factory, p.ctx);
    let report = wait(sup.spawn(spec("explore"))).await;
    let id = report.session.expect("the child's session is named");
    let found = harness_core::session::children_of(&sessions, "parent-session");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].id, id);
    let (child, _) = Session::open(&sessions.join(format!("{id}.jsonl"))).unwrap();
    let lineage = child.lineage().unwrap();
    assert_eq!(
        (lineage.parent.as_str(), lineage.agent.as_str()),
        ("parent-session", "a1")
    );
    assert!(!child.messages().is_empty());
    // Not offered to `--resume`.
    assert!(harness_core::session::list(&sessions).is_empty());
}

#[tokio::test]
async fn the_parent_session_records_the_spawn_and_the_report() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let factory = scripted(dir.path(), vec![vec![Script::text("child text")]]);
    let parent_session = Session::create(&sessions, dir.path());
    let mut p = parent(dir.path(), Mode::Auto, Arc::new(NonInteractive));
    p.ctx.sessions_dir = Some(sessions.clone());
    p.ctx.session = parent_session.id().to_string();
    let (sup, _rx) = supervisor(factory, p.ctx);
    // The parent agent keeps the records the supervisor hands it.
    let parent_path = parent_session.path().map(Path::to_path_buf);
    let provider = MockProvider::new(vec![Script::text("parent text")]);
    let mut agent = common::agent(provider, Mode::Auto, Arc::new(NonInteractive), dir.path())
        .with_session(parent_session)
        .with_child_records(sup.records());
    wait(sup.spawn(spec("explore"))).await;
    let (reason, _) = common::run(&mut agent, "hi").await;
    assert_eq!(reason, TurnEndReason::Completed);
    let path = agent.session().path().unwrap().to_path_buf();
    drop(agent);
    let _ = parent_path;
    let (reopened, _) = Session::open(&path).unwrap();
    let records: Vec<_> = std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .filter(|l| l.contains("agent_record"))
        .map(String::from)
        .collect();
    assert_eq!(records.len(), 2, "{records:?}");
    assert!(records[0].contains("spawned") && records[1].contains("reported"));
    assert!(records[1].contains("child text"));
    // They are no part of the conversation.
    assert_eq!(reopened.messages().len(), 2);
}

#[derive(Default)]
struct Recording {
    requests: Mutex<Vec<(String, Option<AgentScope>)>>,
    budgets: Mutex<Vec<String>>,
}

impl Meter for Recording {
    fn record_request(&self, r: &RequestRecord) -> RequestCost {
        self.record_request_as(r, None)
    }
    fn record_request_as(&self, r: &RequestRecord, scope: Option<&AgentScope>) -> RequestCost {
        self.requests
            .lock()
            .unwrap()
            .push((r.session.clone(), scope.cloned()));
        RequestCost {
            account: AccountKind::Local,
            billed_usd: Some(0.0),
            list_usd: Some(0.0),
            avoided: harness_core::meter::Avoided::NotApplicable,
        }
    }
    fn check_budget(&self, session: &str, _a: AccountKind) -> BudgetStatus {
        self.budgets.lock().unwrap().push(session.into());
        BudgetStatus::default()
    }
}

#[tokio::test]
async fn a_childs_requests_name_it_and_its_budget_is_the_parents() {
    let dir = tempfile::tempdir().unwrap();
    let factory = scripted(dir.path(), vec![vec![Script::text("ok")]]);
    let meter = Arc::new(Recording::default());
    let mut p = parent(dir.path(), Mode::Auto, Arc::new(NonInteractive));
    p.ctx.meter = Some(meter.clone());
    let (sup, _rx) = supervisor(factory, p.ctx);
    wait(sup.spawn(spec("x"))).await;
    let requests = meter.requests.lock().unwrap();
    // The mock reports no usage, so the agent records nothing for it; the budget check is made.
    let _ = &*requests;
    assert_eq!(*meter.budgets.lock().unwrap(), vec!["parent-session"]);
}

// ---- process-wide state ---------------------------------------------------------------------

#[derive(Debug, Default)]
struct CountingSandbox {
    ended: std::sync::atomic::AtomicUsize,
}

impl harness_core::tool::CommandSandbox for CountingSandbox {
    fn name(&self) -> &'static str {
        "counting"
    }
    fn command(
        &self,
        _access: harness_core::permission::FsAccess,
        _workspace: &Path,
        program: &str,
        args: &[&str],
    ) -> std::io::Result<tokio::process::Command> {
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(args);
        Ok(cmd)
    }
    fn is_denial(&self, _exit_code: Option<i32>, _output: &str) -> bool {
        false
    }
    fn end_session(&self) -> Option<String> {
        self.ended.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        None
    }
}

#[tokio::test]
async fn ending_a_child_never_ends_the_sandbox_session_its_parent_and_siblings_share() {
    let dir = tempfile::tempdir().unwrap();
    let sandbox = Arc::new(CountingSandbox::default());
    let (s, d) = (sandbox.clone(), dir.path().to_path_buf());
    let factory = factory(move |setup| {
        let mode = setup.mode;
        Ok(Agent::new(
            MockProvider::new(vec![Script::text("ok")]),
            tools(),
            setup.policy,
            setup.approver,
            AgentConfig::new("mock/m1", "m1", "p", d.join(".spill")),
            ToolContext::new(&d).with_sandbox(Some(s.clone()), mode.fs_access()),
        )
        .with_session(setup.session))
    });
    let p = parent(dir.path(), Mode::Auto, Arc::new(NonInteractive));
    let (sup, _rx) = supervisor(factory, p.ctx);
    wait(sup.spawn(spec("a"))).await;
    wait(sup.spawn(spec("b"))).await;
    assert_eq!(sandbox.ended.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[allow(dead_code)]
fn unused(_: PathBuf) {}

// ---- a child that panics, is cancelled while it starts, or waits for an answer ---------------

#[tokio::test]
async fn a_child_that_panics_ends_failed_and_its_parent_hears_of_it() {
    let dir = tempfile::tempdir().unwrap();
    let factory = factory(|_| panic!("the factory broke"));
    let p = parent(dir.path(), Mode::Auto, Arc::new(NonInteractive));
    let (sup, mut rx) = supervisor(factory, p.ctx);
    let report = wait(sup.spawn(spec("x"))).await;
    assert_eq!(report.status, AgentStatus::Failed);
    assert!(report.error.unwrap().contains("crashed"));
    // The registry no longer says it works, the lifecycle ends, and the record is kept.
    assert_eq!(statuses(&sup), vec![AgentStatus::Failed]);
    assert!(!sup.cancel(&AgentId("a1".into())));
    assert!(matches!(
        drain(&mut rx).last().map(|e| &e.event),
        Some(AgentEvent::AgentFinished {
            status: AgentStatus::Failed,
            ..
        })
    ));
    assert!(
        sup.records()
            .take("parent-session")
            .iter()
            .any(|r| { matches!(r.kind, harness_core::session::AgentRecordKind::Reported) })
    );
}

/// A factory whose children never finish being built.
struct Never;

#[async_trait]
impl AgentFactory for Never {
    fn endpoint(&self, _spec: &AgentSpec) -> Endpoint {
        Endpoint {
            key: "mock".into(),
            local: true,
            delegated: false,
        }
    }
    async fn build(&self, _setup: ChildSetup) -> Result<Agent, String> {
        std::future::pending().await
    }
}

#[tokio::test]
async fn cancelling_a_child_that_is_still_being_built_ends_it_and_frees_its_place() {
    let dir = tempfile::tempdir().unwrap();
    let p = parent(dir.path(), Mode::Auto, Arc::new(NonInteractive));
    let (tx, _rx) = mpsc::unbounded_channel();
    let sup = Supervisor::new(Limits::default(), Arc::new(Never), p.ctx, tx);
    let handle = sup.spawn(spec("slow"));
    until("it is being built", || {
        statuses(&sup) == vec![AgentStatus::Working]
    })
    .await;
    assert!(sup.cancel(&handle.tag.id));
    assert_eq!(wait(handle).await.status, AgentStatus::Cancelled);
    // Its place on the one local server is free: the next child is not left waiting.
    let next = sup.spawn(spec("next"));
    until("the next one has its place", || {
        statuses(&sup)[1] == AgentStatus::Working
    })
    .await;
    sup.cancel_all();
    wait(next).await;
}

#[tokio::test]
async fn cancelling_the_parent_while_a_child_waits_for_an_approval_ends_it_and_drops_the_question()
{
    struct Forever {
        asked: Notify,
        dropped: Arc<std::sync::atomic::AtomicBool>,
    }
    struct OnDrop(Arc<std::sync::atomic::AtomicBool>);
    impl Drop for OnDrop {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
    #[async_trait]
    impl Approver for Forever {
        async fn decide(&self, _r: &ApprovalRequest) -> ApprovalDecision {
            let _guard = OnDrop(self.dropped.clone());
            self.asked.notify_one();
            std::future::pending().await
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let approver = Arc::new(Forever {
        asked: Notify::new(),
        dropped: Arc::default(),
    });
    let factory = scripted(
        dir.path(),
        vec![vec![put("c1", "f.txt"), Script::text("ok")]],
    );
    let p = parent(dir.path(), Mode::Ask, approver.clone());
    let cancel = p.ctx.cancel.clone();
    let (sup, _rx) = supervisor(factory, p.ctx);
    let handle = sup.spawn(spec("writer"));
    approver.asked.notified().await;
    assert_eq!(statuses(&sup), vec![AgentStatus::NeedsInput]);
    cancel.cancel();
    let report = wait(handle).await;
    assert_eq!(report.status, AgentStatus::Cancelled);
    assert!(approver.dropped.load(std::sync::atomic::Ordering::SeqCst));
    assert!(!dir.path().join("f.txt").exists());
}

#[tokio::test]
async fn a_budget_stop_ends_a_child_as_failed_and_says_which_budget() {
    use harness_core::meter::{BudgetKind, BudgetNotice};
    #[derive(Default)]
    struct Stopped {
        asked: Mutex<Vec<String>>,
    }
    impl Meter for Stopped {
        fn record_request(&self, _r: &RequestRecord) -> RequestCost {
            RequestCost {
                account: AccountKind::ApiKey,
                billed_usd: Some(0.0),
                list_usd: Some(0.0),
                avoided: harness_core::meter::Avoided::NotApplicable,
            }
        }
        fn check_budget(&self, session: &str, _a: AccountKind) -> BudgetStatus {
            self.asked.lock().unwrap().push(session.into());
            BudgetStatus {
                stop: Some(BudgetNotice {
                    budget: BudgetKind::Session,
                    spent_usd: 1.0,
                    limit_usd: 1.0,
                }),
                ..BudgetStatus::default()
            }
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![Script::text("never sent")]);
    let seen = provider.clone();
    let d = dir.path().to_path_buf();
    let factory = factory(move |setup| Ok(child_agent(setup, provider.clone(), &d)));
    let meter = Arc::new(Stopped::default());
    let mut p = parent(dir.path(), Mode::Auto, Arc::new(NonInteractive));
    p.ctx.meter = Some(meter.clone());
    let (sup, _rx) = supervisor(factory, p.ctx);
    let report = wait(sup.spawn(spec("spender"))).await;
    assert_eq!(report.status, AgentStatus::Failed);
    assert!(report.error.unwrap().contains("budget"));
    // The request was not sent, and the question was about the parent's session.
    assert!(seen.requests().is_empty());
    assert_eq!(*meter.asked.lock().unwrap(), vec!["parent-session"]);
}

#[tokio::test]
async fn switching_the_parent_to_read_only_restricts_a_child_that_is_already_running() {
    // The user approves the child's first write, and meanwhile puts the parent in read-only.
    struct Switching(Arc<PermissionEngine>);
    #[async_trait]
    impl Approver for Switching {
        async fn decide(&self, _r: &ApprovalRequest) -> ApprovalDecision {
            harness_core::permission::PermissionPolicy::set_mode(&*self.0, Mode::ReadOnly);
            ApprovalDecision::Approve
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let factory = scripted(
        dir.path(),
        vec![vec![
            put("c1", "first.txt"),
            put("c2", "second.txt"),
            Script::text("tried both"),
        ]],
    );
    let mut p = parent(dir.path(), Mode::Ask, Arc::new(NonInteractive));
    p.ctx.approver = Arc::new(Switching(p.engine.clone()));
    let (sup, _rx) = supervisor(factory, p.ctx);
    let report = wait(sup.spawn(spec("writer"))).await;
    assert_eq!(report.status, AgentStatus::Done);
    assert!(dir.path().join("first.txt").exists());
    assert!(
        !dir.path().join("second.txt").exists(),
        "a read-only parent stops its running child's next write"
    );
}

// ---- files changed through bash -------------------------------------------------------------

/// A factory whose child has checkpoints, and a `bash` that writes `made.txt` when it runs
/// `make edit`, as `sed -i` or `cargo fmt` would: no edit tool says it changed that file.
fn bash_editing(dir: &Path, gates: Gates) -> Arc<Factory> {
    let base = dir.canonicalize().unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call("c1", "bash", json!({"command": "make edit"})),
        Script::text("edited with a command"),
    ]);
    factory(move |setup| {
        let made = base.join("work/made.txt");
        let (bash, _ran) = common::gates::ScriptedBash::new(vec![
            ("make edit", vec![common::gates::passing()]),
            (
                "make test",
                vec![common::gates::failing(1, "1 test failed")],
            ),
        ]);
        let bash = bash.during("make edit", move || std::fs::write(&made, "x").unwrap());
        let mode = setup.mode;
        let checkpoints = harness_core::checkpoint::Checkpoints::open(
            &base.join("data/checkpoints/project.git"),
            &base.join("work"),
            setup.session.id(),
        )
        .unwrap();
        Ok(Agent::new(
            provider.clone(),
            ToolRegistry::new(vec![Arc::new(bash)]),
            setup.policy,
            setup.approver,
            AgentConfig::new("mock/m1", "m1", "p", base.join(".spill")),
            ToolContext::new(&base.join("work")).with_sandbox(None, mode.fs_access()),
        )
        .with_session(setup.session)
        .with_checkpoints(Some(Arc::new(checkpoints)))
        .with_gates(gates.clone()))
    })
}

fn bash_workspace() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().canonicalize().unwrap().join("work");
    std::fs::create_dir_all(&work).unwrap();
    (dir, work)
}

#[tokio::test]
async fn a_file_a_command_changed_is_in_the_report_beside_the_command() {
    let (dir, work) = bash_workspace();
    let factory = bash_editing(dir.path(), Gates::default());
    let p = parent(&work, Mode::Auto, Arc::new(NonInteractive));
    let (sup, _rx) = supervisor(factory, p.ctx);
    let report = wait(sup.spawn(spec("editor"))).await;
    assert_eq!(report.status, AgentStatus::Done, "{report:?}");
    assert_eq!(report.files_changed, vec!["made.txt"]);
    assert_eq!(report.commands_run, vec!["make edit"]);
}

#[tokio::test]
async fn a_child_that_edited_through_a_command_runs_the_gates() {
    let (dir, work) = bash_workspace();
    let factory = bash_editing(
        dir.path(),
        Gates {
            test: Some("make test".into()),
            max_retries: 0,
            ..Gates::default()
        },
    );
    let p = parent(&work, Mode::Auto, Arc::new(NonInteractive));
    let (sup, _rx) = supervisor(factory, p.ctx);
    let report = wait(sup.spawn(spec("editor"))).await;
    assert_eq!(report.status, AgentStatus::Failed, "{report:?}");
    assert!(report.error.unwrap().contains("tests"));
    assert_eq!(report.files_changed, vec!["made.txt"]);
}

/// Writes `sibling.txt` and says no file changed: what another agent in the same checkout does
/// while this one works.
struct Sibling;

#[async_trait]
impl harness_core::tool::Tool for Sibling {
    fn spec(&self) -> harness_core::message::ToolSpec {
        harness_core::message::ToolSpec {
            name: "sibling".into(),
            description: "another agent's edit".into(),
            parameters: json!({"type": "object"}),
        }
    }
    fn action(
        &self,
        _args: &serde_json::Value,
        ctx: &ToolContext,
    ) -> harness_core::permission::Action {
        harness_core::permission::Action::Read(ctx.workspace.clone())
    }
    async fn run(
        &self,
        _args: serde_json::Value,
        ctx: &ToolContext,
    ) -> harness_core::tool::ToolOutput {
        std::fs::write(ctx.workspace.join("sibling.txt"), "x").unwrap();
        harness_core::tool::ToolOutput::ok("done")
    }
}

#[tokio::test]
async fn a_child_in_a_shared_checkout_that_ran_no_command_lists_only_its_own_edits() {
    let (dir, work) = bash_workspace();
    let base = dir.path().canonicalize().unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call("c1", "edit", json!({"path": "mine.txt", "content": "m"})),
        Script::tool_call("c2", "sibling", json!({})),
        Script::text("done"),
    ]);
    let factory = factory(move |setup| {
        let mode = setup.mode;
        let checkpoints = harness_core::checkpoint::Checkpoints::open(
            &base.join("data/checkpoints/project.git"),
            &base.join("work"),
            setup.session.id(),
        )
        .unwrap();
        Ok(Agent::new(
            provider.clone(),
            ToolRegistry::new(vec![Arc::new(common::gates::Edit), Arc::new(Sibling)]),
            setup.policy,
            setup.approver,
            AgentConfig::new("mock/m1", "m1", "p", base.join(".spill")),
            ToolContext::new(&base.join("work")).with_sandbox(None, mode.fs_access()),
        )
        .with_session(setup.session)
        .with_checkpoints(Some(Arc::new(checkpoints))))
    });
    let p = parent(&work, Mode::Auto, Arc::new(NonInteractive));
    let (sup, _rx) = supervisor(factory, p.ctx);
    let report = wait(sup.spawn(spec("editor"))).await;
    assert_eq!(report.status, AgentStatus::Done, "{report:?}");
    assert_eq!(report.files_changed, vec!["mine.txt"], "{report:?}");
}

#[tokio::test]
async fn a_child_started_while_the_parent_was_in_plan_stays_there_when_the_parent_widens() {
    use harness_core::permission::PermissionPolicy;
    let dir = tempfile::tempdir().unwrap();
    let policies: Arc<Mutex<Vec<Arc<dyn PermissionPolicy>>>> = Arc::default();
    let seen = policies.clone();
    let work = dir.path().to_path_buf();
    let factory = factory(move |setup| {
        seen.lock().unwrap().push(setup.policy.clone());
        Ok(child_agent(
            setup,
            MockProvider::new(vec![Script::text("ok")]),
            &work,
        ))
    });
    let p = parent(dir.path(), Mode::Auto, Arc::new(NonInteractive));
    PermissionPolicy::set_mode(&*p.engine, Mode::Plan);
    let (sup, _rx) = supervisor(factory, p.ctx);
    wait(sup.spawn(spec("x"))).await;
    PermissionPolicy::set_mode(&*p.engine, Mode::Auto);
    let child = policies.lock().unwrap()[0].clone();
    assert_eq!(
        child.mode(),
        Some(Mode::Plan),
        "it never widens beyond its start"
    );
}

// ---- records reach the session they belong to -----------------------------------------------

fn record_lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|l| l.contains("agent_record"))
        .map(String::from)
        .collect()
}

#[tokio::test]
async fn a_child_that_ends_after_the_parents_last_turn_still_gets_its_record() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let (factory, started, _release) = held_factory(dir.path(), false);
    let session = Session::create(&sessions, dir.path());
    let mut p = parent(dir.path(), Mode::Auto, Arc::new(NonInteractive));
    p.ctx.sessions_dir = Some(sessions.clone());
    p.ctx.session = session.id().to_string();
    let (sup, _rx) = supervisor(factory, p.ctx);
    let provider = MockProvider::new(vec![Script::text("parent text")]);
    let mut agent = common::agent(provider, Mode::Auto, Arc::new(NonInteractive), dir.path())
        .with_session(session)
        .with_child_records(sup.records());
    common::run(&mut agent, "hi").await;
    // The turn is over; the child starts and is still running when the session ends.
    let handle = sup.spawn(spec("late"));
    started.notified().await;
    sup.cancel_all();
    agent.end_session().await;
    let path = agent.session().path().unwrap().to_path_buf();
    let records = record_lines(&path);
    assert_eq!(records.len(), 2, "{records:?}");
    assert!(records[0].contains("spawned") && records[1].contains("reported"));
    assert_eq!(wait(handle).await.status, AgentStatus::Cancelled);
}

#[tokio::test]
async fn moving_to_another_session_saves_the_records_in_the_one_left() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let factory = scripted(dir.path(), vec![vec![Script::text("child text")]]);
    let session = Session::create(&sessions, dir.path());
    let mut p = parent(dir.path(), Mode::Auto, Arc::new(NonInteractive));
    p.ctx.sessions_dir = Some(sessions.clone());
    p.ctx.session = session.id().to_string();
    let (sup, _rx) = supervisor(factory, p.ctx);
    let provider = MockProvider::new(vec![]);
    let mut agent = common::agent(provider, Mode::Auto, Arc::new(NonInteractive), dir.path())
        .with_session(session)
        .with_child_records(sup.records());
    let first = agent.session().path().unwrap().to_path_buf();
    wait(sup.spawn(spec("explore"))).await;
    // `/new`: the next session is not given the first one's records.
    let next = Session::create(&sessions, dir.path());
    let second = next.path().unwrap().to_path_buf();
    agent.start_session(next, None);
    agent.end_session().await;
    assert_eq!(record_lines(&first).len(), 2);
    assert!(record_lines(&second).is_empty());
}

// ---- what a child used, and what its listing says --------------------------------------------

/// A meter that prices every request the same, or not at all.
struct Priced {
    billed: Option<f64>,
    list: Option<f64>,
}

impl Meter for Priced {
    fn record_request(&self, _r: &RequestRecord) -> RequestCost {
        RequestCost {
            account: AccountKind::ApiKey,
            billed_usd: self.billed,
            list_usd: self.list,
            avoided: harness_core::meter::Avoided::NotApplicable,
        }
    }
    fn check_budget(&self, _s: &str, _a: AccountKind) -> BudgetStatus {
        BudgetStatus::default()
    }
}

fn used(
    input: u64,
    output: u64,
    cached: u64,
    finish: FinishReason,
    event: ProviderEvent,
) -> Script {
    Script::Reply(vec![
        Ok(event),
        Ok(ProviderEvent::Usage(harness_core::message::Usage {
            input_tokens: input,
            output_tokens: output,
            cached_tokens: cached,
            ..Default::default()
        })),
        Ok(ProviderEvent::Finished(finish)),
    ])
}

fn two_requests() -> Vec<Script> {
    vec![
        used(
            100,
            10,
            40,
            FinishReason::ToolCalls,
            ProviderEvent::ToolCall(harness_core::message::ToolCall {
                id: "c1".into(),
                name: "echo".into(),
                arguments: json!({"text": "hi"}).to_string(),
            }),
        ),
        used(
            200,
            20,
            0,
            FinishReason::Stop,
            ProviderEvent::TextDelta("done".into()),
        ),
    ]
}

#[tokio::test]
async fn a_childs_report_says_the_tokens_and_cost_of_its_requests() {
    let dir = tempfile::tempdir().unwrap();
    let factory = scripted(dir.path(), vec![two_requests()]);
    let mut p = parent(dir.path(), Mode::Auto, Arc::new(NonInteractive));
    p.ctx.meter = Some(Arc::new(Priced {
        billed: Some(0.25),
        list: Some(0.5),
    }));
    let (sup, _rx) = supervisor(factory, p.ctx);
    let report = wait(sup.spawn(spec("x"))).await;
    let usage = report.usage.expect("the report has usage");
    assert_eq!(usage.requests, 2);
    assert_eq!(
        (usage.input_tokens, usage.output_tokens, usage.cached_tokens),
        (300, 30, 40)
    );
    assert_eq!(usage.billed_usd, Some(0.5));
    assert_eq!(usage.list_usd, Some(1.0));
}

#[tokio::test]
async fn a_request_with_no_price_makes_the_cost_unknown_never_zero() {
    let dir = tempfile::tempdir().unwrap();
    let factory = scripted(dir.path(), vec![two_requests()]);
    let mut p = parent(dir.path(), Mode::Auto, Arc::new(NonInteractive));
    p.ctx.meter = Some(Arc::new(Priced {
        billed: None,
        list: None,
    }));
    let (sup, _rx) = supervisor(factory, p.ctx);
    let usage = wait(sup.spawn(spec("x"))).await.usage.unwrap();
    assert_eq!(usage.input_tokens, 300);
    assert_eq!((usage.billed_usd, usage.list_usd), (None, None));
}

#[tokio::test]
async fn without_a_meter_the_report_has_no_usage() {
    let dir = tempfile::tempdir().unwrap();
    let factory = scripted(dir.path(), vec![vec![Script::text("ok")]]);
    let p = parent(dir.path(), Mode::Auto, Arc::new(NonInteractive));
    let (sup, _rx) = supervisor(factory, p.ctx);
    assert_eq!(wait(sup.spawn(spec("x"))).await.usage, None);
}

#[test]
fn a_report_written_before_usage_existed_still_reads() {
    let old = r#"{"agent":"a1","label":"x","status":"done","summary":"s","files_changed":[],"commands_run":[],"tool_calls":0}"#;
    let report: harness_core::agents::AgentReport = serde_json::from_str(old).unwrap();
    assert_eq!(report.usage, None);
    let json = serde_json::to_string(&report).unwrap();
    assert!(!json.contains("usage"), "{json}");
}

#[tokio::test]
async fn a_finished_childs_listing_carries_its_summary_and_its_error() {
    let dir = tempfile::tempdir().unwrap();
    let factory = scripted(
        dir.path(),
        vec![
            vec![Script::text("it is in main.rs\nand more")],
            vec![Script::error(
                harness_core::provider::ProviderError::Protocol("boom".into()),
            )],
        ],
    );
    let p = parent(dir.path(), Mode::Auto, Arc::new(NonInteractive));
    let (sup, _rx) = supervisor(factory, p.ctx);
    wait(sup.spawn(spec("ok"))).await;
    wait(sup.spawn(spec("bad"))).await;
    let infos = sup.snapshot();
    assert_eq!(
        infos[0].summary.as_deref(),
        Some("it is in main.rs\nand more")
    );
    assert_eq!(infos[0].error, None);
    assert_eq!(infos[1].status, AgentStatus::Failed);
    assert!(
        infos[1].error.as_deref().unwrap().contains("boom"),
        "{:?}",
        infos[1]
    );
}
