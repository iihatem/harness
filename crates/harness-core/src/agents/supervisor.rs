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
    worktree::{Outcome, Request as WorktreeRequest, Worktree, Worktrees},
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
    /// Guidance added after its base system prompt (the parent's, as the factory builds it): what
    /// a definition says of the agent. Unlike `system_prompt`, it keeps the rest of the prompt
    /// (the edit tool's section, the project's instructions).
    pub instructions: Option<String>,
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
    /// It works in a worktree of its own, made for it when it starts. Ignored for an agent that
    /// cannot write (plan or read-only mode): it reads the main checkout.
    pub isolation: bool,
    /// It keeps running when the turn that started it ends or is interrupted; only
    /// [`Supervisor::stop_all`] (and the end of the session) stops it.
    pub background: bool,
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
    /// Variables its shell commands get: in a worktree, who it is and its port offset.
    pub env: Vec<(String, String)>,
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

/// The worktree of a child, as far as lists need it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeInfo {
    pub name: String,
    pub branch: String,
    pub path: PathBuf,
    /// What it left, committed on the branch, once it has ended and changed something.
    pub outcome: Option<Outcome>,
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
    /// Its final text, once it has ended (capped).
    pub summary: Option<String>,
    /// Why it failed or was stopped, once it has ended.
    pub error: Option<String>,
    /// When it started working (it is not set while it waits for a place).
    pub started: Option<Instant>,
    /// It keeps running when the turn ends.
    pub background: bool,
    /// Its worktree, when it has one.
    pub worktree: Option<WorktreeInfo>,
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
    /// The parent of the background children's tokens: cancelled only by [`Supervisor::stop_all`].
    lifetime: CancellationToken,
    worktrees: Mutex<Option<Arc<Worktrees>>>,
    /// Things the user should be told, which [`Supervisor::take_notices`] hands over once.
    notices: Mutex<Vec<String>>,
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
                return ended_without_report(&self.tag);
            }
        }
    }
}

