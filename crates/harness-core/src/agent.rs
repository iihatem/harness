//! The agent loop: call the model, run the tools it requests, feed results back, repeat.

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
};

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::{
    event::{AgentEvent, ErrorKind, TurnEndReason},
    message::{ChatRequest, Message, ToolCall},
    output::{DEFAULT_OUTPUT_LIMIT, limit_output},
    permission::{Action, Decision, FsAccess, PermissionPolicy},
    provider::{FinishReason, Provider, ProviderError, ProviderEvent},
    retry::RetryPolicy,
    tool::{Tool, ToolContext, ToolOutput, ToolRegistry},
};

/// Model calls allowed per turn unless configured otherwise.
pub const DEFAULT_MAX_STEPS: u32 = 50;

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
    history: Vec<Message>,
    validators: HashMap<String, jsonschema::Validator>,
    invalid_calls: u32,
    /// Tool-call ids already used in this session, so a missing or repeated id (from a model or
    /// provider that doesn't guarantee unique ids) can be rewritten before it collides.
    used_call_ids: HashSet<String>,
    next_call_id: u64,
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
            ctx,
            history: Vec::new(),
            validators,
            invalid_calls: 0,
            used_call_ids: HashSet::new(),
            next_call_id: 0,
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

    pub fn history(&self) -> &[Message] {
        &self.history
    }

    pub fn config_mut(&mut self) -> &mut AgentConfig {
        &mut self.config
    }

    /// Invalid tool calls (unknown tool, bad JSON, schema violations) in the current or last turn.
    pub fn invalid_calls_this_turn(&self) -> u32 {
        self.invalid_calls
    }

    /// Runs one user turn to completion, reporting everything on `events`. Cancelling `cancel` stops the
    /// turn promptly: in-flight model calls are dropped and running tools are told to stop.
    pub async fn run_turn(
        &mut self,
        input: String,
        events: &UnboundedSender<AgentEvent>,
        cancel: CancellationToken,
    ) -> TurnEndReason {
        self.ctx.cancel = cancel.clone();
        self.invalid_calls = 0;
        let _ = events.send(AgentEvent::TurnStarted);
        self.history.push(Message::User { content: input });

        for _ in 0..self.config.max_steps {
            let reply = match self.call_model(events, &cancel).await {
                ModelOutcome::Reply(mut reply) => {
                    self.dedupe_call_ids(&mut reply.tool_calls);
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
                        self.history.push(Message::Tool {
                            call_id: skipped.id.clone(),
                            content: "interrupted by the user before this tool ran".into(),
                            is_error: true,
                        });
                    }
                    return self.finish(TurnEndReason::Interrupted, events);
                }
                let output = self.execute(call, events).await;
                self.history.push(Message::Tool {
                    call_id: call.id.clone(),
                    content: output.content,
                    is_error: output.is_error,
                });
            }
            if cancel.is_cancelled() {
                return self.finish(TurnEndReason::Interrupted, events);
            }
        }
        self.finish(TurnEndReason::StepLimit, events)
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
        let model = self.config.model_id.clone();
        let _ = events.send(AgentEvent::AssistantMessage {
            content: text.clone(),
            model: model.clone(),
        });
        self.history.push(Message::Assistant {
            content: text,
            tool_calls,
            model,
        });
    }

    fn finish(&self, reason: TurnEndReason, events: &UnboundedSender<AgentEvent>) -> TurnEndReason {
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
        let request = ChatRequest {
            model: self.config.model_name.clone(),
            system: self.config.system_prompt.clone(),
            messages: self.history.clone(),
            tools: self.tools.specs(),
        };
        let mut stream = self.provider.stream(request);
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
                    let _ = events.send(AgentEvent::Usage {
                        model: self.config.model_id.clone(),
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
