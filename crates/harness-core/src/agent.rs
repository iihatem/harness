//! The agent loop: call the model, run the tools it requests, feed results back, repeat.

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{Value, json};
use tokio::{sync::mpsc::UnboundedSender, time::Instant};
use tokio_util::sync::CancellationToken;

mod gates;

use crate::{
    checkpoint::{CheckpointError, Checkpoints},
    compaction::{self, CompactionConfig},
    event::{AgentEvent, ErrorKind, TurnEndReason},
    gate::Gates,
    message::{ChatRequest, Message, RequestOptions, ToolCall, Usage},
    meter::{AccountKind, GateCounts, MAIN_ROLE, Meter, RequestRecord, TurnRecord},
    output::{DEFAULT_OUTPUT_LIMIT, limit_output},
    permission::{Action, Decision, FsAccess, Mode, PermissionPolicy},
    provider::{FinishReason, Provider, ProviderError, ProviderEvent},
    redact::Redactor,
    retry::RetryPolicy,
    session::{Entry, EntryKind, RewindScope, Session},
    tokens::DEFAULT_CONTEXT_WINDOW,
    tool::{CommandSandbox, Tool, ToolContext, ToolOutput, ToolRegistry},
    turn::{InputPart, Steering, TurnInput, TurnModel},
};

/// Model calls allowed per turn unless configured otherwise.
pub const DEFAULT_MAX_STEPS: u32 = 50;

/// The result of each tool call of a reply the output limit cut off: the call is not run.
pub const CUT_OFF_CALL: &str = "not run: your reply was cut off at the output-token limit, so this call may be incomplete. Continue in smaller steps: write a large file in parts (write the start, then add the rest with edit), and make one change per call.";

/// The note after a reply without tool calls that the output limit cut off.
pub const CUT_OFF_REPLY: &str = "[harness] Your last reply was cut off at the output-token limit. Continue exactly where it stopped, in smaller steps.";

/// What the rewind list says about effects a rewind cannot undo.
pub const REWIND_LIMITS: &str = "Rewinding restores files in the workspace only: network calls, databases, pushed commits, files outside the workspace, and what is inside nested git repositories and submodules stay as they are. Files that checkpoints leave out (git-ignored files, files over 10 MB, node_modules and target) are neither restored nor removed.";

/// A user message the conversation can be rewound to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewindPoint {
    /// The session entry of the message.
    pub entry: String,
    /// What the user typed.
    pub text: String,
}

#[derive(Debug, thiserror::Error)]
pub enum RewindError {
    #[error("there is no user message {0} in this conversation")]
    UnknownPoint(String),
    #[error("checkpoints are disabled for this session, so files cannot be restored")]
    NoCheckpoints,
    #[error("there is no rewind to undo")]
    NothingToUndo,
    #[error(
        "the files were checkpointed in {}, not in this directory; rewind them from there",
        .0.display()
    )]
    OtherWorkspace(PathBuf),
    #[error(
        "files were changed after this message while checkpoints were off, so they cannot be restored to before it; rewind the conversation only, or pick a later message"
    )]
    Unrecorded,
    #[error("restoring files failed: {0}")]
    Restore(#[from] CheckpointError),
}

/// Settings for one agent session.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// `<provider>/<model>`, recorded on every assistant message.
    pub model_id: String,
    /// The model name sent to the provider.
    pub model_name: String,
    pub system_prompt: String,
    pub max_steps: u32,
    pub output_limit: usize,
    pub output_dir: PathBuf,
    pub retry: RetryPolicy,
    /// The model's context window in tokens.
    pub context_window: u64,
    pub compaction: CompactionConfig,
    /// Output limit, temperature and reasoning effort for every request to the session's model.
    pub request: RequestOptions,
    /// Run tool calls the model writes as text (`textcalls`): for local models.
    pub text_tool_calls: bool,
}

impl AgentConfig {
    pub fn new(
        model_id: impl Into<String>,
        model_name: impl Into<String>,
        system_prompt: impl Into<String>,
        output_dir: PathBuf,
    ) -> Self {
        AgentConfig {
            model_id: model_id.into(),
            model_name: model_name.into(),
            system_prompt: system_prompt.into(),
            max_steps: DEFAULT_MAX_STEPS,
            output_limit: DEFAULT_OUTPUT_LIMIT,
            output_dir,
            retry: RetryPolicy::default(),
            context_window: DEFAULT_CONTEXT_WINDOW,
            compaction: CompactionConfig::default(),
            request: RequestOptions::default(),
            text_tool_calls: false,
        }
    }
}

/// A model the session continues on after `/model`: its provider, its ids, and what its profile
/// and window say about requests to it.
#[derive(Clone)]
pub struct SessionModel {
    pub provider: Arc<dyn Provider>,
    /// `<provider>/<model>`, recorded on its assistant messages.
    pub id: String,
    /// The model name sent to the provider.
    pub name: String,
    pub context_window: u64,
    pub request: RequestOptions,
    pub text_tool_calls: bool,
}

impl std::fmt::Debug for SessionModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionModel")
            .field("id", &self.id)
            .field("context_window", &self.context_window)
            .finish()
    }
}

/// The OS sandbox shell commands get in each mode, for a session whose mode can change: one for
/// read-only access (`plan`, `read-only`) and one for workspace-write access (`ask`, `auto`).
/// Either may be missing: a workspace too broad to make writable has no workspace-write sandbox,
/// and neither has a system without one. `full-access` never uses one.
#[derive(Debug, Clone, Default)]
pub struct Sandboxes {
    pub read_only: Option<Arc<dyn CommandSandbox>>,
    pub workspace_write: Option<Arc<dyn CommandSandbox>>,
}

impl Sandboxes {
    /// The sandbox for `mode`.
    pub fn for_mode(&self, mode: Mode) -> Option<Arc<dyn CommandSandbox>> {
        match mode {
            Mode::Plan | Mode::ReadOnly => self.read_only.clone(),
            Mode::Ask | Mode::Auto => self.workspace_write.clone(),
            Mode::FullAccess => None,
        }
    }
}

/// What an approval decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalKind {
    /// Whether the action may run: once, for the rest of the session, or not.
    Action,
    /// Whether a command may run without the sandbox: once, or not. It is never approved for
    /// the session.
    RunUnsandboxed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRequest {
    pub call_id: String,
    pub tool: String,
    /// The call's arguments as the model sent them, before any redaction: what an approver shows
    /// of the change must be worked out from them, and only what it shows redacted.
    pub arguments: Value,
    pub action: Action,
    pub reason: String,
    pub kind: ApprovalKind,
    /// Whether approving it for the session would be kept. When it would not (destructive
    /// commands, and others the policy always asks about), such an approval applies once.
    pub kept_for_session: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalDecision {
    Approve,
    /// Approve, and remember the approval for similar actions for the rest of the session.
    ApproveForSession,
    Deny {
        feedback: Option<String>,
    },
    /// Nobody can answer (headless run): the action is blocked.
    Unavailable,
}

#[async_trait]
pub trait Approver: Send + Sync {
    async fn decide(&self, request: &ApprovalRequest) -> ApprovalDecision;
}

/// Used when nobody can answer prompts (e.g. `harness ask`): every approval is unavailable.
pub struct NonInteractive;

#[async_trait]
impl Approver for NonInteractive {
    async fn decide(&self, _request: &ApprovalRequest) -> ApprovalDecision {
        ApprovalDecision::Unavailable
    }
}

/// Where the next request's tokens go, estimated, and the model's context window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ContextUsage {
    /// The context window, in tokens.
    pub window: u64,
    /// The system prompt, instruction files and environment included.
    pub system: u64,
    /// The tool definitions.
    pub tools: u64,
    /// The conversation.
    pub messages: u64,
    /// The whole next request: from the input tokens the provider reported for the last one
    /// where it did, else the sum of the estimates above.
    pub total: u64,
}

/// What a turn's model calls took, for its [`AgentEvent::TurnStats`].
#[derive(Debug, Default)]
struct Stats {
    /// The model that answered last; `None` until one was asked.
    model: Option<String>,
    time_to_first_token: Option<Duration>,
    generation: Duration,
    usage: Usage,
    /// Tool calls run in the turn.
    tool_calls: u32,
}

/// What one model call produced so far. Kept outside the stream future so partial output survives.
#[derive(Debug, Default)]
struct ModelReply {
    text: String,
    tool_calls: Vec<ToolCall>,
    finish: Option<FinishReason>,
    /// Whether any output was already shown to the user (then the call must not be retried).
    emitted: bool,
    /// The token counts the provider reported for this call.
    usage: Option<Usage>,
    /// When the request was sent, when the first output arrived, and when the stream ended.
    started: Option<Instant>,
    first_output: Option<Instant>,
    ended: Option<Instant>,
    /// How much of `text` was sent as text deltas. With text tool calls on, text that may still
    /// turn out to be calls is held back, and shown once it cannot (see [`Self::show`]).
    shown: usize,
    watch: crate::textcalls::CallWatch,
}

impl ModelReply {
    /// Sends the text not shown yet.
    fn show(&mut self, events: &UnboundedSender<AgentEvent>) {
        if self.shown < self.text.len() {
            let text = self.text[self.shown..].to_string();
            self.shown = self.text.len();
            self.emitted = true;
            let _ = events.send(AgentEvent::TextDelta { text });
        }
    }
}

/// Why the conversation is being compacted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Trigger {
    /// Estimated usage reached the threshold.
    Auto,
    /// The user asked (`/compact`).
    Manual,
    /// The provider rejected a request as longer than the context window.
    Overflow,
}

/// Why the conversation was not compacted.
#[derive(Debug)]
enum CompactError {
    /// Nothing before the part that is kept but, at most, an earlier summary.
    NothingToCompact,
    Interrupted,
    /// The summary request was longer than the model's context window; the text says so.
    Overflow(String),
    /// The summary request failed; the text says why.
    Failed(String),
}