/// The report of a child whose task ended with none: failed, and says so.
fn ended_without_report(tag: &AgentTag) -> AgentReport {
    let mut report = AgentReport::build(
        tag.id.as_str(),
        &tag.label,
        AgentStatus::Failed,
        "",
        &ToolLog::default(),
        std::path::Path::new("/"),
    );
    report.error = Some("the subagent ended without a report".into());
    report
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
                lifetime: CancellationToken::new(),
                worktrees: Mutex::new(None),
                notices: Mutex::new(Vec::new()),
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

    /// Cancels every child of the foreground that has not ended: the turn was interrupted or
    /// is over. Background children go on.
    pub fn cancel_all(&self) {
        for entry in &lock(&self.inner.registry).entries {
            if !entry.info.background {
                entry.cancel.cancel();
            }
        }
    }

    /// Cancels every child that has not ended, background ones too: the session is ending, or
    /// the user said to stop them.
    pub fn stop_all(&self) {
        self.inner.lifetime.cancel();
        for entry in &lock(&self.inner.registry).entries {
            entry.cancel.cancel();
        }
    }

    /// How many background children have not ended.
    pub fn live_background(&self) -> usize {
        lock(&self.inner.registry)
            .entries
            .iter()
            .filter(|e| e.info.background && !e.info.status.is_final())
            .count()
    }

    /// Children made by [`Supervisor::spawn`] that ask for isolation get a worktree from
    /// `worktrees`; without it they fail to start, saying why.
    pub fn set_worktrees(&self, worktrees: Arc<Worktrees>) {
        *lock(&self.inner.worktrees) = Some(worktrees);
    }

    /// The worktrees isolated children are made in, when there are any.
    pub fn worktrees(&self) -> Option<Arc<Worktrees>> {
        lock(&self.inner.worktrees).clone()
    }

    /// What the user should be told (a note about uncommitted work, a copy that stopped), each
    /// said once.
    pub fn take_notices(&self) -> Vec<String> {
        std::mem::take(&mut *lock(&self.inner.notices))
    }

    /// Waits until no child is still running (each has settled its worktree by then), for at most
    /// `limit`; whether that happened.
    pub async fn settled(&self, limit: std::time::Duration) -> bool {
        let mut changes = self.subscribe();
        let all_ended = || {
            lock(&self.inner.registry)
                .entries
                .iter()
                .all(|e| e.info.status.is_final())
        };
        tokio::time::timeout(limit, async {
            while !all_ended() {
                if changes.changed().await.is_err() {
                    break;
                }
            }
        })
        .await
        .is_ok()
    }

    /// Spawns a child under the parent's own token.
    pub fn spawn(&self, spec: AgentSpec) -> AgentHandle {
        let cancel = lock(&self.inner.parent).cancel.clone();
        self.spawn_under(spec, cancel)
    }

    /// Spawns a child that is cancelled when `parent_cancel` is: a tool call spawning it passes
    /// the turn's token. It waits for a place under the limits, then runs.
    pub fn spawn_under(&self, spec: AgentSpec, parent_cancel: CancellationToken) -> AgentHandle {
        // A background child answers to the supervisor, not to the turn that started it.
        let cancel = if spec.background {
            self.inner.lifetime.child_token()
        } else {
            parent_cancel.child_token()
        };
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
                    summary: None,
                    error: None,
                    started: None,
                    background: spec.background,
                    worktree: None,
                },
                cancel: cancel.clone(),
            });
            tag
        };
        self.inner.version.send_modify(|v| *v += 1);
        let (done, report) = watch::channel(None);
        let inner = self.inner.clone();
        let task_tag = tag.clone();
        let (workspace, parent_session) = {
            let parent = lock(&self.inner.parent);
            (
                spec.workspace.resolve(&parent.workspace),
                parent.session.clone(),
            )
        };
        self.inner.records.expect_report();
        let task_text = spec.task.clone();
        let task = tokio::spawn(run_child(
            inner.clone(),
            parent_session.clone(),
            task_tag.clone(),
            spec,
            cancel,
        ));
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
                        // Its worktree is settled too: what it had changed is committed, and
                        // the lock this process holds released.
                        let worktree = lock(&inner.registry)
                            .entries
                            .iter()
                            .find(|e| e.info.tag.id == task_tag.id)
                            .and_then(|e| e.info.worktree.as_ref().map(|w| w.name.clone()))
                            .and_then(|name| lock(&inner.worktrees).clone()?.worktree(&name));
                        let report = settle_worktree(
                            &inner,
                            &task_tag.id,
                            worktree.as_ref(),
                            &task_tag.label,
                            &task_text,
                            report,
                        )
                        .await;
                        finish_child(&inner, &parent_session, &task_tag, report, None)
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
    parent_session: &str,
    tag: &AgentTag,
    mut report: AgentReport,
    session: Option<String>,
) -> AgentReport {
    let id = &tag.id;
    report.session = session.clone();
    let entry = lock(&inner.registry)
        .entries
        .iter()
        .find(|e| &e.info.tag.id == id)
        .map(|e| e.info.clone());
    // What a worktree's record says, for a listing after a restart. Written before the status
    // changes: whoever the change wakes (the terminal's panel) reads the record at once, and must
    // find the result in it.
    if let (Some(info), Some(worktrees)) = (&entry, lock(&inner.worktrees).clone())
        && let Some(worktree) = &info.worktree
    {
        let _ = worktrees.update_record(&worktree.name, |record| {
            record.status = report.status;
            record.summary = Some(crate::agents::report::cap_chars(&report.summary, 400).0)
                .filter(|summary| !summary.is_empty());
            record.error = report.error.clone();
            record.session = session.clone();
            record.model = info.model.clone();
            record.tool_calls = report.tool_calls;
            record.outcome = report.worktree.clone();
        });
    }
    inner.update(id, |info| {
        info.status = report.status;
        info.summary = Some(crate::agents::report::cap_chars(&report.summary, 400).0)
            .filter(|summary| !summary.is_empty());
        info.error = report.error.clone();
        if let Some(worktree) = &mut info.worktree {
            worktree.outcome = report.worktree.clone();
        }
    });
    let _ = inner.events.send(AgentEnvelope::child(
        id,
        AgentEvent::AgentFinished {
            status: report.status,
            summary: crate::agents::report::cap_chars(&report.summary, 200).0,
            branch: report.worktree.as_ref().map(|o| o.branch.clone()),
        },
    ));
    inner.records.push(
        parent_session,
        AgentRecord {
            agent: id.to_string(),
            label: tag.label.clone(),
            kind: AgentRecordKind::Reported,
            session,
            report: serde_json::to_value(&report).ok(),
        },
    );
    report
}

