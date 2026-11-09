//! The supervisor: spawns child agents from specs, limits how many run at once, tracks what each
//! is doing, cancels them with their parent, and hands back a report.
//!
//! A child is an ordinary [`Agent`] built by an [`AgentFactory`] (the frontend knows how to make
//! one), run for one turn on a task of its own. Its events go to the supervisor's sink in
//! envelopes naming it, its approvals go through its parent's approver labelled with it, its
//! permissions are narrowed from its parent's ([`perm`](super::perm)), its transcript is a
//! session of its own, and its requests are recorded under it ([`ScopedMeter`]).
//!
//! The supervisor never calls [`CommandSandbox::end_session`](crate::tool::CommandSandbox): that
//! ends every process any agent's commands left running, and belongs to the end of the whole
//! session. Ending a child ends only the child.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex, PoisonError},
    time::Instant,
};

use async_trait::async_trait;
use tokio::sync::{mpsc::UnboundedSender, watch};
use tokio_util::sync::CancellationToken;

use crate::{
    agent::{Agent, ApprovalDecision, ApprovalRequest, Approver},
    agents::{
        AgentEnvelope, AgentId, AgentRecords, AgentReport, AgentStatus, AgentTag, ChildWorkspace,
        Endpoint, Limits, ScopedMeter, ToolLog, perm, sched::Scheduler,
    },
    engine::{EngineConfig, PermissionEngine, RuleSet},
    event::{AgentEvent, TurnEndReason},
    meter::{AgentScope, Meter},
    permission::{Mode, PermissionPolicy},
    role::Role,
    session::{AgentRecord, AgentRecordKind, Lineage, Session},
    turn::{InputPart, TurnInput},
};

/// What a child is asked to do, and with what.
#[derive(Debug, Clone, Default)]
pub struct AgentSpec {
    /// The name shown for it.
    pub label: String,
    /// What it is asked: the first message of its turn.
    pub task: String,
    /// Its system prompt, when it is not the factory's default.
    pub system_prompt: Option<String>,
    /// The `<provider>/<model>` it runs on, when it is not its parent's or its role's.
    pub model: Option<String>,
    /// The tools it is offered, by name; `None` is all of its parent's.
    pub tools: Option<Vec<String>>,
    /// Its mode. Never wider than its parent's.
    pub mode: Option<Mode>,
    /// Rules of its own. Never wider than its parent's.
    pub rules: RuleSet,
    /// Where it works: its parent's workspace, or one harness made for it. Never a free path.
    pub workspace: ChildWorkspace,
    pub max_steps: Option<u32>,
    /// The role its turn runs for, which picks its model when `model` does not.
    pub role: Option<Role>,
}

impl AgentSpec {
    pub fn new(label: impl Into<String>, task: impl Into<String>) -> Self {
        AgentSpec {
            label: label.into(),
            task: task.into(),
            ..AgentSpec::default()
        }
    }
}

/// What the supervisor decided for a child, for the factory to build it from.
pub struct ChildSetup {
    pub tag: AgentTag,
    pub spec: AgentSpec,
    /// Its permissions: narrowed from its parent's, with grants of its own, and following the
    /// parent's mode while it runs ([`perm::ChildPolicy`]).
    pub policy: Arc<dyn PermissionPolicy>,
    /// The mode of `policy` when it starts, to choose the sandbox with.
    pub mode: Mode,
    /// Asks the user (or fails, when there is none) on the child's behalf.
    pub approver: Arc<dyn Approver>,
    /// Reports its requests as its own; `None` when the parent has no meter.
    pub meter: Option<Arc<dyn Meter>>,
    /// Where its transcript is saved.
    pub session: Session,
}

/// Makes child agents. Implemented by the frontend, which knows the providers, tools, sandboxes
/// and settings.
#[async_trait]
pub trait AgentFactory: Send + Sync {
    /// Where the child's requests will go, for the concurrency limits: known before the child is
    /// built, so a child that has to wait does not load a model it is not yet using.
    fn endpoint(&self, spec: &AgentSpec) -> Endpoint;

    /// Builds the child. The supervisor runs one turn of it with `spec.task`.
    async fn build(&self, setup: ChildSetup) -> Result<Agent, String>;
}

