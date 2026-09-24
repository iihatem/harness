//! The agent loop: call the model, run the tools it requests, feed results back, repeat.

use std::{collections::HashMap, path::PathBuf, sync::Arc};

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::{
    event::{AgentEvent, ErrorKind, TurnEndReason},
    message::{ChatRequest, Message, ToolCall},
    output::{DEFAULT_OUTPUT_LIMIT, limit_output},
    permission::{Action, Decision, PermissionPolicy},
    provider::{FinishReason, Provider, ProviderError, ProviderEvent},
    tool::{ToolContext, ToolOutput, ToolRegistry},
};

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
            max_steps: 50,
            output_limit: DEFAULT_OUTPUT_LIMIT,
            output_dir,
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

    /// Runs one user turn to completion, reporting everything on `events`.
    pub async fn run_turn(
        &mut self,
        input: String,
        events: &UnboundedSender<AgentEvent>,
        cancel: CancellationToken,
    ) -> TurnEndReason {
        self.ctx.cancel = cancel;
        self.invalid_calls = 0;
        let _ = events.send(AgentEvent::TurnStarted);
        self.history.push(Message::User { content: input });

        for _ in 0..self.config.max_steps {
            let mut reply = ModelReply::default();
            if let Err(error) = self.stream_into(&mut reply, events).await {
                return self.fail(error, reply, events);
            }
            let calls = std::mem::take(&mut reply.tool_calls);
            self.push_assistant(reply.text, calls.clone(), events);
            if calls.is_empty() {
                return self.finish(TurnEndReason::Completed, events);
            }
            for call in calls {
                let output = self.execute(&call, events).await;
                self.history.push(Message::Tool {
                    call_id: call.id,
                    content: output.content,
                    is_error: output.is_error,
                });
            }
        }
        self.finish(TurnEndReason::StepLimit, events)
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
        let output = ToolOutput {
            content,
            is_error: raw.is_error,
        };
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
        tool.run(args, &self.ctx).await
    }
}

/// A human-readable error message for the user.
fn describe(error: &ProviderError) -> String {
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