/// What it takes to say a child's worktree is done with.
fn worktree_note(wt: &Worktree) -> String {
    format!(
        "You work in a git worktree of the repository, on the branch `{branch}`, made from commit {base}; the repository's other checkout and its uncommitted changes are not here. You can read git state (status, log, diff, show) but you cannot write it: do not commit, checkout, reset, stash, merge, rebase or push. Harness commits your changes on `{branch}` when you finish, and the user then reviews them. Your shell commands have HARNESS_AGENT_ID={agent} and HARNESS_PORT_OFFSET={offset}: add the offset to any port a server of yours listens on, so that agents working at the same time do not clash.",
        branch = wt.branch,
        base = &wt.base[..wt.base.len().min(10)],
        agent = wt.agent,
        offset = wt.port_offset,
    )
}

/// Puts an end to the worktree of a child that has ended: what it changed is committed on its
/// branch and the lock released; a worktree with no changes is removed with its branch. Puts
/// what it left in `report` (and an error, when the changes could not be committed: the
/// worktree then stays as it is).
async fn settle_worktree(
    inner: &Arc<Inner>,
    id: &AgentId,
    worktree: Option<&Worktree>,
    label: &str,
    task: &str,
    mut report: AgentReport,
) -> AgentReport {
    let (Some(worktree), Some(wts)) = (worktree, lock(&inner.worktrees).clone()) else {
        return report;
    };
    let (wt, label, task) = (worktree.clone(), label.to_string(), task.to_string());
    let settled = tokio::task::spawn_blocking(move || {
        let committed = wts.commit_result(&wt, &label, &task);
        let _ = wts.release(&wt);
        match committed {
            Ok(Some(outcome)) => (Some(outcome), None, false),
            Ok(None) => match wts.remove(&wt.name) {
                Ok(()) => (None, None, true),
                Err(e) => (
                    None,
                    Some(format!("its worktree could not be removed: {e}")),
                    false,
                ),
            },
            Err(e) => (
                None,
                Some(format!("its changes could not be committed: {e}")),
                false,
            ),
        }
    })
    .await;
    let (outcome, error, removed) = settled.unwrap_or((
        None,
        Some("its worktree could not be settled".into()),
        false,
    ));
    if removed {
        inner.update(id, |info| info.worktree = None);
    }
    report.worktree = outcome;
    if let Some(error) = error {
        report.error = Some(match report.error.take() {
            Some(first) => format!("{first}; {error}"),
            None => error,
        });
    }
    report
}