impl std::fmt::Display for CompactError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompactError::NothingToCompact => f.write_str("there is nothing to compact yet"),
            CompactError::Interrupted => f.write_str("interrupted"),
            CompactError::Overflow(why) | CompactError::Failed(why) => f.write_str(why),
        }
    }
}

/// How one model call (including its retries) ended.
enum ModelOutcome {
    Reply(ModelReply),
    Failed(ProviderError, ModelReply),
    Interrupted(ModelReply),
}

pub struct Agent {
    provider: Arc<dyn Provider>,
    tools: ToolRegistry,
    policy: Arc<dyn PermissionPolicy>,
    approver: Arc<dyn Approver>,
    config: AgentConfig,
    ctx: ToolContext,
    /// Where the conversation is saved; its active branch is `history`.
    session: Session,
    history: Vec<Message>,
    /// The session entry of each message in `history`.
    history_ids: Vec<String>,
    /// Warnings to report at the end of the current (or next) turn.
    warnings: Vec<String>,
    /// Snapshots of the workspace; `None` when checkpoints are disabled.
    checkpoints: Option<Arc<Checkpoints>>,
    /// Whether the current turn already took its snapshot.
    turn_checkpointed: bool,
    /// Whether the current turn's user message is saved yet. A slash command's shell parts run
    /// before it is, and entries about the turn they make wait in `held_entries`, to be saved
    /// just after it: rewinding to the message looks for them after it.
    message_recorded: bool,
    held_entries: Vec<EntryKind>,
    /// Input tokens the provider reported for the last request, and how many messages it had;
    /// `None` until a provider reports usage, and after the history changes.
    reported_usage: Option<(u64, usize)>,
    validators: HashMap<String, jsonschema::Validator>,
    invalid_calls: u32,
    /// Tool-call ids already used in this session, so a missing or repeated id (from a model or
    /// provider that doesn't guarantee unique ids) can be rewritten before it collides.
    used_call_ids: HashSet<String>,
    next_call_id: u64,
    /// The model answering the current turn, when it is not the session's.
    turn_model: Option<TurnModel>,
    /// Set when an automatic compaction left the estimate at or over the threshold: automatic
    /// compaction then waits until the estimate drops below it, rather than repeat at every step
    /// without shrinking anything.
    auto_compaction_paused: bool,
    /// Keeps secrets out of the session file and tool-output files.
    redactor: Option<Arc<Redactor>>,
    /// The current turn's model calls, for its stats.
    stats: Stats,
    /// The sandbox for each mode, when switching modes also switches the sandbox.
    sandboxes: Option<Sandboxes>,
    /// Input the user sends while a turn runs.
    steering: Option<Steering>,
    /// Told about every model request, for the usage ledger.
    meter: Option<Arc<dyn Meter>>,
    /// The current turn: when it started (for its duration, and as seconds since the epoch), and
    /// its user message's entry.
    turn_started: Option<(Instant, u64)>,
    turn_entry: String,
    /// Retries made in the current turn.
    retry_count: std::sync::atomic::AtomicU32,
    /// Whether the user switched the session's model with `/model`.
    model_chosen_by_user: bool,
    /// The verification gates; none runs unless one is configured.
    gates: Gates,
    /// Whether an edit tool changed a file in the current turn.
    turn_changed: bool,
    /// Gate commands run so far, for their call ids.
    gate_calls: u64,
    /// What the end-of-turn test gate has done in the current turn.
    gate_turn: gates::GateTurn,
}

impl Agent {
    pub fn new(
        provider: Arc<dyn Provider>,
        tools: ToolRegistry,
        policy: Arc<dyn PermissionPolicy>,
        approver: Arc<dyn Approver>,
        config: AgentConfig,
        ctx: ToolContext,
    ) -> Self {
        let validators = tools
            .specs()
            .into_iter()
            .map(|spec| {
                let validator = jsonschema::validator_for(&spec.parameters)
                    .unwrap_or_else(|e| panic!("tool `{}` has an invalid schema: {e}", spec.name));
                (spec.name, validator)
            })
            .collect();
        Agent {
            provider,
            tools,
            policy,
            approver,
            config,
            session: Session::in_memory(&ctx.workspace),
            ctx,
            history: Vec::new(),
            history_ids: Vec::new(),
            warnings: Vec::new(),
            checkpoints: None,
            turn_checkpointed: false,
            message_recorded: true,
            held_entries: Vec::new(),
            reported_usage: None,
            validators,
            invalid_calls: 0,
            used_call_ids: HashSet::new(),
            next_call_id: 0,
            turn_model: None,
            auto_compaction_paused: false,
            redactor: None,
            stats: Stats::default(),
            sandboxes: None,
            steering: None,
            meter: None,
            turn_started: None,
            turn_entry: String::new(),
            retry_count: std::sync::atomic::AtomicU32::new(0),
            model_chosen_by_user: false,
            gates: Gates::default(),
            turn_changed: false,
            gate_calls: 0,
            gate_turn: gates::GateTurn::default(),
        }
    }

    /// Reports every model request to `meter`: the turn's requests, a failed or stopped one, and
    /// the summary requests of compaction.
    pub fn with_meter(mut self, meter: Arc<dyn Meter>) -> Self {
        self.meter = Some(meter);
        self
    }

    /// Tells the meter that the turn ended as `reason`, with `stats`.
    fn meter_turn(&self, reason: TurnEndReason, stats: &Stats) {
        let Some(meter) = &self.meter else {
            return;
        };
        let (started, started_at) = self
            .turn_started
            .unwrap_or_else(|| (Instant::now(), crate::time::now_unix()));
        meter.record_turn(&TurnRecord {
            session: self.session.id().to_string(),
            turn: self.turn_entry.clone(),
            role: MAIN_ROLE.to_string(),
            model: self.model_id().to_string(),
            selected_by: if self.model_chosen_by_user {
                "user"
            } else {
                "config"
            }
            .to_string(),
            input_tokens: stats.usage.input_tokens,
            output_tokens: stats.usage.output_tokens,
            first_token_ms: stats.time_to_first_token.map(|d| d.as_millis() as u64),
            duration_ms: started.elapsed().as_millis() as u64,
            tool_calls: stats.tool_calls,
            invalid_calls: self.invalid_calls,
            retries: self.retry_count.load(std::sync::atomic::Ordering::Relaxed),
            finish_reason: reason.as_str().to_string(),
            started_at,
            ended_at: crate::time::now_unix(),
            gates: GateCounts::default(),
        });
    }

    /// Asks the meter whether the budgets allow the next request: says each 80% warning, and
    /// whether one is reached (said as an event too).
    fn budget_reached(&self, events: &UnboundedSender<AgentEvent>, said_paused: &mut bool) -> bool {
        let Some(meter) = &self.meter else {
            return false;
        };
        let local = match &self.turn_model {
            Some(turn) => turn.options().local,
            None => self.config.request.local,
        };
        let account = AccountKind::of(self.model_id(), local);
        let status = meter.check_budget(self.session.id(), account);
        for notice in status.warnings {
            let _ = events.send(AgentEvent::BudgetWarning { notice });
        }
        for message in meter.take_warnings() {
            let _ = events.send(AgentEvent::Warning { message });
        }
        if let Some(notice) = status.paused
            && !std::mem::replace(said_paused, true)
        {
            let _ = events.send(AgentEvent::Warning {
                message: notice.paused_message(),
            });
        }
        match status.stop {
            Some(notice) => {
                let _ = events.send(AgentEvent::BudgetReached { notice });
                true
            }
            None => false,
        }
    }

    /// Tells the meter, if there is one, that a model request ended as `outcome` (`ok`, or
    /// `error:<kind>`) with `usage`, `started` being when it was sent. What the meter could not
    /// keep is shown as warnings.
    fn meter_request(
        &self,
        outcome: String,
        usage: Usage,
        started: Instant,
        events: &UnboundedSender<AgentEvent>,
    ) {
        let Some(meter) = &self.meter else {
            return;
        };
        let local = match &self.turn_model {
            Some(turn) => turn.options().local,
            None => self.config.request.local,
        };
        let cost = meter.record_request(&RequestRecord {
            session: self.session.id().to_string(),
            role: MAIN_ROLE.to_string(),
            model: self.model_id().to_string(),
            local,
            usage,
            duration: started.elapsed(),
            outcome,
        });
        let _ = events.send(AgentEvent::Metered {
            model: self.model_id().to_string(),
            cost,
        });
        for message in meter.take_warnings() {
            let _ = events.send(AgentEvent::Warning { message });
        }
    }

    /// Ensures every call in `calls` has a non-empty id not already used in this session,
    /// rewriting any empty or repeated id to a fresh `call_h{n}`. The rewritten id is used
    /// everywhere downstream (events, history, spill file), so two calls never collide.
    fn dedupe_call_ids(&mut self, calls: &mut [ToolCall]) {
        for call in calls.iter_mut() {
            if call.id.is_empty() || self.used_call_ids.contains(&call.id) {
                loop {
                    let candidate = format!("call_h{}", self.next_call_id);
                    self.next_call_id += 1;
                    if !self.used_call_ids.contains(&candidate) {
                        call.id = candidate;
                        break;
                    }
                }
            }
            self.used_call_ids.insert(call.id.clone());
        }
    }

    /// Saves the conversation in `session` from now on, continuing its active branch. Tool calls
    /// a stopped run left without results at the end of it get results, saved in the session.
    pub fn with_session(mut self, session: Session) -> Self {
        self.session = session;
        if let Some(redactor) = &self.redactor {
            self.session.set_redactor(redactor.clone());
        }
        self.load_history(true);
        self
    }

    /// Keeps the secrets `redactor` knows out of the session file and tool-output files. The
    /// model is still sent everything as it is.
    pub fn with_redactor(mut self, redactor: Arc<Redactor>) -> Self {
        self.session.set_redactor(redactor.clone());
        self.redactor = Some(redactor);
        self
    }

