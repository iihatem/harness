//! Input the user sends while a turn runs ("send now") reaches the model with the next tool
//! results of that turn.

mod common;

use std::sync::Arc;

use async_trait::async_trait;
use common::{Echo, run};
use harness_core::{
    agent::{Agent, AgentConfig, NonInteractive},
    engine::{EngineConfig, PermissionEngine},
    event::AgentEvent,
    message::{Message, ToolSpec},
    permission::{Action, Mode},
    testing::{MockProvider, Script},
    tool::{Tool, ToolContext, ToolOutput, ToolRegistry},
    turn::Steering,
};
use serde_json::{Value, json};

/// A long-running tool, during which the user sends input, and maybe stops the turn.
struct Tests(Steering, Option<tokio_util::sync::CancellationToken>);

#[async_trait]
impl Tool for Tests {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "run_tests".into(),
            description: "runs the tests".into(),
            parameters: json!({"type": "object"}),
        }
    }
    fn action(&self, _args: &Value, ctx: &ToolContext) -> Action {
        Action::Read(ctx.workspace.clone())
    }
    async fn run(&self, _args: Value, _ctx: &ToolContext) -> ToolOutput {
        self.0.send("use the v2 API instead");
        if let Some(stop) = &self.1 {
            stop.cancel();
        }
        ToolOutput::ok("3 tests failed")
    }
}

fn agent(provider: Arc<MockProvider>, dir: &std::path::Path, steering: &Steering) -> Agent {
    agent_stopping(provider, dir, steering, None)
}

fn agent_stopping(
    provider: Arc<MockProvider>,
    dir: &std::path::Path,
    steering: &Steering,
    stop: Option<tokio_util::sync::CancellationToken>,
) -> Agent {
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
        ToolRegistry::new(vec![
            Arc::new(Tests(steering.clone(), stop)),
            Arc::new(Echo),
        ]),
        policy,
        Arc::new(NonInteractive),
        AgentConfig::new("mock/m1", "m1", "system prompt", dir.join(".spill")),
        ToolContext::new(dir),
    )
    .with_steering(steering.clone())
}

#[tokio::test]
async fn input_sent_now_goes_with_the_next_tool_result_in_the_same_turn() {
    let dir = tempfile::tempdir().unwrap();
    let steering = Steering::new();
    let provider = MockProvider::new(vec![
        Script::tool_call("t1", "run_tests", json!({})),
        Script::text("Switching to the v2 API."),
    ]);
    let mut agent = agent(provider.clone(), dir.path(), &steering);
    let (_, events) = run(&mut agent, "fix the tests").await;
    let second = &provider.requests()[1].messages;
    let n = second.len();
    assert!(
        matches!(&second[n - 2], Message::Tool { call_id, .. } if call_id == "t1"),
        "{second:#?}"
    );
    assert_eq!(
        second[n - 1],
        Message::User {
            content: "use the v2 API instead".into()
        }
    );
    // Reported where it happened: after the tool finished, before the next reply.
    let steered = events
        .iter()
        .position(|e| matches!(e, AgentEvent::Steered { text } if text == "use the v2 API instead"))
        .expect("a steered event");
    let finished = events
        .iter()
        .position(|e| matches!(e, AgentEvent::ToolCallFinished { .. }))
        .unwrap();
    let replied = events
        .iter()
        .position(
            |e| matches!(e, AgentEvent::AssistantMessage { content, .. } if content.contains("v2")),
        )
        .unwrap();
    assert!(finished < steered && steered < replied);
    assert!(steering.is_empty());
    // The session keeps it as a user message of that turn.
    assert!(
        agent
            .rewind_points()
            .iter()
            .any(|p| p.text == "use the v2 API instead")
    );
}

#[tokio::test]
async fn input_sent_after_the_last_tool_result_waits_for_the_frontend() {
    let dir = tempfile::tempdir().unwrap();
    let steering = Steering::new();
    let provider = MockProvider::new(vec![Script::text("No tools needed.")]);
    let mut agent = agent(provider.clone(), dir.path(), &steering);
    steering.send("too late for this turn");
    let (_, events) = run(&mut agent, "hi").await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::Steered { .. }))
    );
    // Nothing took it: the frontend sends it as the next turn.
    assert_eq!(steering.take(), ["too late for this turn"]);
}

#[tokio::test]
async fn an_interrupted_turn_leaves_what_was_sent_for_the_frontend() {
    let dir = tempfile::tempdir().unwrap();
    let steering = Steering::new();
    let provider = MockProvider::new(vec![
        Script::tool_call("t1", "run_tests", json!({})),
        Script::text("never asked"),
    ]);
    let cancel = tokio_util::sync::CancellationToken::new();
    let mut agent = agent_stopping(
        provider.clone(),
        dir.path(),
        &steering,
        Some(cancel.clone()),
    );
    let (reason, events) = common::run_with(&mut agent, "go", cancel).await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::Steered { .. }))
    );
    assert_eq!(reason, harness_core::event::TurnEndReason::Interrupted);
    assert_eq!(steering.take(), ["use the v2 API instead"]);
}