async fn run_child(
    inner: Arc<Inner>,
    parent_session: String,
    tag: AgentTag,
    mut spec: AgentSpec,
    cancel: CancellationToken,
) -> AgentReport {
    // The child belongs to the session its parent was in when it was spawned, whatever happens
    // to the parent meanwhile.
    let mut parent = lock(&inner.parent).clone();
    parent.session = parent_session;
    let id = tag.id.clone();
    let finish = |inner: &Inner, report: AgentReport, session: Option<String>| {
        finish_child(inner, &parent.session, &tag, report, session)
    };
    let main_workspace = spec.workspace.resolve(&parent.workspace);
    let empty = |status, error: Option<String>| {
        let mut report = AgentReport::build(
            id.as_str(),
            &tag.label,
            status,
            "",
            &ToolLog::default(),
            &main_workspace,
        );
        report.error = error;
        report
    };

    // A place under the limits; a parent that is cancelled while this waits ends it here, before
    // anything is built.
    let endpoint = inner.factory.endpoint(&spec);
    // An agent that runs in another program (Claude Code, Codex) is outside harness's sandbox, so
    // one that can edit never works in the checkout: it gets a worktree whatever its spec says.
    let delegated = endpoint.delegated;
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

    // A worktree of its own, when it asked for one and can write: made now that it has its place,
    // so a child that waits makes none. Its workspace, its prompt and its info are then the
    // worktree's.
    let live_mode = parent.policy.mode().unwrap_or(parent.engine.mode);
    let effective = spec.mode.map_or(live_mode, |own| live_mode.narrower(own));
    let mut worktree: Option<Worktree> = None;
    if (spec.isolation || delegated) && !matches!(effective, Mode::Plan | Mode::ReadOnly) {
        spec.isolation = true;
        let Some(wts) = lock(&inner.worktrees).clone() else {
            let why = if delegated {
                "could not start: an agent that runs in Claude Code or Codex and can edit works in a worktree, and that needs a git repository with a commit: there is none (a read-only agent needs no worktree)"
            } else {
                "could not start: it asked for a worktree, and isolation needs a git repository"
            };
            return finish(&inner, empty(AgentStatus::Failed, Some(why.into())), None);
        };
        let (agent, label, task) = (id.to_string(), tag.label.clone(), spec.task.clone());
        let made = {
            let wts = wts.clone();
            tokio::task::spawn_blocking(move || {
                wts.create(&WorktreeRequest {
                    agent: &agent,
                    label: &label,
                    task: &task,
                })
            })
            .await
        };
        let created = match made {
            Ok(Ok(created)) => created,
            Ok(Err(e)) => {
                return finish(
                    &inner,
                    empty(
                        AgentStatus::Failed,
                        Some(format!(
                            "could not start: its worktree could not be made: {e}"
                        )),
                    ),
                    None,
                );
            }
            Err(_) => {
                return finish(
                    &inner,
                    empty(
                        AgentStatus::Failed,
                        Some("could not start: its worktree could not be made".into()),
                    ),
                    None,
                );
            }
        };
        let made = created.worktree.clone();
        spec.workspace = ChildWorkspace::worktree(&made);
        let note = worktree_note(&made);
        spec.instructions = Some(match spec.instructions.take() {
            Some(first) => format!("{first}\n\n{note}"),
            None => note,
        });
        let background = spec.background;
        let _ = wts.update_record(&made.name, |record| record.background = background);
        let mut said = Vec::new();
        if created.said_uncommitted {
            said.push(format!(
                "{} file{} with changes not committed in the main checkout {} not in the agents' worktrees: a worktree starts from the last commit.",
                created.uncommitted,
                if created.uncommitted == 1 { "" } else { "s" },
                if created.uncommitted == 1 { "is" } else { "are" },
            ));
        }
        said.extend(created.included.notes.iter().cloned());
        for text in said {
            let _ = inner.events.send(AgentEnvelope::main(AgentEvent::Warning {
                message: text.clone(),
            }));
            lock(&inner.notices).push(text);
        }
        inner.update(&id, |info| {
            info.worktree = Some(WorktreeInfo {
                name: made.name.clone(),
                branch: made.branch.clone(),
                path: made.path.clone(),
                outcome: None,
            });
        });
        worktree = Some(made);
    }
    let workspace = spec.workspace.resolve(&parent.workspace);

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
    // Its record names the session from the start, so a crash does not lose the link.
    if let (Some(wt), Some(wts)) = (&worktree, lock(&inner.worktrees).clone()) {
        let session_id = session_id.clone();
        let _ = wts.update_record(&wt.name, |record| record.session = Some(session_id));
    }
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
    let mut tally = None;
    let meter: Option<Arc<dyn Meter>> = parent.meter.clone().map(|meter| {
        let scoped = ScopedMeter::new(
            meter,
            AgentScope {
                agent: id.to_string(),
                label: tag
                    .label
                    .chars()
                    .filter(|c| !c.is_control())
                    .take(40)
                    .collect(),
                parent: parent.session.clone(),
            },
        );
        tally = Some(scoped.tally());
        Arc::new(scoped) as Arc<dyn Meter>
    });
    inner.records.push(
        &parent.session,
        AgentRecord {
            agent: id.to_string(),
            label: tag.label.clone(),
            kind: AgentRecordKind::Spawned,
            session: Some(session_id.clone()),
            report: None,
        },
    );
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
            env: worktree.as_ref().map(Worktree::env).unwrap_or_default(),
        }) => built,
        _ = cancel.cancelled() => {
            drop(permit);
            let report = empty(AgentStatus::Cancelled, Some("cancelled while it started".into()));
            let report = settle_worktree(&inner, &id, worktree.as_ref(), &tag.label, &spec.task, report).await;
            return finish(&inner, report, Some(session_id));
        }
    };
    let mut agent = match built {
        Ok(agent) => agent,
        Err(why) => {
            drop(permit);
            let report = empty(AgentStatus::Failed, Some(format!("could not start: {why}")));
            let report = settle_worktree(
                &inner,
                &id,
                worktree.as_ref(),
                &tag.label,
                &spec.task,
                report,
            )
            .await;
            return finish(&inner, report, Some(session_id));
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
    report.usage = tally.and_then(|tally| tally.snapshot());
    let report = settle_worktree(
        &inner,
        &id,
        worktree.as_ref(),
        &tag.label,
        &spec.task,
        report,
    )
    .await;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_child_that_ended_without_a_report_is_failed_and_says_why() {
        let tag = AgentTag {
            id: AgentId("a1".into()),
            label: "explore".into(),
        };
        let report = ended_without_report(&tag);
        assert_eq!(report.status, AgentStatus::Failed);
        assert_eq!(
            report.error.as_deref(),
            Some("the subagent ended without a report")
        );
    }
}