    /// Rebuilds the history from the session's active branch. Every tool call needs a result, or
    /// providers reject the request, but a run that was killed while a tool ran left none: such
    /// a call gets one saying its effects are unknown. At the end of the branch that result is
    /// saved when `save` is set; elsewhere, and otherwise, it exists only in the history, where
    /// it shares its call's entry id.
    fn load_history(&mut self, save: bool) {
        let mut history = Vec::new();
        let mut ids = Vec::new();
        // The calls of the last assistant message still waiting for a result.
        let mut waiting: Vec<String> = Vec::new();
        let mut answered = 0;
        for (id, message) in self.session.messages() {
            if let Message::Tool { call_id, .. } = &message {
                waiting.retain(|c| c != call_id);
            } else {
                for call_id in waiting.drain(..) {
                    history.push(stopped_result(call_id));
                    ids.push(ids.last().cloned().unwrap_or_default());
                    answered += 1;
                }
            }
            if let Message::Assistant { tool_calls, .. } = &message {
                self.used_call_ids
                    .extend(tool_calls.iter().map(|c| c.id.clone()));
                waiting = tool_calls.iter().map(|c| c.id.clone()).collect();
            }
            history.push(message);
            ids.push(id);
        }
        self.history = history;
        self.history_ids = ids;
        self.reported_usage = None;
        for call_id in waiting {
            if save {
                self.record(stopped_result(call_id), None, false);
            } else {
                self.history.push(stopped_result(call_id));
                let id = self.history_ids.last().cloned().unwrap_or_default();
                self.history_ids.push(id);
            }
            answered += 1;
        }
        if answered > 0 && save {
            self.warnings.push(format!(
                "harness stopped before {answered} tool call(s) in this conversation finished; the model is told their effects are unknown"
            ));
        }
    }

    /// Gives shell commands the sandbox for the mode whenever the mode changes
    /// ([`set_mode`](Self::set_mode)). Without it, a mode change keeps the sandbox the agent
    /// started with.
    pub fn with_sandboxes(mut self, sandboxes: Sandboxes) -> Self {
        self.sandboxes = Some(sandboxes);
        self
    }

    /// Gives the model what the user sends through `steering` while a turn runs, with the
    /// results of the next tool calls.
    /// Runs `gates`: the after-edit command after each successful edit.
    pub fn with_gates(mut self, gates: Gates) -> Self {
        self.gates = gates;
        self
    }

    pub fn with_steering(mut self, steering: Steering) -> Self {
        self.steering = Some(steering);
        self
    }

