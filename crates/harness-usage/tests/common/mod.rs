#![allow(dead_code)]

use std::{path::Path, sync::Arc};

use async_trait::async_trait;
use harness_core::{
    agent::{Agent, AgentConfig, NonInteractive},
    engine::{EngineConfig, PermissionEngine},
    event::{AgentEvent, TurnEndReason},
    message::ToolSpec,
    permission::{Action, Mode},
    testing::MockProvider,
    tool::{Tool, ToolContext, ToolOutput, ToolRegistry},
};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Returns its `text` argument.
pub struct Echo;

#[async_trait]
impl Tool for Echo {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "echo".into(),
            description: "test tool".into(),
            parameters: json!({"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"], "additionalProperties": false}),
        }
    }
    fn action(&self, _args: &Value, ctx: &ToolContext) -> Action {
        Action::Read(ctx.workspace.clone())
    }
    async fn run(&self, args: Value, _ctx: &ToolContext) -> ToolOutput {
        ToolOutput::ok(args["text"].as_str().unwrap_or_default())
    }
}

/// An agent on `provider` in `dir`, in auto mode, with the `echo` tool.
pub fn agent(provider: Arc<MockProvider>, dir: &Path) -> Agent {
    let policy = Arc::new(PermissionEngine::new(EngineConfig {
        mode: Mode::Auto,
        workspace: dir.to_path_buf(),
        read_dirs: vec![],
        rules: Default::default(),
        sandbox_available: false,
        writes_need_approval: false,
    }));
    Agent::new(
        provider,
        ToolRegistry::new(vec![Arc::new(Echo)]),
        policy,
        Arc::new(NonInteractive),
        AgentConfig::new("mock/m1", "m1", "system prompt", dir.join(".spill")),
        ToolContext::new(dir).with_sandbox(None, Mode::Auto.fs_access()),
    )
}

/// Runs one turn, and returns how it ended and its events.
pub async fn run(agent: &mut Agent, input: &str) -> (TurnEndReason, Vec<AgentEvent>) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let reason = agent
        .run_turn(input.to_string(), &tx, CancellationToken::new())
        .await;
    drop(tx);
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }
    (reason, events)
}