/// Who spawns the children, and what they inherit from it.
#[derive(Clone)]
pub struct ParentContext {
    /// The parent's session: the one its children are linked to and whose budget they spend.
    pub session: String,
    /// Cancelling it cancels every child spawned without a token of its own.
    pub cancel: CancellationToken,
    /// The parent's permissions, for the mode it is in when a child starts.
    pub policy: Arc<dyn PermissionPolicy>,
    /// The parent's rules and paths, which a child's are narrowed from.
    pub engine: EngineConfig,
    /// Who answers approvals.
    pub approver: Arc<dyn Approver>,
    pub meter: Option<Arc<dyn Meter>>,
    /// Where child sessions are saved; `None` keeps them in memory.
    pub sessions_dir: Option<PathBuf>,
    pub workspace: PathBuf,
}

/// What the supervisor knows of a child, for lists and status lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentInfo {
    pub tag: AgentTag,
    pub status: AgentStatus,
    pub model: Option<String>,
    pub tool_calls: u32,
    /// The tool it called last.
    pub activity: Option<String>,
    /// When it started working (it is not set while it waits for a place).
    pub started: Option<Instant>,
}

struct Entry {
    info: AgentInfo,
    cancel: CancellationToken,
}

struct Registry {
    next: u64,
    entries: Vec<Entry>,
}

struct Inner {
    factory: Arc<dyn AgentFactory>,
    scheduler: Scheduler,
    events: UnboundedSender<AgentEnvelope>,
    parent: Mutex<ParentContext>,
    registry: Mutex<Registry>,
    version: watch::Sender<u64>,
    records: AgentRecords,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Inner {
    fn update(&self, id: &AgentId, change: impl FnOnce(&mut AgentInfo)) {
        {
            let mut registry = lock(&self.registry);
            if let Some(entry) = registry.entries.iter_mut().find(|e| &e.info.tag.id == id) {
                change(&mut entry.info);
            }
        }
        self.version.send_modify(|v| *v += 1);
    }

    fn status(&self, id: &AgentId) -> Option<AgentStatus> {
        lock(&self.registry)
            .entries
            .iter()
            .find(|e| &e.info.tag.id == id)
            .map(|e| e.info.status)
    }
}

/// Runs child agents. See the module documentation.
pub struct Supervisor {
    inner: Arc<Inner>,
}

/// A spawned child: wait for its report.
pub struct AgentHandle {
    pub tag: AgentTag,
    report: watch::Receiver<Option<AgentReport>>,
}

impl AgentHandle {
    /// Waits until the child has ended, and returns its report.
    pub async fn wait(mut self) -> AgentReport {
        loop {
            if let Some(report) = self.report.borrow().clone() {
                return report;
            }
            if self.report.changed().await.is_err() {
                // The task ended without a report (the runtime shut down).
                return AgentReport::build(
                    self.tag.id.as_str(),
                    &self.tag.label,
                    AgentStatus::Failed,
                    "",
                    &ToolLog::default(),
                    std::path::Path::new("/"),
                );
            }
        }
    }
}

impl Supervisor {
    pub fn new(
        limits: Limits,
        factory: Arc<dyn AgentFactory>,
        parent: ParentContext,
        events: UnboundedSender<AgentEnvelope>,
    ) -> Arc<Supervisor> {
        Arc::new(Supervisor {
            inner: Arc::new(Inner {
                factory,
                scheduler: Scheduler::new(limits),
                events,
                parent: Mutex::new(parent),
                registry: Mutex::new(Registry {
                    next: 1,
                    entries: Vec::new(),
                }),
                version: watch::channel(0).0,
                records: AgentRecords::default(),
            }),
        })
    }

    /// The records of children the parent agent should save in its session
    /// ([`Agent::with_child_records`]).
    pub fn records(&self) -> AgentRecords {
        self.inner.records.clone()
    }

    /// The parent continues in another session (`/new`, `/resume`): children spawned from now on
    /// belong to it.
    pub fn set_parent_session(&self, session: &str) {
        lock(&self.inner.parent).session = session.to_string();
    }

