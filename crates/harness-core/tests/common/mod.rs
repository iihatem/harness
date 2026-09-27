#![allow(dead_code)]

use std::{path::Path, sync::Arc, time::Duration};

use async_trait::async_trait;
use harness_core::agent::{Agent, AgentConfig, ApprovalDecision, ApprovalRequest, Approver};
use harness_core::engine::{EngineConfig, PermissionEngine};
use harness_core::event::{AgentEvent, TurnEndReason};
use harness_core::message::ToolSpec;
use harness_core::permission::{Action, Mode};
use harness_core::testing::MockProvider;
use harness_core::tool::{Tool, ToolContext, ToolOutput, ToolRegistry};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

fn spec(name: &str, params: Value) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: format!("test tool {name}"),
        parameters: params,
    }
}

/// Returns its `text` argument. Reads the workspace, so it is always allowed.
pub struct Echo;
#[async_trait]
impl Tool for Echo {
    fn spec(&self) -> ToolSpec {
        spec(
            "echo",
            json!({"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"], "additionalProperties": false}),
        )
    }
    fn action(&self, _args: &Value, ctx: &ToolContext) -> Action {
        Action::Read(ctx.workspace.clone())
    }
    async fn run(&self, args: Value, _ctx: &ToolContext) -> ToolOutput {
        ToolOutput::ok(args["text"].as_str().unwrap_or_default())
    }
}

/// Creates an empty file at `path`.
pub struct Touch;
#[async_trait]
impl Tool for Touch {
    fn spec(&self) -> ToolSpec {
        spec(
            "touch",
            json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}),
        )
    }
    fn action(&self, args: &Value, ctx: &ToolContext) -> Action {
        Action::Write(ctx.resolve(args["path"].as_str().unwrap_or_default()))
    }
    async fn run(&self, args: Value, ctx: &ToolContext) -> ToolOutput {
        std::fs::write(ctx.resolve(args["path"].as_str().unwrap_or_default()), "").unwrap();
        ToolOutput::ok("touched")
    }
}

/// Always fails.
pub struct Fail;
#[async_trait]
impl Tool for Fail {
    fn spec(&self) -> ToolSpec {
        spec("fail", json!({"type": "object"}))
    }
    fn action(&self, _args: &Value, ctx: &ToolContext) -> Action {
        Action::Read(ctx.workspace.clone())
    }
    async fn run(&self, _args: Value, _ctx: &ToolContext) -> ToolOutput {
        ToolOutput::error("tool exploded")
    }
}

/// Waits until the turn is cancelled (or 600 s pass).
pub struct Sleepy;
#[async_trait]
impl Tool for Sleepy {
    fn spec(&self) -> ToolSpec {
        spec("sleepy", json!({"type": "object"}))
    }
    fn action(&self, _args: &Value, ctx: &ToolContext) -> Action {
        Action::Read(ctx.workspace.clone())
    }
    async fn run(&self, _args: Value, ctx: &ToolContext) -> ToolOutput {
        tokio::select! {
            _ = ctx.cancel.cancelled() => ToolOutput::error("interrupted"),
            _ = tokio::time::sleep(Duration::from_secs(600)) => ToolOutput::ok("slept"),
        }
    }
}

/// Fails with a sandbox denial unless the context allows an unsandboxed re-run.
pub struct Boxed;
#[async_trait]
impl Tool for Boxed {
    fn spec(&self) -> ToolSpec {
        spec("boxed", json!({"type": "object"}))
    }
    fn action(&self, _args: &Value, _ctx: &ToolContext) -> Action {
        Action::Bash("curl https://example.com".into())
    }
    async fn run(&self, _args: Value, ctx: &ToolContext) -> ToolOutput {
        if ctx.unsandboxed {
            return ToolOutput::ok("ran without the sandbox");
        }
        let mut out = ToolOutput::error("exit code 6\ncurl: (6) Could not resolve host");
        out.sandbox_denied = true;
        out
    }
}

pub struct AlwaysApprove;
#[async_trait]
impl Approver for AlwaysApprove {
    async fn decide(&self, _request: &ApprovalRequest) -> ApprovalDecision {
        ApprovalDecision::Approve
    }
}

pub struct ApproveForSession;
#[async_trait]
impl Approver for ApproveForSession {
    async fn decide(&self, _request: &ApprovalRequest) -> ApprovalDecision {
        ApprovalDecision::ApproveForSession
    }
}

pub struct DenyWith(pub &'static str);
#[async_trait]
impl Approver for DenyWith {
    async fn decide(&self, _request: &ApprovalRequest) -> ApprovalDecision {
        ApprovalDecision::Deny {
            feedback: Some(self.0.to_string()),
        }
    }
}

fn build(
    provider: Arc<MockProvider>,
    mode: Mode,
    approver: Arc<dyn Approver>,
    dir: &Path,
    sandbox: bool,
) -> Agent {
    let tools = ToolRegistry::new(vec![
        Arc::new(Echo),
        Arc::new(Touch),
        Arc::new(Fail),
        Arc::new(Sleepy),
        Arc::new(Boxed),
    ]);
    let policy = Arc::new(PermissionEngine::new(EngineConfig {
        mode,
        workspace: dir.to_path_buf(),
        read_dirs: vec![],
        rules: Default::default(),
        sandbox_available: sandbox,
    }));
    let config = AgentConfig::new("mock/m1", "m1", "system prompt", dir.join(".spill"));
    Agent::new(
        provider,
        tools,
        policy,
        approver,
        config,
        ToolContext::new(dir).with_sandbox(None, mode.fs_access()),
    )
}

pub fn agent(
    provider: Arc<MockProvider>,
    mode: Mode,
    approver: Arc<dyn Approver>,
    dir: &Path,
) -> Agent {
    build(provider, mode, approver, dir, false)
}

pub fn agent_with_sandbox(
    provider: Arc<MockProvider>,
    mode: Mode,
    approver: Arc<dyn Approver>,
    dir: &Path,
) -> Agent {
    build(provider, mode, approver, dir, true)
}

pub async fn run(agent: &mut Agent, input: &str) -> (TurnEndReason, Vec<AgentEvent>) {
    run_with(agent, input, CancellationToken::new()).await
}

pub async fn run_with(
    agent: &mut Agent,
    input: &str,
    cancel: CancellationToken,
) -> (TurnEndReason, Vec<AgentEvent>) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let reason = agent.run_turn(input.to_string(), &tx, cancel).await;
    drop(tx);
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }
    (reason, events)
}

pub fn finished_outputs(events: &[AgentEvent]) -> Vec<(String, bool)> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolCallFinished {
                output, is_error, ..
            } => Some((output.clone(), *is_error)),
            _ => None,
        })
        .collect()
}