    /// Snapshots the workspace before each turn's first change, so it can be rewound.
    pub fn with_checkpoints(mut self, checkpoints: Option<Arc<Checkpoints>>) -> Self {
        self.checkpoints = checkpoints;
        self
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    /// Continues in `session` from now on, as `/new` and `/resume` do: its active branch becomes
    /// the conversation, and `checkpoints` its snapshots. The model, the mode, the tools and the
    /// system prompt stay as they are, so providers keep their prompt caches. The session left
    /// is released, for another process to continue.
    pub fn start_session(&mut self, session: Session, checkpoints: Option<Arc<Checkpoints>>) {
        self.session = session;
        if let Some(redactor) = &self.redactor {
            self.session.set_redactor(redactor.clone());
        }
        self.checkpoints = checkpoints;
        self.turn_checkpointed = false;
        self.message_recorded = true;
        self.held_entries.clear();
        self.invalid_calls = 0;
        self.used_call_ids.clear();
        self.turn_model = None;
        self.auto_compaction_paused = false;
        self.stats = Stats::default();
        self.load_history(true);
    }

    /// The user messages on the active branch, oldest first, for the rewind list. Notes from
    /// harness are left out.
    pub fn rewind_points(&self) -> Vec<RewindPoint> {
        self.session
            .branch()
            .into_iter()
            .filter_map(|entry| match &entry.kind {
                EntryKind::Message {
                    message: Message::User { content },
                    display,
                    note: false,
                    ..
                } => Some(RewindPoint {
                    entry: entry.id.clone(),
                    text: display.clone().unwrap_or_else(|| content.clone()),
                }),
                _ => None,
            })
            .collect()
    }

    /// The plan the user last approved on the active branch, if any.
    pub fn approved_plan(&self) -> Option<String> {
        self.session
            .branch()
            .into_iter()
            .rev()
            .find_map(|entry| match &entry.kind {
                EntryKind::Message {
                    plan: Some(plan), ..
                } => Some(plan.clone()),
                _ => None,
            })
    }

    /// Whether the rewind list offers "undo last rewind": nothing has happened since the last
    /// rewind.
    pub fn can_undo_rewind(&self) -> bool {
        self.session
            .get(self.session.leaf())
            .is_some_and(|e| matches!(e.kind, EntryKind::Rewind { .. }))
    }

    /// Rewinds to just before the user message `entry`: restores the workspace files, the
    /// conversation, or both, as they were then. Restoring files first snapshots the workspace,
    /// and rewinding the conversation starts a new branch, so the rewind can be undone.
    pub async fn rewind(&mut self, entry: &str, scope: RewindScope) -> Result<(), RewindError> {
        let branch: Vec<Entry> = self.session.branch().into_iter().cloned().collect();
        let position = self
            .rewind_points()
            .iter()
            .any(|p| p.entry == entry)
            .then(|| branch.iter().position(|e| e.id == entry))
            .flatten()
            .ok_or_else(|| RewindError::UnknownPoint(entry.to_string()))?;
        let mut snapshot = None;
        if scope != RewindScope::Conversation {
            let checkpoints = self.checkpoints.clone().ok_or(RewindError::NoCheckpoints)?;
            // The workspace before that message is the first snapshot taken at or after it; with
            // none, no change was made since. A turn before it that changed files without one
            // leaves a gap that no snapshot covers.
            let checkpoint = branch[position..].iter().find_map(|e| match &e.kind {
                EntryKind::Checkpoint { commit, workspace } => Some(Some((commit, workspace))),
                EntryKind::NoCheckpoint => Some(None),
                _ => None,
            });
            if checkpoint == Some(None) {
                return Err(RewindError::Unrecorded);
            }
            if let Some((commit, workspace)) = checkpoint.flatten() {
                if let Some(taken) = workspace
                    .as_ref()
                    .filter(|w| **w != checkpoints.workspace())
                {
                    return Err(RewindError::OtherWorkspace(taken.clone()));
                }
                let commit = commit.clone();
                let restored =
                    tokio::task::spawn_blocking(move || checkpoints.restore(&commit)).await;
                match restored.map_err(|e| CheckpointError::Io(e.into()))? {
                    Ok(before) => snapshot = Some(before),
                    Err(CheckpointError::Restore { before, source }) => {
                        // Files may be half restored: recorded as a rewind of code, undoing it
                        // puts them back as they were. The conversation stays.
                        let from = self.session.leaf().to_string();
                        self.session.append(EntryKind::Rewind {
                            from,
                            target: entry.to_string(),
                            scope: RewindScope::Code,
                            snapshot: Some(before.clone()),
                        });
                        self.after_session_change();
                        return Err(CheckpointError::Restore { before, source }.into());
                    }
                    Err(e) => return Err(e.into()),
                }
            }
        }
        let from = self.session.leaf().to_string();
        let parent = match scope {
            RewindScope::Code => from.clone(),
            _ => branch[position]
                .parent_id
                .clone()
                .expect("a user message has a parent"),
        };
        self.session.append_under(
            &parent,
            EntryKind::Rewind {
                from,
                target: entry.to_string(),
                scope,
                snapshot,
            },
        );
        self.after_session_change();
        // The turns from that message on are the ones undone.
        if let Some(meter) = &self.meter {
            let undone: Vec<String> = branch[position..]
                .iter()
                .filter(|e| {
                    matches!(
                        &e.kind,
                        EntryKind::Message {
                            message: Message::User { .. },
                            note: false,
                            ..
                        }
                    )
                })
                .map(|e| e.id.clone())
                .collect();
            meter.turns_rewound(self.session.id(), &undone);
        }
        Ok(())
    }

    /// Undoes the last rewind, when nothing has happened since: restores the files and the
    /// conversation as they were just before it. An undo whose restore fails partway is recorded
    /// as a rewind of code, which can be undone in turn.
    pub async fn undo_rewind(&mut self) -> Result<(), RewindError> {
        let Some(Entry {
            id,
            kind:
                EntryKind::Rewind {
                    from,
                    target,
                    snapshot,
                    ..
                },
            ..
        }) = self.session.get(self.session.leaf()).cloned()
        else {
            return Err(RewindError::NothingToUndo);
        };
        let mut before = None;
        if let Some(snapshot) = snapshot {
            let checkpoints = self.checkpoints.clone().ok_or(RewindError::NoCheckpoints)?;
            let restored =
                tokio::task::spawn_blocking(move || checkpoints.restore(&snapshot)).await;
            match restored.map_err(|e| CheckpointError::Io(e.into()))? {
                Ok(snapshot) => before = Some(snapshot),
                Err(CheckpointError::Restore { before, source }) => {
                    // Files may be half restored: recorded as a rewind of code, undoing it puts
                    // them back as they were, and this rewind can then be undone again.
                    self.session.append(EntryKind::Rewind {
                        from: id,
                        target,
                        scope: RewindScope::Code,
                        snapshot: Some(before.clone()),
                    });
                    self.after_session_change();
                    return Err(CheckpointError::Restore { before, source }.into());
                }
                Err(e) => return Err(e.into()),
            }
        }
        // The snapshot taken just before is kept, so changes made since the rewind (in an
        // editor, say) are not lost.
        let undone = self.session.append_under(
            &from,
            EntryKind::UndoRewind {
                rewind: id,
                snapshot: before,
            },
        );
        // Back where `from` left the session. When that was just after another rewind, nothing
        // has happened since that one, so it can still be undone.
        if let Some(Entry {
            kind: rewind @ EntryKind::Rewind { .. },
            ..
        }) = self.session.get(&from).cloned()
        {
            self.session.append_under(&undone, rewind);
        }
        self.after_session_change();
        Ok(())
    }

    /// Reloads the history after the active branch moved, and notes a failure to save.
    fn after_session_change(&mut self) {
        self.load_history(false);
        self.note_save_error();
    }

    /// Queues a warning, once, when the session file could not be written, and the session's
    /// warnings about saving.
    fn note_save_error(&mut self) {
        self.warnings.extend(self.session.take_warnings());
        if let Some(e) = self.session.take_save_error() {
            self.warnings.push(format!(
                "cannot save the session: {e}; the conversation continues but is no longer saved"
            ));
        }
    }

    /// Whether running `action` may change the workspace: a file write, or a shell command in a
    /// mode that lets commands write.
    fn is_mutating(&self, action: &Action) -> bool {
        match action {
            Action::Write(_) => true,
            Action::Bash(_) => self.ctx.access == FsAccess::WorkspaceWrite,
            Action::Read(_) => false,
        }
    }

    /// Snapshots the workspace before the turn's first change. A snapshot that fails or takes
    /// too long disables checkpoints for the rest of the session, with a warning; the turn goes on.
    async fn checkpoint(&mut self, events: &UnboundedSender<AgentEvent>) {
        if self.turn_checkpointed {
            return;
        }
        self.turn_checkpointed = true;
        // Without a snapshot, the session notes that files may have changed here, so no rewind
        // restores code across this turn as if they had not.
        let Some(checkpoints) = self.checkpoints.clone() else {
            self.append_turn_entry(EntryKind::NoCheckpoint);
            return;
        };
        let message = format!("before a turn of session {}", self.session.id());
        let workspace = checkpoints.workspace().to_path_buf();
        let result = tokio::task::spawn_blocking(move || checkpoints.snapshot(&message)).await;
        match result {
            Ok(Ok(commit)) => {
                self.append_turn_entry(EntryKind::Checkpoint {
                    commit: commit.clone(),
                    workspace: Some(workspace),
                });
                let _ = events.send(AgentEvent::CheckpointCreated { commit });
            }
            Ok(Err(e)) => self.disable_checkpoints(&e.to_string(), events),
            Err(e) => self.disable_checkpoints(&e.to_string(), events),
        }
    }

    /// Saves `kind`, an entry about the current turn, after the turn's user message; until that
    /// is saved, it waits.
    fn append_turn_entry(&mut self, kind: EntryKind) {
        if self.message_recorded {
            self.session.append(kind);
            self.note_save_error();
        } else {
            self.held_entries.push(kind);
        }
    }

    fn disable_checkpoints(&mut self, why: &str, events: &UnboundedSender<AgentEvent>) {
        self.checkpoints = None;
        self.append_turn_entry(EntryKind::NoCheckpoint);
        let _ = events.send(AgentEvent::Warning {
            message: format!("checkpoints are disabled for this session: {why}"),
        });
    }

    pub fn history(&self) -> &[Message] {
        &self.history
    }

    /// Adds `message` to the history and saves it in the session. If the session file cannot be
    /// written, the conversation continues in memory and a warning says so once.
    fn record(&mut self, message: Message, display: Option<String>, note: bool) {
        self.record_entry(message, display, note, None);
    }

    /// [`record`](Self::record), with the plan the message asks to build.
    fn record_entry(
        &mut self,
        message: Message,
        display: Option<String>,
        note: bool,
        plan: Option<String>,
    ) -> String {
        let id = self.session.append(EntryKind::Message {
            message: message.clone(),
            display,
            note,
            plan,
        });
        self.history.push(message);
        self.history_ids.push(id.clone());
        self.note_save_error();
        id
    }

    pub fn config_mut(&mut self) -> &mut AgentConfig {
        &mut self.config
    }

    /// Where the next request's tokens would go.
    pub fn context_usage(&self) -> ContextUsage {
        let system = crate::tokens::estimate(&self.config.system_prompt);
        let tools = compaction::request_tokens("", &self.tools.specs(), &[]);
        let messages = self.history.iter().map(compaction::message_tokens).sum();
        ContextUsage {
            window: self.config.context_window,
            system,
            tools,
            messages,
            total: self.estimated_tokens(),
        }
    }

    /// Invalid tool calls (unknown tool, bad JSON, schema violations) in the current or last turn.
    pub fn invalid_calls_this_turn(&self) -> u32 {
        self.invalid_calls
    }

    /// Continues on `model` from the next request, as `/model` does between turns. The
    /// conversation stays as it is: it is kept in a form every provider takes, and each adapter
    /// leaves out what its provider cannot accept. The next request's size is estimated afresh,
    /// since the new model counts tokens its own way, and it is compacted against the new window.
    pub fn switch_model(&mut self, model: SessionModel) {
        self.provider = model.provider;
        self.config.model_id = model.id;
        self.config.model_name = model.name;
        self.config.context_window = model.context_window;
        self.config.request = model.request;
        self.config.text_tool_calls = model.text_tool_calls;
        self.reported_usage = None;
        self.auto_compaction_paused = false;
        self.model_chosen_by_user = true;
    }

    /// Switches the approval mode between turns, and with [`with_sandboxes`](Self::with_sandboxes)
    /// the sandbox with it. The system prompt stays as it is, so providers keep reusing their
    /// prompt caches; the change is appended to the conversation as a note.
    pub fn set_mode(&mut self, mode: Mode) {
        if let Some(sandboxes) = &self.sandboxes {
            self.ctx.sandbox = sandboxes.for_mode(mode);
            self.policy
                .set_sandbox_available(self.ctx.sandbox.is_some());
        }
        self.policy.set_mode(mode);
        self.ctx.access = mode.fs_access();
        self.record(
            Message::User {
                content: mode_note(mode, self.ctx.sandbox.is_some()),
            },
            None,
            true,
        );
    }

    /// Runs one user turn to completion, reporting everything on `events`. Cancelling `cancel` stops the
    /// turn promptly: in-flight model calls are dropped and running tools are told to stop.
    pub async fn run_turn(
        &mut self,
        input: impl Into<TurnInput>,
        events: &UnboundedSender<AgentEvent>,
        cancel: CancellationToken,
    ) -> TurnEndReason {
        let input = input.into();
        self.ctx.cancel = cancel.clone();
        self.invalid_calls = 0;
        self.stats = Stats::default();
        self.retry_count
            .store(0, std::sync::atomic::Ordering::Relaxed);
        self.turn_started = Some((Instant::now(), crate::time::now_unix()));
        self.turn_checkpointed = false;
        self.turn_changed = false;
        self.gate_turn = gates::GateTurn::default();
        // Settings that apply to this turn only.
        self.turn_model = input.model.clone();
        self.policy.set_turn_rules(Some(input.rules.clone()));
        let access = self.ctx.access;
        if input.read_only_shell {
            self.ctx.access = FsAccess::ReadOnly;
        }
        let reason = self.turn(input, events, cancel).await;
        self.ctx.access = access;
        self.policy.set_turn_rules(None);
        self.turn_model = None;
        reason
    }

    async fn turn(
        &mut self,
        input: TurnInput,
        events: &UnboundedSender<AgentEvent>,
        cancel: CancellationToken,
    ) -> TurnEndReason {
        let _ = events.send(AgentEvent::TurnStarted);
        self.message_recorded = false;
        let content = self.user_message(input.parts, events).await;
        self.turn_entry =
            self.record_entry(Message::User { content }, input.display, false, input.plan);
        self.message_recorded = true;
        for kind in std::mem::take(&mut self.held_entries) {
            self.append_turn_entry(kind);
        }
        if cancel.is_cancelled() {
            return self.finish(TurnEndReason::Interrupted, events);
        }

        let mut auto_compaction_failed = false;
        let mut said_paused = false;
        for _ in 0..self.config.max_steps {
            // Before each request, not each turn: a turn with many tool calls can overrun.
            if self.budget_reached(events, &mut said_paused) {
                return self.finish(TurnEndReason::Budget, events);
            }
            if !auto_compaction_failed {
                auto_compaction_failed = !self.compact_automatically(events, &cancel).await;
            }
            let outcome = self.call_model_compacting(events, &cancel).await;
            match &outcome {
                ModelOutcome::Reply(reply)
                | ModelOutcome::Failed(_, reply)
                | ModelOutcome::Interrupted(reply) => self.tally(reply, events),
            }
            let reply = match outcome {
                ModelOutcome::Reply(mut reply) => {
                    self.dedupe_call_ids(&mut reply.tool_calls);
                    // The reported input covers the request; the reply is estimated like any
                    // later message, since output tokens (reasoning included) are not sent back.
                    if let Some(usage) = reply.usage {
                        self.reported_usage = Some((usage.input_tokens, self.history.len()));
                    }
                    reply
                }
                ModelOutcome::Failed(error, partial) => return self.fail(error, partial, events),
                ModelOutcome::Interrupted(mut partial) => {
                    if !partial.text.is_empty() {
                        partial.show(events);
                        self.push_assistant(partial.text, Vec::new(), events);
                    }
                    return self.finish(TurnEndReason::Interrupted, events);
                }
            };
            let mut reply = reply;
            let cut_off = reply.finish == Some(FinishReason::Length);
            // A cut-off text call lacks its end, so it is not looked for.
            if reply.tool_calls.is_empty()
                && !cut_off
                && self.config.text_tool_calls
                && let Some(mut calls) = crate::textcalls::recover(&reply.text, &self.tools)
            {
                self.dedupe_call_ids(&mut calls);
                reply.text.clear();
                reply.tool_calls = calls;
            }
            // Text held back in case it was calls, and that was not.
            reply.show(events);
            let calls = reply.tool_calls.clone();
            // A cut-off reply with nothing in it (all reasoning, say) leaves no empty message.
            if !(cut_off && reply.text.trim().is_empty() && calls.is_empty()) {
                self.push_assistant(reply.text, calls.clone(), events);
            }
            if cut_off {
                self.after_cut_off(&calls, events);
                continue;
            }
            if calls.is_empty() {
                match self.end_of_turn(events, &cancel).await {
                    gates::EndOfTurn::Finish(reason) => return self.finish(reason, events),
                    gates::EndOfTurn::Continue => continue,
                }
            }
            for (index, call) in calls.iter().enumerate() {
                if cancel.is_cancelled() {
                    // Every tool call needs a result, or the next request would be rejected.
                    for skipped in &calls[index..] {
                        let message = Message::Tool {
                            call_id: skipped.id.clone(),
                            content: STOPPED_BEFORE_RUNNING.into(),
                            is_error: true,
                        };
                        self.record(message, None, false);
                    }
                    return self.finish(TurnEndReason::Interrupted, events);
                }
                self.stats.tool_calls += 1;
                let output = self.execute(call, events).await;
                let message = Message::Tool {
                    call_id: call.id.clone(),
                    content: output.content,
                    is_error: output.is_error,
                };
                self.record(message, None, false);
            }
            if cancel.is_cancelled() {
                return self.finish(TurnEndReason::Interrupted, events);
            }
            self.deliver_steering(events);
        }
        self.finish(TurnEndReason::StepLimit, events)
    }

    /// After a reply the output limit cut off: its tool calls get results saying they were not
    /// run, or, without calls, a note asks the model to go on. The turn goes on either way.
    fn after_cut_off(&mut self, calls: &[ToolCall], events: &UnboundedSender<AgentEvent>) {
        let message = if calls.is_empty() {
            self.record(
                Message::User {
                    content: CUT_OFF_REPLY.into(),
                },
                None,
                true,
            );
            "the model's reply was cut off at its output limit; asking it to continue"
        } else {
            for call in calls {
                let result = Message::Tool {
                    call_id: call.id.clone(),
                    content: CUT_OFF_CALL.into(),
                    is_error: true,
                };
                self.record(result, None, false);
            }
            "the model's reply was cut off at its output limit, so its tool calls were not run"
        };
        let _ = events.send(AgentEvent::Warning {
            message: message.into(),
        });
    }

    /// Adds what the user sent during the turn to the conversation, after the tool results.
    /// Whether there was anything to deliver.
    fn deliver_steering(&mut self, events: &UnboundedSender<AgentEvent>) -> bool {
        let Some(steering) = &self.steering else {
            return false;
        };
        let taken = steering.take();
        let any = !taken.is_empty();
        for text in taken {
            self.record(
                Message::User {
                    content: text.clone(),
                },
                None,
                false,
            );
            let _ = events.send(AgentEvent::Steered { text });
        }
        any
    }

    /// The turn's user message: text parts as they are, and each shell part replaced by the output
    /// of running it as a `bash` tool call, with the same permission check, approval and sandbox.
    /// Once the turn is interrupted, later shell parts are neither run nor asked about.
    async fn user_message(
        &mut self,
        parts: Vec<InputPart>,
        events: &UnboundedSender<AgentEvent>,
    ) -> String {
        let mut message = String::new();
        for part in parts {
            match part {
                InputPart::Text(text) => message.push_str(&text),
                InputPart::Shell(command) if self.ctx.cancel.is_cancelled() => {
                    message.push_str(&format!("[`{command}` not run: interrupted]"));
                }
                InputPart::Shell(command) => {
                    let mut call = [ToolCall {
                        id: String::new(),
                        name: "bash".into(),
                        arguments: json!({ "command": command }).to_string(),
                    }];
                    self.dedupe_call_ids(&mut call);
                    let output = self.execute(&call[0], events).await;
                    message.push_str(&shell_part_text(&command, &output));
                }
            }
        }
        message
    }

    /// Compacts the conversation now (`/compact`): the older part is replaced by a summary the
    /// model writes, with `focus` saying what to keep in particular.
    pub async fn compact(
        &mut self,
        focus: Option<&str>,
        events: &UnboundedSender<AgentEvent>,
        cancel: CancellationToken,
    ) -> Result<(), String> {
        let result = self
            .compact_history(focus, Trigger::Manual, events, &cancel)
            .await;
        for message in self.warnings.drain(..) {
            let _ = events.send(AgentEvent::Warning { message });
        }
        result.map_err(|e| e.to_string())
    }

    /// Compacts the conversation before a model call when the request would reach the threshold,
    /// unless an earlier automatic compaction could not bring it below. Returns `false` when the
    /// compaction failed (after a warning), so the turn stops trying.
    async fn compact_automatically(
        &mut self,
        events: &UnboundedSender<AgentEvent>,
        cancel: &CancellationToken,
    ) -> bool {
        if !self.near_the_window() {
            self.auto_compaction_paused = false;
            return true;
        }
        if self.auto_compaction_paused {
            return true;
        }
        match self
            .compact_history(None, Trigger::Auto, events, cancel)
            .await
        {
            Ok(()) => {
                self.auto_compaction_paused = self.near_the_window();
                true
            }
            // One large message, say: nothing is wrong, and the next step may have more to
            // compact.
            Err(CompactError::NothingToCompact) => true,
            Err(CompactError::Interrupted) => false,
            Err(e) => {
                let _ = events.send(AgentEvent::Warning {
                    message: format!("could not compact the conversation: {e}"),
                });
                false
            }
        }
    }

    /// Estimated tokens of the next request: what the provider last reported plus an estimate
    /// for the messages added since, or else an estimate of the whole request.
    fn estimated_tokens(&self) -> u64 {
        if let Some((reported, covered)) = self.reported_usage
            && covered <= self.history.len()
        {
            let added: u64 = self.history[covered..]
                .iter()
                .map(compaction::message_tokens)
                .sum();
            return reported + added;
        }
        compaction::request_tokens(
            &self.config.system_prompt,
            &self.tools.specs(),
            &self.history,
        )
    }

    /// Whether the next request would reach the compaction threshold.
    fn near_the_window(&self) -> bool {
        self.estimated_tokens() as f64 >= self.threshold_tokens()
    }

    /// The compaction threshold, in tokens.
    fn threshold_tokens(&self) -> f64 {
        self.config.context_window as f64 * self.config.compaction.threshold
    }

    /// Where the current turn starts: its user message (or the summary standing for it).
    fn turn_start(&self) -> usize {
        self.history
            .iter()
            .rposition(|m| matches!(m, Message::User { .. }))
            .unwrap_or(0)
    }

    /// Where the last step starts: the last message that is not a tool result, so a model reply
    /// and the results of its tool calls stay together.
    fn last_step_start(&self) -> usize {
        self.history
            .iter()
            .rposition(|m| !matches!(m, Message::Tool { .. }))
            .unwrap_or(0)
    }

    /// Where the kept part starts when no recent part fits the keep budget: the current turn,
    /// unless the turn itself reaches the threshold (or is only an earlier summary's), and then
    /// its last step, so that what is kept fits.
    fn fallback_cut(&self) -> usize {
        let turn_start = self.turn_start();
        let turn = compaction::request_tokens(
            &self.config.system_prompt,
            &self.tools.specs(),
            &self.history[turn_start..],
        );
        if (turn as f64) < self.threshold_tokens()
            && compaction::summarizes(&self.history[..turn_start])
        {
            turn_start
        } else {
            self.last_step_start()
        }
    }

    /// Replaces the older part of the conversation by a summary. The kept part fits the
    /// configured share of the window. Failing that, automatic compaction keeps the current
    /// turn, or only its last step when the turn itself reaches the threshold; overflow
    /// compaction keeps only the last step; and `/compact` summarizes everything. A part to
    /// summarize that is only an earlier summary is nothing to compact. The summary is saved as a
    /// compaction entry, so the summarized messages stay in the session and can be rewound to.
    async fn compact_history(
        &mut self,
        focus: Option<&str>,
        trigger: Trigger,
        events: &UnboundedSender<AgentEvent>,
        cancel: &CancellationToken,
    ) -> Result<(), CompactError> {
        let window = self.config.context_window;
        let budget = (window as f64 * self.config.compaction.keep_recent) as u64;
        let fits = compaction::cut(&self.history, budget);
        let cut = match trigger {
            Trigger::Auto => fits.unwrap_or_else(|| self.fallback_cut()),
            // The provider's window is smaller than assumed: keep no more than the current turn,
            // and only the last step when even that is over the budget.
            Trigger::Overflow => match fits {
                Some(fits) => fits.max(self.turn_start()),
                None => self.last_step_start(),
            },
            Trigger::Manual => fits.unwrap_or(self.history.len()),
        };
        if !compaction::summarizes(&self.history[..cut]) {
            return Err(CompactError::NothingToCompact);
        }
        let before = self.estimated_tokens();
        let model = self
            .turn_model
            .as_ref()
            .map_or(self.config.model_name.clone(), |m| m.name.clone());
        // The transcript is sized from an estimate, so the request can still be too long for the
        // model: then it is tried once more with half as much.
        let mut max_tokens = window / 2;
        let summary = loop {
            let mut request =
                compaction::summary_request(&model, &self.history[..cut], focus, max_tokens);
            // The session's model writes it under its profile's options, as it answers turns; a
            // slash command's model gets the provider's defaults, but for whether it is local, as
            // for its turn.
            match &self.turn_model {
                None => {
                    request.options = self.config.request.clone();
                    let input = compaction::request_tokens(
                        &request.system,
                        &request.tools,
                        &request.messages,
                    );
                    request.output_room = Some(window.saturating_sub(input));
                }
                Some(turn) => request.options = turn.options(),
            }
            match self.summarize(request, events, cancel).await {
                Err(CompactError::Overflow(_)) if max_tokens == window / 2 => max_tokens /= 2,
                result => break result?,
            }
        };
        let first_kept = self.history_ids.get(cut).cloned();
        self.session.append(EntryKind::Compaction {
            summary: summary.clone(),
            first_kept,
        });
        self.after_session_change();
        let _ = events.send(AgentEvent::Compacted {
            summary,
            tokens_before: before,
            tokens_after: self.estimated_tokens(),
        });
        Ok(())
    }

    /// Asks the model answering this turn for the summary `request` asks for, retrying transient
    /// errors like any model call.
    async fn summarize(
        &self,
        request: ChatRequest,
        events: &UnboundedSender<AgentEvent>,
        cancel: &CancellationToken,
    ) -> Result<String, CompactError> {
        let provider = self
            .turn_model
            .as_ref()
            .map_or(&self.provider, |m| &m.provider)
            .clone();
        let mut attempt = 1;
        let started = Instant::now();
        loop {
            let attempt_started = Instant::now();
            let mut text = String::new();
            let mut finish = None;
            let mut usage: Option<Usage> = None;
            let result = tokio::select! {
                result = async {
                    let mut stream = provider.stream(request.clone());
                    while let Some(item) = stream.next().await {
                        match item? {
                            ProviderEvent::TextDelta(delta) => text.push_str(&delta),
                            ProviderEvent::Finished(reason) => finish = Some(reason),
                            // Last one wins, as for any other reply: some servers report it
                            // cumulatively, in every chunk.
                            ProviderEvent::Usage(reported) => usage = Some(reported),
                            _ => {}
                        }
                    }
                    Ok::<(), ProviderError>(())
                } => result,
                _ = cancel.cancelled() => {
                    // What was reported before the stop is billed.
                    self.meter_request(
                        "error:interrupted".into(),
                        usage.unwrap_or_default(),
                        started,
                        events,
                    );
                    return Err(CompactError::Interrupted);
                }
            };
            // A compaction request is a paid call like any other, and often the turn's largest,
            // so it counts towards `/usage`, the budgets and the status line's totals, whatever
            // becomes of its result.
            match result {
                Ok(()) if text.trim().is_empty() => {
                    self.meter_summary("error:incomplete", usage, started, events);
                    return Err(CompactError::Failed(
                        "the model returned an empty summary".into(),
                    ));
                }
                // The end of a summary says what remains to be done: a cut-off one is no use.
                Ok(()) if finish == Some(FinishReason::Length) => {
                    self.meter_summary("error:incomplete", usage, started, events);
                    return Err(CompactError::Failed(
                        "the summary was cut off at the model's output limit".into(),
                    ));
                }
                Ok(()) => {
                    self.meter_summary("ok", usage, started, events);
                    return Ok(text.trim().to_string());
                }
                Err(error) if self.config.retry.retries(&error, attempt) => {
                    // What the attempt reported before it failed is billed, as a record of its own.
                    if usage.is_some() {
                        let outcome = format!("error:{}", error.kind());
                        self.meter_request(
                            outcome,
                            usage.unwrap_or_default(),
                            attempt_started,
                            events,
                        );
                    }
                    let delay = self.config.retry.delay(attempt, error.retry_after());
                    let _ = events.send(AgentEvent::Retrying {
                        attempt,
                        reason: error.to_string(),
                        delay_ms: delay.as_millis() as u64,
                    });
                    tokio::select! {
                        _ = tokio::time::sleep(delay) => {}
                        _ = cancel.cancelled() => {
                            self.meter_request(
                                "error:interrupted".into(),
                                Usage::default(),
                                started,
                                events,
                            );
                            return Err(CompactError::Interrupted);
                        }
                    }
                    attempt += 1;
                }
                Err(error) => {
                    let outcome = format!("error:{}", error.kind());
                    self.meter_request(outcome, usage.unwrap_or_default(), started, events);
                    return Err(if error.is_context_overflow() {
                        CompactError::Overflow(describe(&error))
                    } else {
                        CompactError::Failed(describe(&error))
                    });
                }
            }
        }
    }

    /// Meters a summary request that got an answer (`ok`, or `error:incomplete` for one that is
    /// no use) and reports its usage.
    fn meter_summary(
        &self,
        outcome: &str,
        usage: Option<Usage>,
        started: Instant,
        events: &UnboundedSender<AgentEvent>,
    ) {
        self.meter_request(outcome.into(), usage.unwrap_or_default(), started, events);
        if let Some(usage) = usage {
            let _ = events.send(AgentEvent::Usage {
                model: self.model_id().to_string(),
                usage,
            });
        }
    }

    /// [`call_model`](Self::call_model); when the provider rejects the request as longer than its
    /// context window, the conversation is compacted and the request sent once more.
    async fn call_model_compacting(
        &mut self,
        events: &UnboundedSender<AgentEvent>,
        cancel: &CancellationToken,
    ) -> ModelOutcome {
        let outcome = self.call_model(events, cancel).await;
        match &outcome {
            ModelOutcome::Failed(error, partial)
                if error.is_context_overflow() && !partial.emitted => {}
            _ => return outcome,
        }
        match self
            .compact_history(None, Trigger::Overflow, events, cancel)
            .await
        {
            Ok(()) => self.call_model(events, cancel).await,
            // Ctrl-C while compacting ends the turn as an interruption, not as the overflow.
            Err(_) if cancel.is_cancelled() => ModelOutcome::Interrupted(ModelReply::default()),
            Err(e) => {
                let _ = events.send(AgentEvent::Warning {
                    message: format!("could not compact the conversation: {e}"),
                });
                outcome
            }
        }
    }

    /// The provider the session's model runs on, to ask it where a subscription's windows stand.
    pub fn provider(&self) -> Arc<dyn Provider> {
        self.provider.clone()
    }

    /// The id of the model answering the current turn: between turns, the session's.
    pub fn model_id(&self) -> &str {
        self.turn_model
            .as_ref()
            .map_or(&self.config.model_id, |model| &model.id)
    }

    /// One model call with retries for transient errors, reported to the meter. Never retries
    /// once output reached the user.
    async fn call_model(
        &self,
        events: &UnboundedSender<AgentEvent>,
        cancel: &CancellationToken,
    ) -> ModelOutcome {
        let started = Instant::now();
        let outcome = self.call_model_with_retries(events, cancel).await;
        let (result, usage) = match &outcome {
            ModelOutcome::Reply(reply) => ("ok".to_string(), reply.usage),
            // What the provider reported before the request failed or was stopped is billed too.
            ModelOutcome::Failed(error, partial) => {
                (format!("error:{}", error.kind()), partial.usage)
            }
            ModelOutcome::Interrupted(partial) => ("error:interrupted".to_string(), partial.usage),
        };
        self.meter_request(result, usage.unwrap_or_default(), started, events);
        outcome
    }

    async fn call_model_with_retries(
        &self,
        events: &UnboundedSender<AgentEvent>,
        cancel: &CancellationToken,
    ) -> ModelOutcome {
        let mut attempt = 1;
        loop {
            let attempt_started = Instant::now();
            let mut reply = ModelReply::default();
            let result = tokio::select! {
                result = self.stream_into(&mut reply, events) => Some(result),
                _ = cancel.cancelled() => None,
            };
            match result {
                None => return ModelOutcome::Interrupted(reply),
                Some(Ok(())) => return ModelOutcome::Reply(reply),
                Some(Err(error))
                    if self.config.retry.retries(&error, attempt)
                        && !reply.emitted
                        && !error
                            .retry_after()
                            .is_some_and(|d| d > crate::retry::MAX_AUTOMATIC_RETRY_AFTER) =>
                {
                    // What the attempt reported before it failed is billed, as a record of its own.
                    if let Some(usage) = reply.usage {
                        let outcome = format!("error:{}", error.kind());
                        self.meter_request(outcome, usage, attempt_started, events);
                    }
                    let delay = self.config.retry.delay(attempt, error.retry_after());
                    self.retry_count
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let _ = events.send(AgentEvent::Retrying {
                        attempt,
                        reason: error.to_string(),
                        delay_ms: delay.as_millis() as u64,
                    });
                    let waited = tokio::select! {
                        _ = tokio::time::sleep(delay) => true,
                        _ = cancel.cancelled() => false,
                    };
                    if !waited {
                        return ModelOutcome::Interrupted(ModelReply::default());
                    }
                    attempt += 1;
                }
                Some(Err(error)) => return ModelOutcome::Failed(error, reply),
            }
        }
    }

    fn push_assistant(
        &mut self,
        text: String,
        tool_calls: Vec<ToolCall>,
        events: &UnboundedSender<AgentEvent>,
    ) {
        let model = self.model_id().to_string();
        let _ = events.send(AgentEvent::AssistantMessage {
            content: text.clone(),
            model: model.clone(),
        });
        let message = Message::Assistant {
            content: text,
            tool_calls,
            model,
        };
        self.record(message, None, false);
    }

    /// Adds what a model call took to the turn's stats, and reports its usage once: a server
    /// that reports usage cumulatively, in every chunk (redact.rs knows such servers exist), must
    /// not be counted once per chunk, only once per reply, with the last chunk's number.
    fn tally(&mut self, reply: &ModelReply, events: &UnboundedSender<AgentEvent>) {
        self.stats.model = Some(self.model_id().to_string());
        if let (Some(started), Some(first)) = (reply.started, reply.first_output) {
            if self.stats.time_to_first_token.is_none() {
                self.stats.time_to_first_token = Some(first - started);
            }
            let ended = reply.ended.unwrap_or_else(Instant::now);
            self.stats.generation += ended.saturating_duration_since(first);
        }
        if let Some(usage) = reply.usage {
            self.stats.usage.input_tokens += usage.input_tokens;
            self.stats.usage.output_tokens += usage.output_tokens;
            self.stats.usage.cached_tokens += usage.cached_tokens;
            self.stats.usage.cache_write_tokens += usage.cache_write_tokens;
            self.stats.usage.cache_write_1h_tokens += usage.cache_write_1h_tokens;
            self.stats.usage.reasoning_tokens += usage.reasoning_tokens;
            let _ = events.send(AgentEvent::Usage {
                model: self.model_id().to_string(),
                usage,
            });
        }
    }

    fn finish(
        &mut self,
        reason: TurnEndReason,
        events: &UnboundedSender<AgentEvent>,
    ) -> TurnEndReason {
        for message in self.warnings.drain(..) {
            let _ = events.send(AgentEvent::Warning { message });
        }
        let stats = std::mem::take(&mut self.stats);
        self.meter_turn(reason, &stats);
        if let Some(model) = stats.model {
            let _ = events.send(AgentEvent::TurnStats {
                model,
                time_to_first_token_ms: stats.time_to_first_token.map(|d| d.as_millis() as u64),
                generation_ms: stats.generation.as_millis() as u64,
                input_tokens: stats.usage.input_tokens,
                output_tokens: stats.usage.output_tokens,
                cached_tokens: stats.usage.cached_tokens,
            });
        }
        let _ = events.send(AgentEvent::TurnFinished { reason });
        reason
    }

    fn fail(
        &mut self,
        error: ProviderError,
        mut partial: ModelReply,
        events: &UnboundedSender<AgentEvent>,
    ) -> TurnEndReason {
        if !partial.text.is_empty() {
            partial.show(events);
            self.push_assistant(partial.text, Vec::new(), events);
        }
        if error.is_quota_exhausted()
            && let Some(resets_at) = error.resets_at()
        {
            let _ = events.send(AgentEvent::LimitReached { resets_at });
        }
        let _ = events.send(AgentEvent::Error {
            kind: ErrorKind::Provider,
            message: describe(&error),
        });
        self.finish(TurnEndReason::Error, events)
    }

    /// Streams one model call into `reply`, forwarding deltas as events.
    async fn stream_into(
        &self,
        reply: &mut ModelReply,
        events: &UnboundedSender<AgentEvent>,
    ) -> Result<(), ProviderError> {
        // A slash command's model gets the provider's defaults, but for whether it is local: the
        // options, and the window, are the session model's.
        let (provider, model, options, output_room) = match &self.turn_model {
            Some(turn) => (&turn.provider, turn.name.clone(), turn.options(), None),
            None => (
                &self.provider,
                self.config.model_name.clone(),
                self.config.request.clone(),
                Some(
                    self.config
                        .context_window
                        .saturating_sub(self.estimated_tokens()),
                ),
            ),
        };
        let request = ChatRequest {
            model,
            system: self.config.system_prompt.clone(),
            messages: request_messages(&self.history),
            tools: self.tools.specs(),
            options,
            output_room,
        };
        reply.started = Some(Instant::now());
        let mut stream = provider.stream(request);
        while let Some(item) = stream.next().await {
            let item = item?;
            // `OutputStarted` is a tool-call reply's only early signal: the call itself is
            // buffered by the wire parser and arrives whole only once the stream ends. The other
            // three are kept as a fallback for a provider that has none to emit.
            if matches!(
                item,
                ProviderEvent::OutputStarted
                    | ProviderEvent::TextDelta(_)
                    | ProviderEvent::ReasoningDelta(_)
                    | ProviderEvent::ToolCall(_)
            ) && reply.first_output.is_none()
            {
                reply.first_output = Some(Instant::now());
            }
            match item {
                ProviderEvent::TextDelta(text) => {
                    reply.text.push_str(&text);
                    // Tool calls written as text are not shown as text: what may still become
                    // them waits for the reply's end, or until it cannot.
                    let held = self.config.text_tool_calls
                        && reply.shown == 0
                        && reply.watch.may_be_calls(&reply.text);
                    if !held {
                        reply.show(events);
                    }
                }
                ProviderEvent::ReasoningDelta(text) => {
                    reply.emitted = true;
                    let _ = events.send(AgentEvent::ReasoningDelta { text });
                }
                ProviderEvent::ToolCall(call) => reply.tool_calls.push(call),
                ProviderEvent::OutputStarted => {}
                // Kept, last one wins, for `tally` to report once the reply is whole: some
                // servers send it cumulatively, in every chunk, and counting each would
                // over-count both `/usage` and the status line's session totals.
                ProviderEvent::Usage(usage) => reply.usage = Some(usage),
                ProviderEvent::RateLimits(snapshot) => {
                    if let Some(meter) = &self.meter {
                        meter.record_window(&snapshot);
                    }
                    let _ = events.send(AgentEvent::RateLimits { snapshot });
                }
                ProviderEvent::Finished(reason) => reply.finish = Some(reason),
            }
        }
        reply.ended = Some(Instant::now());
        Ok(())
    }

    async fn execute(
        &mut self,
        call: &ToolCall,
        events: &UnboundedSender<AgentEvent>,
    ) -> ToolOutput {
        let _ = events.send(AgentEvent::ToolCallRequested {
            id: call.id.clone(),
            name: call.name.clone(),
            arguments: call.arguments.clone(),
        });
        let raw = self.execute_inner(call, events).await;
        let mut content = limit_output(
            &raw.content,
            self.config.output_limit,
            &self.config.output_dir,
            &call.id,
            self.redactor.as_deref(),
        );
        // What the checks after an edit found goes in the same result, after the limit, which
        // would cut into it.
        if !raw.is_error
            && let Some(checks) = self.after_edit(call, events).await
        {
            content.push('\n');
            content.push_str(&checks);
        }
        let output = ToolOutput { content, ..raw };
        let _ = events.send(AgentEvent::ToolCallFinished {
            id: call.id.clone(),
            output: output.content.clone(),
            is_error: output.is_error,
        });
        output
    }

    async fn execute_inner(
        &mut self,
        call: &ToolCall,
        events: &UnboundedSender<AgentEvent>,
    ) -> ToolOutput {
        let Some(tool) = self.tools.get(&call.name) else {
            self.invalid_calls += 1;
            let names: Vec<String> = self.tools.specs().into_iter().map(|s| s.name).collect();
            return ToolOutput::error(format!(
                "unknown tool `{}`; available tools: {}",
                call.name,
                names.join(", ")
            ));
        };
        let raw = if call.arguments.trim().is_empty() {
            "{}"
        } else {
            call.arguments.as_str()
        };
        let args: Value = match serde_json::from_str(raw) {
            Ok(args) => args,
            Err(e) => {
                self.invalid_calls += 1;
                return ToolOutput::error(format!(
                    "arguments for `{}` are not valid JSON: {e}",
                    call.name
                ));
            }
        };
        let problems: Vec<String> = self
            .validators
            .get(&call.name)
            .map(|v| v.iter_errors(&args).map(|e| e.to_string()).collect())
            .unwrap_or_default();
        if !problems.is_empty() {
            self.invalid_calls += 1;
            return ToolOutput::error(format!(
                "invalid arguments for `{}`: {}",
                call.name,
                problems.join("; ")
            ));
        }

        let action = tool.action(&args, &self.ctx);
        let mutating = self.is_mutating(&action);
        let decision = self.policy.check(&action);
        // A sandbox that can no longer run this command (Linux, git protection required, after a
        // drop to the basic tier) leaves one way to run it: outside the sandbox, if approved.
        if let (Decision::Allow | Decision::Ask(_), Action::Bash(command)) = (&decision, &action)
            && !self.ctx.unsandboxed
            && let Some(why) = self
                .ctx
                .sandbox
                .as_ref()
                .and_then(|sandbox| sandbox.cannot_run(self.ctx.access))
        {
            let command = command.clone();
            let asked_anyway = match &decision {
                Decision::Ask(reason) => Some(reason.clone()),
                _ => None,
            };
            return self
                .run_outside_sandbox(
                    call,
                    &tool,
                    args,
                    &command,
                    &why,
                    asked_anyway.as_deref(),
                    mutating,
                    events,
                )
                .await;
        }
        match decision {
            Decision::Allow => {}
            Decision::Deny(reason) => return ToolOutput::refused(format!("denied: {reason}")),
            Decision::Ask(reason) => {
                let request = ApprovalRequest {
                    call_id: call.id.clone(),
                    tool: call.name.clone(),
                    arguments: args.clone(),
                    kept_for_session: self.policy.can_remember(&action),
                    action,
                    reason: reason.clone(),
                    kind: ApprovalKind::Action,
                };
                let Some(decision) = self.ask(&request, events).await else {
                    return ToolOutput::error(STOPPED_BEFORE_RUNNING);
                };
                match decision {
                    ApprovalDecision::Approve => {}
                    ApprovalDecision::ApproveForSession => {
                        if !self.policy.remember(&request.action) {
                            let _ = events.send(AgentEvent::Warning {
                                message: format!(
                                    "approved once: {reason} cannot be approved for the rest of the session, so harness will ask again next time"
                                ),
                            });
                        }
                    }
                    ApprovalDecision::Deny {
                        feedback: Some(note),
                    } => {
                        return ToolOutput::refused(format!("the user denied this action: {note}"));
                    }
                    ApprovalDecision::Deny { feedback: None } => {
                        return ToolOutput::refused("the user denied this action");
                    }
                    ApprovalDecision::Unavailable => {
                        let _ = events.send(AgentEvent::ActionBlocked {
                            id: call.id.clone(),
                            reason: reason.clone(),
                        });
                        return ToolOutput::refused(format!(
                            "blocked: {reason} needs approval and no user is available to approve it"
                        ));
                    }
                }
            }
        }
        if mutating {
            self.checkpoint(events).await;
        }
        let output = tool.run(args.clone(), &self.ctx).await;
        if output.guard_blocked {
            let reason = "the sandbox's git-metadata guard undid changes this command made";
            let _ = events.send(AgentEvent::ActionBlocked {
                id: call.id.clone(),
                reason: reason.to_string(),
            });
            return output;
        }
        if output.sandbox_denied && self.ctx.access == FsAccess::ReadOnly {
            return ToolOutput {
                content: format!(
                    "{}\n[this mode runs commands only in a read-only sandbox, so it cannot be run without it]",
                    output.content
                ),
                ..output
            };
        }
        if output.sandbox_denied {
            return self
                .offer_unsandboxed_rerun(call, &tool, args, output, events)
                .await;
        }
        output
    }

    /// Asks the user about `request`, unless the turn was stopped: then nothing is asked, and a
    /// turn stopped while the user has not answered stops waiting for the answer. `None` when
    /// stopped, and the action must not run.
    async fn ask(
        &self,
        request: &ApprovalRequest,
        events: &UnboundedSender<AgentEvent>,
    ) -> Option<ApprovalDecision> {
        let cancel = self.ctx.cancel.clone();
        if cancel.is_cancelled() {
            return None;
        }
        let _ = events.send(AgentEvent::ApprovalNeeded {
            id: request.call_id.clone(),
            reason: request.reason.clone(),
        });
        tokio::select! {
            biased;
            _ = cancel.cancelled() => None,
            decision = self.approver.decide(request) => Some(decision),
        }
    }

    /// The sandbox cannot run `command` now, for the reason `why`: asks whether to run it outside
    /// the sandbox, once. Nobody to ask blocks it.
    #[allow(
        clippy::too_many_arguments,
        reason = "what executing the call knows, passed on"
    )]
    async fn run_outside_sandbox(
        &mut self,
        call: &ToolCall,
        tool: &Arc<dyn Tool>,
        args: Value,
        command: &str,
        why: &str,
        asked_anyway: Option<&str>,
        mutating: bool,
        events: &UnboundedSender<AgentEvent>,
    ) -> ToolOutput {
        // What the policy would have asked about anyway (a destructive command, a `confirm`
        // rule) stays in the question.
        let reason = match asked_anyway {
            Some(reason) => format!("{reason}, and {why}: run it without the sandbox?"),
            None => {
                let mut shown: String = command.chars().take(80).collect();
                if shown.len() < command.len() {
                    shown.push('…');
                }
                format!("{why}; run `{shown}` without the sandbox?")
            }
        };
        let request = ApprovalRequest {
            call_id: call.id.clone(),
            tool: call.name.clone(),
            arguments: args.clone(),
            action: Action::Bash(command.to_string()),
            reason,
            kind: ApprovalKind::RunUnsandboxed,
            kept_for_session: false,
        };
        let Some(decision) = self.ask(&request, events).await else {
            return ToolOutput::error(STOPPED_BEFORE_RUNNING);
        };
        match decision {
            ApprovalDecision::Approve | ApprovalDecision::ApproveForSession => {
                if mutating {
                    self.checkpoint(events).await;
                }
                let mut ctx = self.ctx.clone();
                ctx.unsandboxed = true;
                tool.run(args, &ctx).await
            }
            ApprovalDecision::Deny {
                feedback: Some(note),
            } => ToolOutput::refused(format!(
                "the user declined to run it without the sandbox: {note}"
            )),
            ApprovalDecision::Deny { feedback: None } => {
                ToolOutput::refused("the user declined to run it without the sandbox")
            }
            ApprovalDecision::Unavailable => {
                let blocked = format!(
                    "{why}, and no user is available to approve running the command without the sandbox"
                );
                let _ = events.send(AgentEvent::ActionBlocked {
                    id: call.id.clone(),
                    reason: blocked.clone(),
                });
                ToolOutput::refused(format!("blocked: {blocked}"))
            }
        }
    }

    /// A command failed inside the sandbox in a way that looks like a denial: ask whether to run
    /// it once without the sandbox. Denials are recognised heuristically, so the wording hedges.
    async fn offer_unsandboxed_rerun(
        &mut self,
        call: &ToolCall,
        tool: &Arc<dyn Tool>,
        args: Value,
        first: ToolOutput,
        events: &UnboundedSender<AgentEvent>,
    ) -> ToolOutput {
        let reason = "the sandbox may have blocked this command; run it again without the sandbox?"
            .to_string();
        let request = ApprovalRequest {
            call_id: call.id.clone(),
            tool: call.name.clone(),
            arguments: args.clone(),
            action: tool.action(&args, &self.ctx),
            reason,
            kind: ApprovalKind::RunUnsandboxed,
            kept_for_session: false,
        };
        let Some(decision) = self.ask(&request, events).await else {
            return ToolOutput {
                content: format!(
                    "{}\n[not run again without the sandbox: the user stopped the turn]",
                    first.content
                ),
                ..first
            };
        };
        let note = match decision {
            ApprovalDecision::Approve | ApprovalDecision::ApproveForSession => {
                let mut ctx = self.ctx.clone();
                ctx.unsandboxed = true;
                return tool.run(args, &ctx).await;
            }
            ApprovalDecision::Deny {
                feedback: Some(note),
            } => {
                format!("the user declined to run it without the sandbox: {note}")
            }
            ApprovalDecision::Deny { feedback: None } => {
                "the user declined to run it without the sandbox".to_string()
            }
            ApprovalDecision::Unavailable => {
                let reason = "the sandbox may have blocked this command and no user is available to approve running it without the sandbox";
                let _ = events.send(AgentEvent::ActionBlocked {
                    id: call.id.clone(),
                    reason: reason.into(),
                });
                reason.to_string()
            }
        };
        ToolOutput {
            content: format!("{}\n[{note}]", first.content),
            ..first
        }
    }
}