    /// Cancelling `cancel` cancels the children spawned without a token of their own.
    pub fn set_parent_cancel(&self, cancel: CancellationToken) {
        lock(&self.inner.parent).cancel = cancel;
    }

    /// Every child spawned, oldest first, as it is now.
    pub fn snapshot(&self) -> Vec<AgentInfo> {
        lock(&self.inner.registry)
            .entries
            .iter()
            .map(|e| e.info.clone())
            .collect()
    }

    /// A counter that changes whenever any child's status or activity does, for a display to
    /// redraw on.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.inner.version.subscribe()
    }

    /// Cancels child `id`; whether there was one that had not ended.
    pub fn cancel(&self, id: &AgentId) -> bool {
        let registry = lock(&self.inner.registry);
        match registry.entries.iter().find(|e| &e.info.tag.id == id) {
            Some(entry) if !entry.info.status.is_final() => {
                entry.cancel.cancel();
                true
            }
            _ => false,
        }
    }

    /// Cancels every child that has not ended.
    pub fn cancel_all(&self) {
        for entry in &lock(&self.inner.registry).entries {
            entry.cancel.cancel();
        }
    }

    /// Spawns a child under the parent's own token.
    pub fn spawn(&self, spec: AgentSpec) -> AgentHandle {
        let cancel = lock(&self.inner.parent).cancel.clone();
        self.spawn_under(spec, cancel)
    }

    /// Spawns a child that is cancelled when `parent_cancel` is: a tool call spawning it passes
    /// the turn's token. It waits for a place under the limits, then runs.
    pub fn spawn_under(&self, spec: AgentSpec, parent_cancel: CancellationToken) -> AgentHandle {
        let cancel = parent_cancel.child_token();
        let tag = {
            let mut registry = lock(&self.inner.registry);
            let tag = AgentTag {
                id: AgentId(format!("a{}", registry.next)),
                label: spec.label.clone(),
            };
            registry.next += 1;
            registry.entries.push(Entry {
                info: AgentInfo {
                    tag: tag.clone(),
                    status: AgentStatus::Queued,
                    model: None,
                    tool_calls: 0,
                    activity: None,
                    started: None,
                },
                cancel: cancel.clone(),
            });
            tag
        };
        self.inner.version.send_modify(|v| *v += 1);
        let (done, report) = watch::channel(None);
        let inner = self.inner.clone();
        let task_tag = tag.clone();
        let workspace = spec.workspace.resolve(&lock(&self.inner.parent).workspace);
        let task = tokio::spawn(run_child(inner.clone(), task_tag.clone(), spec, cancel));
        // A child that panics still ends: the registry says so, and its parent hears of it.
        tokio::spawn(async move {
            let report = match task.await {
                Ok(report) => report,
                Err(why) => {
                    let mut report = AgentReport::build(
                        task_tag.id.as_str(),
                        &task_tag.label,
                        AgentStatus::Failed,
                        "",
                        &ToolLog::default(),
                        &workspace,
                    );
                    report.error = Some(if why.is_panic() {
                        "the agent crashed".into()
                    } else {
                        "the agent was stopped".into()
                    });
                    if inner.status(&task_tag.id).is_some_and(|s| !s.is_final()) {
                        finish_child(&inner, &task_tag, report, None)
                    } else {
                        report
                    }
                }
            };
            let _ = done.send(Some(report));
        });
        AgentHandle { tag, report }
    }
}

/// What a child's events say, kept for its report.
#[derive(Default)]
struct Seen {
    text: String,
    error: Option<String>,
}

/// Ends a child: its status, its `AgentFinished` event and the record its parent keeps.
fn finish_child(
    inner: &Inner,
    tag: &AgentTag,
    mut report: AgentReport,
    session: Option<String>,
) -> AgentReport {
    let id = &tag.id;
    report.session = session.clone();
    inner.update(id, |info| info.status = report.status);
    let _ = inner.events.send(AgentEnvelope::child(
        id,
        AgentEvent::AgentFinished {
            status: report.status,
            summary: crate::agents::report::cap_chars(&report.summary, 200).0,
        },
    ));
    inner.records.push(AgentRecord {
        agent: id.to_string(),
        label: tag.label.clone(),
        kind: AgentRecordKind::Reported,
        session,
        report: serde_json::to_value(&report).ok(),
    });
    report
}

