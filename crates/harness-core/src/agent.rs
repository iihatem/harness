//! The agent loop: call the model, run the tools it requests, feed results back, repeat.

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
};

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::{
    checkpoint::{CheckpointError, Checkpoints},
    compaction::{self, CompactionConfig},
    event::{AgentEvent, ErrorKind, TurnEndReason},
    message::{ChatRequest, Message, ToolCall, Usage},
    output::{DEFAULT_OUTPUT_LIMIT, limit_output},
    permission::{Action, Decision, FsAccess, Mode, PermissionPolicy},
    provider::{FinishReason, Provider, ProviderError, ProviderEvent},
    retry::RetryPolicy,
    session::{Entry, EntryKind, RewindScope, Session},
    tokens::DEFAULT_CONTEXT_WINDOW,
    tool::{Tool, ToolContext, ToolOutput, ToolRegistry},
    turn::{InputPart, TurnInput, TurnModel},
};

/// Model calls allowed per turn unless configured otherwise.
pub const DEFAULT_MAX_STEPS: u32 = 50;

/// What the rewind list says about effects a rewind cannot undo.
pub const REWIND_LIMITS: &str = "Rewinding restores files in the workspace only: network calls, databases, pushed commits and files outside the workspace stay as they are.";

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
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRequest {
    pub call_id: String,
    pub tool: String,
    pub action: Action,
    pub reason: String,
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

/// What one model call produced so far. Kept outside the stream future so partial output survives.
#[derive(Debug, Default)]
struct ModelReply {
    text: String,
    tool_calls: Vec<ToolCall>,
    #[allow(dead_code)] // read in P4 (truncation)
    finish: Option<FinishReason>,
    /// Whether any output was already shown to the user (then the call must not be retried).
    emitted: bool,
    /// The token counts the provider reported for this call.
    usage: Option<Usage>,
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
    /// Tokens the provider reported for the last request and its reply, and how many messages
    /// they covered; `None` until a provider reports usage, and after the history changes.
    reported_usage: Option<(u64, usize)>,
    validators: HashMap<String, jsonschema::Validator>,
    invalid_calls: u32,
    /// Tool-call ids already used in this session, so a missing or repeated id (from a model or
    /// provider that doesn't guarantee unique ids) can be rewritten before it collides.
    used_call_ids: HashSet<String>,
    next_call_id: u64,
    /// The model answering the current turn, when it is not the session's.
    turn_model: Option<TurnModel>,
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
            reported_usage: None,
            validators,
            invalid_calls: 0,
            used_call_ids: HashSet::new(),
            next_call_id: 0,
            turn_model: None,
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
        self.load_history(true);
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

    /// Snapshots the workspace before each turn's first change, so it can be rewound.
    pub fn with_checkpoints(mut self, checkpoints: Option<Arc<Checkpoints>>) -> Self {
        self.checkpoints = checkpoints;
        self
    }

    pub fn session(&self) -> &Session {
        &self.session
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
                } => Some(RewindPoint {
                    entry: entry.id.clone(),
                    text: display.clone().unwrap_or_else(|| content.clone()),
                }),
                _ => None,
            })
            .collect()
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
            // none, no change was made since.
            let commit = branch[position..].iter().find_map(|e| match &e.kind {
                EntryKind::Checkpoint { commit } => Some(commit.clone()),
                _ => None,
            });
            if let Some(commit) = commit {
                let restored =
                    tokio::task::spawn_blocking(move || checkpoints.restore(&commit)).await;
                snapshot = Some(restored.map_err(|e| CheckpointError::Io(e.into()))??);
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
        Ok(())
    }

    /// Undoes the last rewind, when nothing has happened since: restores the files and the
    /// conversation as they were just before it.
    pub async fn undo_rewind(&mut self) -> Result<(), RewindError> {
        let Some(Entry {
            id,
            kind: EntryKind::Rewind { from, snapshot, .. },
            ..
        }) = self.session.get(self.session.leaf()).cloned()
        else {
            return Err(RewindError::NothingToUndo);
        };
        if let Some(snapshot) = snapshot {
            let checkpoints = self.checkpoints.clone().ok_or(RewindError::NoCheckpoints)?;
            tokio::task::spawn_blocking(move || checkpoints.restore(&snapshot))
                .await
                .map_err(|e| CheckpointError::Io(e.into()))??;
        }
        self.session
            .append_under(&from, EntryKind::UndoRewind { rewind: id });
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
        let Some(checkpoints) = self.checkpoints.clone() else {
            return;
        };
        self.turn_checkpointed = true;
        let message = format!("before a turn of session {}", self.session.id());
        let result = tokio::task::spawn_blocking(move || checkpoints.snapshot(&message)).await;
        match result {
            Ok(Ok(commit)) => {
                self.session.append(EntryKind::Checkpoint {
                    commit: commit.clone(),
                });
                self.note_save_error();
                let _ = events.send(AgentEvent::CheckpointCreated { commit });
            }
            Ok(Err(e)) => self.disable_checkpoints(&e.to_string(), events),
            Err(e) => self.disable_checkpoints(&e.to_string(), events),
        }
    }

    fn disable_checkpoints(&mut self, why: &str, events: &UnboundedSender<AgentEvent>) {
        self.checkpoints = None;
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
        let id = self.session.append(EntryKind::Message {
            message: message.clone(),
            display,
            note,
        });
        self.history.push(message);
        self.history_ids.push(id);
        self.note_save_error();
    }

    pub fn config_mut(&mut self) -> &mut AgentConfig {
        &mut self.config
    }

    /// Invalid tool calls (unknown tool, bad JSON, schema violations) in the current or last turn.
    pub fn invalid_calls_this_turn(&self) -> u32 {
        self.invalid_calls
    }

    /// Switches the approval mode between turns. The system prompt stays as it is, so providers
    /// keep reusing their prompt caches; the change is appended to the conversation as a note.
    pub fn set_mode(&mut self, mode: Mode) {
        self.policy.set_mode(mode);
        self.ctx.access = mode.fs_access();
        self.record(
            Message::User {
                content: mode_note(mode),
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
        self.turn_checkpointed = false;
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
        let content = self.user_message(input.parts, events).await;
        self.record(Message::User { content }, input.display, false);
        if cancel.is_cancelled() {
            return self.finish(TurnEndReason::Interrupted, events);
        }

        let mut auto_compaction_failed = false;
        for _ in 0..self.config.max_steps {
            if !auto_compaction_failed
                && self.near_the_window()
                && let Err(e) = self
                    .compact_history(None, Trigger::Auto, events, &cancel)
                    .await
            {
                auto_compaction_failed = true;
                if !cancel.is_cancelled() {
                    let _ = events.send(AgentEvent::Warning {
                        message: format!("could not compact the conversation: {e}"),
                    });
                }
            }
            let reply = match self.call_model_compacting(events, &cancel).await {
                ModelOutcome::Reply(mut reply) => {
                    self.dedupe_call_ids(&mut reply.tool_calls);
                    if let Some(usage) = reply.usage {
                        let total = usage.input_tokens + usage.output_tokens;
                        self.reported_usage = Some((total, self.history.len() + 1));
                    }
                    reply
                }
                ModelOutcome::Failed(error, partial) => return self.fail(error, partial, events),
                ModelOutcome::Interrupted(partial) => {
                    if !partial.text.is_empty() {
                        self.push_assistant(partial.text, Vec::new(), events);
                    }
                    return self.finish(TurnEndReason::Interrupted, events);
                }
            };
            let calls = reply.tool_calls.clone();
            self.push_assistant(reply.text, calls.clone(), events);
            if calls.is_empty() {
                return self.finish(TurnEndReason::Completed, events);
            }
            for (index, call) in calls.iter().enumerate() {
                if cancel.is_cancelled() {
                    // Every tool call needs a result, or the next request would be rejected.
                    for skipped in &calls[index..] {
                        let message = Message::Tool {
                            call_id: skipped.id.clone(),
                            content: "interrupted by the user before this tool ran".into(),
                            is_error: true,
                        };
                        self.record(message, None, false);
                    }
                    return self.finish(TurnEndReason::Interrupted, events);
                }
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
        }
        self.finish(TurnEndReason::StepLimit, events)
    }

    /// The turn's user message: text parts as they are, and each shell part replaced by the output
    /// of running it as a `bash` tool call, with the same permission check, approval and sandbox.
    async fn user_message(
        &mut self,
        parts: Vec<InputPart>,
        events: &UnboundedSender<AgentEvent>,
    ) -> String {
        let mut message = String::new();
        for part in parts {
            match part {
                InputPart::Text(text) => message.push_str(&text),
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
        result
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
        let threshold = self.config.context_window as f64 * self.config.compaction.threshold;
        self.estimated_tokens() as f64 >= threshold
    }

    /// Replaces the older part of the conversation by a summary. The kept part fits the
    /// configured share of the window; failing that, automatic and overflow compaction keep the
    /// current turn, and `/compact` summarizes everything. The summary is saved as a compaction
    /// entry, so the summarized messages stay in the session and can be rewound to.
    async fn compact_history(
        &mut self,
        focus: Option<&str>,
        trigger: Trigger,
        events: &UnboundedSender<AgentEvent>,
        cancel: &CancellationToken,
    ) -> Result<(), String> {
        let window = self.config.context_window;
        let budget = (window as f64 * self.config.compaction.keep_recent) as u64;
        let fits = compaction::cut(&self.history, budget);
        let turn_start = self
            .history
            .iter()
            .rposition(|m| matches!(m, Message::User { .. }))
            .unwrap_or(0);
        let cut = match trigger {
            Trigger::Auto => fits.or(Some(turn_start)),
            // The provider's window is smaller than assumed: keep no more than the current turn.
            Trigger::Overflow => Some(fits.unwrap_or(0).max(turn_start)),
            Trigger::Manual => fits.or(Some(self.history.len())),
        }
        .filter(|&cut| cut > 0)
        .ok_or("there is nothing to compact yet")?;
        let before = self.estimated_tokens();
        let model = self
            .turn_model
            .as_ref()
            .map_or(self.config.model_name.clone(), |m| m.name.clone());
        let request = compaction::summary_request(&model, &self.history[..cut], focus, window / 2);
        let summary = self.summarize(request, events, cancel).await?;
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
    ) -> Result<String, String> {
        let provider = self
            .turn_model
            .as_ref()
            .map_or(&self.provider, |m| &m.provider)
            .clone();
        let mut attempt = 1;
        loop {
            let collect = async {
                let mut text = String::new();
                let mut stream = provider.stream(request.clone());
                while let Some(item) = stream.next().await {
                    if let ProviderEvent::TextDelta(delta) = item? {
                        text.push_str(&delta);
                    }
                }
                Ok::<String, ProviderError>(text)
            };
            let result = tokio::select! {
                result = collect => result,
                _ = cancel.cancelled() => return Err("interrupted".into()),
            };
            match result {
                Ok(text) if text.trim().is_empty() => {
                    return Err("the model returned an empty summary".into());
                }
                Ok(text) => return Ok(text.trim().to_string()),
                Err(error) if error.is_retryable() && attempt < self.config.retry.max_attempts => {
                    let delay = self.config.retry.delay(attempt, error.retry_after());
                    let _ = events.send(AgentEvent::Retrying {
                        attempt,
                        reason: error.to_string(),
                        delay_ms: delay.as_millis() as u64,
                    });
                    tokio::select! {
                        _ = tokio::time::sleep(delay) => {}
                        _ = cancel.cancelled() => return Err("interrupted".into()),
                    }
                    attempt += 1;
                }
                Err(error) => return Err(describe(&error)),
            }
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
            Err(e) => {
                let _ = events.send(AgentEvent::Warning {
                    message: format!("could not compact the conversation: {e}"),
                });
                outcome
            }
        }
    }

    /// The id of the model answering the current turn.
    fn model_id(&self) -> &str {
        self.turn_model
            .as_ref()
            .map_or(&self.config.model_id, |model| &model.id)
    }

    /// One model call with retries for transient errors. Never retries once output reached the user.
    async fn call_model(
        &self,
        events: &UnboundedSender<AgentEvent>,
        cancel: &CancellationToken,
    ) -> ModelOutcome {
        let mut attempt = 1;
        loop {
            let mut reply = ModelReply::default();
            let result = tokio::select! {
                result = self.stream_into(&mut reply, events) => Some(result),
                _ = cancel.cancelled() => None,
            };
            match result {
                None => return ModelOutcome::Interrupted(reply),
                Some(Ok(())) => return ModelOutcome::Reply(reply),
                Some(Err(error))
                    if error.is_retryable()
                        && !reply.emitted
                        && attempt < self.config.retry.max_attempts
                        && !error
                            .retry_after()
                            .is_some_and(|d| d > crate::retry::MAX_AUTOMATIC_RETRY_AFTER) =>
                {
                    let delay = self.config.retry.delay(attempt, error.retry_after());
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

    fn finish(
        &mut self,
        reason: TurnEndReason,
        events: &UnboundedSender<AgentEvent>,
    ) -> TurnEndReason {
        for message in self.warnings.drain(..) {
            let _ = events.send(AgentEvent::Warning { message });
        }
        let _ = events.send(AgentEvent::TurnFinished { reason });
        reason
    }

    fn fail(
        &mut self,
        error: ProviderError,
        partial: ModelReply,
        events: &UnboundedSender<AgentEvent>,
    ) -> TurnEndReason {
        if !partial.text.is_empty() {
            self.push_assistant(partial.text, Vec::new(), events);
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
        let (provider, model) = match &self.turn_model {
            Some(turn) => (&turn.provider, turn.name.clone()),
            None => (&self.provider, self.config.model_name.clone()),
        };
        let request = ChatRequest {
            model,
            system: self.config.system_prompt.clone(),
            messages: self.history.clone(),
            tools: self.tools.specs(),
        };
        let mut stream = provider.stream(request);
        while let Some(item) = stream.next().await {
            match item? {
                ProviderEvent::TextDelta(text) => {
                    reply.emitted = true;
                    reply.text.push_str(&text);
                    let _ = events.send(AgentEvent::TextDelta { text });
                }
                ProviderEvent::ReasoningDelta(text) => {
                    reply.emitted = true;
                    let _ = events.send(AgentEvent::ReasoningDelta { text });
                }
                ProviderEvent::ToolCall(call) => reply.tool_calls.push(call),
                ProviderEvent::Usage(usage) => {
                    reply.usage = Some(usage);
                    let _ = events.send(AgentEvent::Usage {
                        model: self.model_id().to_string(),
                        usage,
                    });
                }
                ProviderEvent::Finished(reason) => reply.finish = Some(reason),
            }
        }
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
        let content = limit_output(
            &raw.content,
            self.config.output_limit,
            &self.config.output_dir,
            &call.id,
        );
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
        match self.policy.check(&action) {
            Decision::Allow => {}
            Decision::Deny(reason) => return ToolOutput::error(format!("denied: {reason}")),
            Decision::Ask(reason) => {
                let _ = events.send(AgentEvent::ApprovalNeeded {
                    id: call.id.clone(),
                    reason: reason.clone(),
                });
                let request = ApprovalRequest {
                    call_id: call.id.clone(),
                    tool: call.name.clone(),
                    action,
                    reason: reason.clone(),
                };
                match self.approver.decide(&request).await {
                    ApprovalDecision::Approve => {}
                    ApprovalDecision::ApproveForSession => {
                        self.policy.remember(&request.action);
                    }
                    ApprovalDecision::Deny {
                        feedback: Some(note),
                    } => {
                        return ToolOutput::error(format!("the user denied this action: {note}"));
                    }
                    ApprovalDecision::Deny { feedback: None } => {
                        return ToolOutput::error("the user denied this action");
                    }
                    ApprovalDecision::Unavailable => {
                        let _ = events.send(AgentEvent::ActionBlocked {
                            id: call.id.clone(),
                            reason: reason.clone(),
                        });
                        return ToolOutput::error(format!(
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
        let _ = events.send(AgentEvent::ApprovalNeeded {
            id: call.id.clone(),
            reason: reason.clone(),
        });
        let request = ApprovalRequest {
            call_id: call.id.clone(),
            tool: call.name.clone(),
            action: tool.action(&args, &self.ctx),
            reason,
        };
        let note = match self.approver.decide(&request).await {
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

/// The result given to a tool call that a stopped run left without one.
fn stopped_result(call_id: String) -> Message {
    Message::Tool {
        call_id,
        content: "harness stopped before this tool call finished; its effects are unknown".into(),
        is_error: true,
    }
}

/// The note appended to the conversation when the approval mode changes.
fn mode_note(mode: Mode) -> String {
    let rules = match mode {
        Mode::Plan | Mode::ReadOnly => "file edits are refused and shell commands can only read",
        Mode::Ask => {
            "file edits and shell commands need the user's approval unless a rule allows them"
        }
        Mode::Auto => {
            "file edits in the workspace and sandboxed shell commands run without approval"
        }
        Mode::FullAccess => {
            "actions run without approval or sandbox, except those a deny rule forbids"
        }
    };
    format!("[harness] The approval mode is now {mode}: {rules}.")
}

/// A human-readable error message for the user.
fn describe(error: &ProviderError) -> String {
    if let Some(wait) = error.retry_after()
        && wait > crate::retry::MAX_AUTOMATIC_RETRY_AFTER
    {
        return format!(
            "the provider asked to wait {}s before retrying, which is longer than we wait automatically. {error}",
            wait.as_secs()
        );
    }
    match error {
        ProviderError::Http { status: 429, .. } => {
            format!(
                "{error}. The provider is rate limiting; try again later or switch models with --model."
            )
        }
        ProviderError::Http { status, body, .. } => {
            let body: String = body.chars().take(500).collect();
            format!("HTTP {status}: {body}")
        }
        other => other.to_string(),
    }
}