/// What a shell part becomes in the user message: the command's output, or, when it did not run
/// or failed, a note saying so followed by what the tool reported.
fn shell_part_text(command: &str, output: &ToolOutput) -> String {
    match output.content.strip_prefix("exit code 0\n") {
        Some(text) if !output.is_error => text.trim_end_matches('\n').to_string(),
        _ => format!(
            "[`{command}` did not run successfully]\n{}",
            output.content.trim_end_matches('\n')
        ),
    }
}

/// The result of a tool call the user stopped the turn before, while harness waited to ask or
/// for the answer.
pub(crate) const STOPPED_BEFORE_RUNNING: &str = "interrupted by the user before this tool ran";

/// The result given to a tool call that a stopped run left without one.
fn stopped_result(call_id: String) -> Message {
    Message::Tool {
        call_id,
        content: "harness stopped before this tool call finished; its effects are unknown".into(),
        is_error: true,
    }
}

/// The note appended to the conversation when the approval mode changes to `mode`, saying what it
/// allows with or without an OS sandbox for shell commands (`sandboxed`), as the base system
/// prompt does.
fn mode_note(mode: Mode, sandboxed: bool) -> String {
    let rules = match mode {
        Mode::Plan if sandboxed => {
            "file edits are refused, and shell commands run in a read-only sandbox. Investigate the task, then end your reply with a step-by-step implementation plan; the user will build it, edit it, or keep planning"
        }
        Mode::Plan => {
            "file edits and shell commands are refused, since no OS sandbox is active; use the read, grep and glob tools. Investigate the task, then end your reply with a step-by-step implementation plan; the user will build it, edit it, or keep planning"
        }
        Mode::ReadOnly if sandboxed => {
            "file edits are refused, and shell commands run in a read-only sandbox"
        }
        Mode::ReadOnly => {
            "file edits and shell commands are refused, since no OS sandbox is active; use the read, grep and glob tools"
        }
        Mode::Ask if sandboxed => {
            "file edits and sandboxed shell commands need the user's approval unless a rule allows them"
        }
        Mode::Ask => {
            "file edits need the user's approval unless a rule allows them, and every shell command needs approval, since no OS sandbox is active"
        }
        Mode::Auto if sandboxed => {
            "file edits in the workspace and sandboxed shell commands run without approval"
        }
        Mode::Auto => {
            "every shell command needs approval, since no OS sandbox is active; use the file tools instead"
        }
        Mode::FullAccess if sandboxed => {
            "actions run without approval, except those a deny rule forbids or may match; shell commands still run in the sandbox"
        }
        Mode::FullAccess => {
            "actions run without approval or sandbox, except those a deny rule forbids or may match"
        }
    };
    format!("[harness] The approval mode is now {mode}: {rules}.")
}