async fn run_child(
    inner: Arc<Inner>,
    tag: AgentTag,
    spec: AgentSpec,
    cancel: CancellationToken,
) -> AgentReport {
    let parent = lock(&inner.parent).clone();
    let id = tag.id.clone();
    let finish = |inner: &Inner, report: AgentReport, session: Option<String>| {
        finish_child(inner, &tag, report, session)
    };
    let workspace = spec.workspace.resolve(&parent.workspace);
    let empty = |status, error: Option<String>| {
        let mut report = AgentReport::build(
            id.as_str(),
            &tag.label,
            status,
            "",
            &ToolLog::default(),
            &workspace,
        );
        report.error = error;
        report
    };

    // A place under the limits; a parent that is cancelled while this waits ends it here, before
    // anything is built.
    let endpoint = inner.factory.endpoint(&spec);
    let permit = tokio::select! {
        permit = inner.scheduler.acquire(endpoint) => permit,
        _ = cancel.cancelled() => {
            return finish(&inner, empty(AgentStatus::Cancelled, Some("cancelled before it started".into())), None);
        }
    };
    inner.update(&id, |info| {
        info.status = AgentStatus::Working;
        info.started = Some(Instant::now());
    });

    // Its session, its permissions, its approver and its meter.
    let lineage = Lineage {
        parent: parent.session.clone(),
        agent: id.to_string(),
        label: tag.label.clone(),
    };
    let session = match &parent.sessions_dir {
        Some(dir) => Session::create_child(dir, &workspace, lineage),
        None => Session::in_memory(&workspace),
    };
    let session_id = session.id().to_string();
    let live_mode = parent.policy.mode().unwrap_or(parent.engine.mode);
    let config = perm::child_engine_config(
        &parent.engine,
        live_mode,
        spec.mode,
        &spec.rules,
        &spec.workspace,
    );
    let mode = config.mode;
    let policy: Arc<dyn PermissionPolicy> = Arc::new(perm::ChildPolicy::new(
        PermissionEngine::new(config),
        parent.policy.clone(),
        spec.mode,
    ));
    let failure = Arc::new(Mutex::new(None));
    let approver = Arc::new(ChildApprover {
        tag: tag.clone(),
        inner: parent.approver.clone(),
        supervisor: inner.clone(),
        cancel: cancel.clone(),
        failure: failure.clone(),
    });
    let meter: Option<Arc<dyn Meter>> = parent.meter.clone().map(|meter| {
        Arc::new(ScopedMeter::new(
            meter,
            AgentScope {
                agent: id.to_string(),
                parent: parent.session.clone(),
            },
        )) as Arc<dyn Meter>
    });
    inner.records.push(AgentRecord {
        agent: id.to_string(),
        label: tag.label.clone(),
        kind: AgentRecordKind::Spawned,
        session: Some(session_id.clone()),
        report: None,
    });
    // Cancelled while it is being built (a model's server may take a while to answer): it ends
    // here, and its place is free again.
    let built = tokio::select! {
        built = inner.factory.build(ChildSetup {
            tag: tag.clone(),
            spec: spec.clone(),
            policy,
            mode,
            approver,
            meter,
            session,
        }) => built,
        _ = cancel.cancelled() => {
            drop(permit);
            return finish(
                &inner,
                empty(AgentStatus::Cancelled, Some("cancelled while it started".into())),
                Some(session_id),
            );
        }
    };
    let mut agent = match built {
        Ok(agent) => agent,
        Err(why) => {
            drop(permit);
            return finish(
                &inner,
                empty(AgentStatus::Failed, Some(format!("could not start: {why}"))),
                Some(session_id),
            );
        }
    };
    agent.keep_tool_log();
    let model = agent.model_id().to_string();
    inner.update(&id, |info| info.model = Some(model.clone()));
    let _ = inner.events.send(AgentEnvelope::child(
        &id,
        AgentEvent::AgentStarted {
            label: tag.label.clone(),
            model,
        },
    ));

    // One turn, its events passed on as they come.
    let (events, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let forward = {
        let (inner, id) = (inner.clone(), id.clone());
        tokio::spawn(async move {
            let mut seen = Seen::default();
            while let Some(event) = receiver.recv().await {
                match &event {
                    AgentEvent::AssistantMessage { content, .. } if !content.is_empty() => {
                        seen.text = content.clone();
                    }
                    AgentEvent::Error { message, .. } => seen.error = Some(message.clone()),
                    AgentEvent::ToolCallRequested { name, .. } => {
                        let name = name.clone();
                        inner.update(&id, |info| {
                            info.tool_calls += 1;
                            info.activity = Some(name);
                        });
                    }
                    _ => {}
                }
                let _ = inner.events.send(AgentEnvelope::child(&id, event));
            }
            seen
        })
    };
    let input = TurnInput {
        parts: vec![InputPart::Text(spec.task.clone())],
        role: spec.role,
        ..TurnInput::default()
    };
    let reason = agent.run_turn(input, &events, cancel.clone()).await;
    drop(events);
    let seen = forward.await.unwrap_or_default();
    let mut log = agent.take_tool_log();
    // What changed in the workspace since the child's checkpoint counts too, so a file `bash`
    // edited is listed (the tools' own list comes first). Not waited for after a cancel.
    if !cancel.is_cancelled()
        && let Some(files) = agent.files_changed_in_workspace().await
    {
        for file in files {
            if !log.files_changed.contains(&file) {
                log.files_changed.push(file);
            }
        }
    }
    agent.close().await;
    drop(agent);
    drop(permit);

    let failure = lock(&failure).take();
    let (status, error) = match (reason, failure) {
        (_, Some(why)) => (AgentStatus::Failed, Some(why)),
        (TurnEndReason::Completed, None) => (AgentStatus::Done, None),
        (TurnEndReason::Interrupted, None) => (AgentStatus::Cancelled, None),
        (TurnEndReason::Error, None) => (
            AgentStatus::Failed,
            Some(
                seen.error
                    .clone()
                    .unwrap_or_else(|| "a model request failed".into()),
            ),
        ),
        (TurnEndReason::StepLimit, None) => (
            AgentStatus::Failed,
            Some("it stopped at its step limit".into()),
        ),
        (TurnEndReason::Budget, None) => (
            AgentStatus::Failed,
            Some("a budget was reached, so it stopped".into()),
        ),
        (TurnEndReason::GateFailed, None) => (
            AgentStatus::Failed,
            Some("the tests still fail after its changes".into()),
        ),
    };
    let mut report = AgentReport::build(
        id.as_str(),
        &tag.label,
        status,
        &seen.text,
        &log,
        &workspace,
    );
    report.error = error;
    finish(&inner, report, Some(session_id))
}

/// Asks on a child's behalf: through its parent's approver, the request labelled with the child
/// and its call id qualified. While the question waits the child needs input. Where no one can
/// answer, the child ends as failed, and says why.
struct ChildApprover {
    tag: AgentTag,
    inner: Arc<dyn Approver>,
    supervisor: Arc<Inner>,
    cancel: CancellationToken,
    failure: Arc<Mutex<Option<String>>>,
}

#[async_trait]
impl Approver for ChildApprover {
    async fn decide(&self, request: &ApprovalRequest) -> ApprovalDecision {
        let mut labelled = request.clone();
        labelled.agent = Some(self.tag.clone());
        labelled.call_id = crate::agents::qualify_call_id(&self.tag.id, &request.call_id);
        let id = &self.tag.id;
        self.supervisor
            .update(id, |info| info.status = AgentStatus::NeedsInput);
        let decision = self.inner.decide(&labelled).await;
        if self.supervisor.status(id) == Some(AgentStatus::NeedsInput) {
            self.supervisor
                .update(id, |info| info.status = AgentStatus::Working);
        }
        if decision == ApprovalDecision::Unavailable {
            *lock(&self.failure) = Some(format!(
                "it needed approval ({}) and no one could answer",
                request.reason
            ));
            self.cancel.cancel();
        }
        decision
    }
}