/// `history` as sent to a provider. Consecutive user messages, such as a mode-change note and the
/// next prompt, become one: some chat templates reject two user messages in a row. The session
/// keeps them apart.
fn request_messages(history: &[Message]) -> Vec<Message> {
    let mut messages: Vec<Message> = Vec::with_capacity(history.len());
    for message in history {
        match (messages.last_mut(), message) {
            (Some(Message::User { content: joined }), Message::User { content }) => {
                joined.push_str("\n\n");
                joined.push_str(content);
            }
            _ => messages.push(message.clone()),
        }
    }
    messages
}

/// A human-readable error message for the user.
fn describe(error: &ProviderError) -> String {
    // An exhausted quota says when it resets, whatever `Retry-After` asks.
    let quoted = |body: &str| -> String { body.chars().take(500).collect() };
    if error.is_quota_exhausted() {
        let resets = error
            .resets_at()
            .map(|at| format!("; it resets at {}", crate::time::timestamp(at)))
            .unwrap_or_default();
        let said = match error {
            ProviderError::Http { status, body, .. } => format!("HTTP {status}: {}", quoted(body)),
            ProviderError::Reported { body, .. } => {
                format!("the provider reported: {}", quoted(body))
            }
            _ => String::new(),
        };
        return format!(
            "the provider's usage limit is reached{resets}. Switch models with --model (or /model in the terminal UI). {said}"
        );
    }
    if error.is_spend_cap() {
        let said = match error {
            ProviderError::Http { body, .. } | ProviderError::Reported { body, .. } => quoted(body),
            _ => String::new(),
        };
        return format!(
            "the account's spend limit is reached, and waiting does not end it: raise the limit with the provider, or switch models with --model (or /model in the terminal UI). {said}"
        );
    }
    if let Some(wait) = error.retry_after()
        && wait > crate::retry::MAX_AUTOMATIC_RETRY_AFTER
    {
        return format!(
            "the provider asked to wait {}s before retrying, which is longer than we wait automatically. {error}",
            wait.as_secs()
        );
    }
    match error {
        ProviderError::NoStart {
            message,
            local: true,
        } => format!(
            "{message}: the local server may still be loading the model, or reading a long prompt on a CPU. harness does not retry, since a retry would start that work over; check the server (its log, and whether the model fits in memory and runs on the GPU), or use a smaller context or model"
        ),
        ProviderError::Http { status: 429, .. } | ProviderError::Reported { status: 429, .. } => {
            format!(
                "{error}. The provider is rate limiting; try again later or switch models with --model."
            )
        }
        ProviderError::Http { status, body, .. } => format!("HTTP {status}: {}", quoted(body)),
        // What fixes it is kept, however long the body.
        ProviderError::KeyRefused { status, body, hint } => {
            format!("HTTP {status}: {}; {hint}", quoted(body))
        }
        ProviderError::Reported {
            status,
            body,
            retry_after,
        } => ProviderError::Reported {
            status: *status,
            body: quoted(body),
            retry_after: *retry_after,
        }
        .to_string(),
        other => other.to_string(),
    }
}
