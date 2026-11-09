//! A tool added to one agent with `with_extra_tool` (the one that starts subagents) stays through
//! model switches and turns on another model, is not handed to the children built from the
//! agent's model, and sees which turn it is called in.

mod common;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use common::run;
use harness_core::{
    agent::{Agent, AgentConfig, NonInteractive, SessionModel},
    agents::ParentModel,
    engine::{EngineConfig, PermissionEngine},
    message::{RequestOptions, ToolSpec},
    permission::{Action, Mode},
    testing::{MockProvider, Script},
    tool::{Tool, ToolContext, ToolOutput, ToolRegistry},
};
use serde_json::{Value, json};

/// Says in which turn it was called.
#[derive(Default)]
struct Extra {
    turns: Mutex<Vec<u64>>,
}

#[async_trait]
impl Tool for Extra {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "extra".into(),
            description: "the extra tool".into(),
            parameters: json!({"type": "object", "properties": {}, "additionalProperties": false}),
        }
    }
    fn action(&self, _args: &Value, ctx: &ToolContext) -> Action {
        Action::Read(ctx.workspace.clone())
    }
    async fn run(&self, _args: Value, ctx: &ToolContext) -> ToolOutput {
        self.turns.lock().unwrap().push(ctx.turn);
        ToolOutput::ok("extra ran")
    }
}

struct Plain(&'static str);

#[async_trait]
impl Tool for Plain {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.0.into(),
            description: String::new(),
            parameters: json!({"type": "object"}),
        }
    }
    fn action(&self, _args: &Value, ctx: &ToolContext) -> Action {
        Action::Read(ctx.workspace.clone())
    }
    async fn run(&self, _args: Value, _ctx: &ToolContext) -> ToolOutput {
        ToolOutput::ok("")
    }
}

fn registry(names: &[&'static str]) -> ToolRegistry {
    ToolRegistry::new(
        names
            .iter()
            .map(|n| Arc::new(Plain(n)) as Arc<dyn Tool>)
            .collect(),
    )
}

fn agent(provider: Arc<MockProvider>, dir: &std::path::Path, extra: Arc<Extra>) -> Agent {
    let policy = Arc::new(PermissionEngine::new(EngineConfig {
        mode: Mode::Auto,
        workspace: dir.to_path_buf(),
        read_dirs: vec![],
        rules: Default::default(),
        sandbox_available: true,
        writes_need_approval: false,
    }));
    Agent::new(
        provider,
        registry(&["read", "edit"]),
        policy,
        Arc::new(NonInteractive),
        AgentConfig::new("mock/a", "a", "prompt", dir.join(".spill")),
        ToolContext::new(dir),
    )
    .with_extra_tool(extra)
}

fn names(provider: &MockProvider, request: usize) -> Vec<String> {
    provider.requests()[request]
        .tools
        .iter()
        .map(|t| t.name.clone())
        .collect()
}

#[tokio::test]
async fn an_extra_tool_is_offered_after_the_others_and_stays_when_the_model_switches() {
    let dir = tempfile::tempdir().unwrap();
    let first = MockProvider::new(vec![Script::text("one")]);
    let second = MockProvider::new(vec![Script::text("two")]);
    let mut agent = agent(first.clone(), dir.path(), Arc::default());
    run(&mut agent, "hello").await;
    agent.switch_model(SessionModel {
        provider: second.clone(),
        id: "mock/b".into(),
        name: "b".into(),
        context_window: 32_768,
        request: RequestOptions::default(),
        text_tool_calls: false,
        tools: Some(registry(&["read", "apply_patch"])),
        edit_section: None,
    });
    run(&mut agent, "again").await;
    assert_eq!(names(&first, 0), ["read", "edit", "extra"]);
    assert_eq!(names(&second, 0), ["read", "apply_patch", "extra"]);
}

#[tokio::test]
async fn the_model_children_are_built_from_does_not_include_the_extra_tool() {
    let dir = tempfile::tempdir().unwrap();
    let cell = ParentModel::default();
    let agent =
        agent(MockProvider::new(vec![]), dir.path(), Arc::default()).with_model_cell(cell.clone());
    let published: Vec<String> = cell
        .get()
        .unwrap()
        .tools
        .specs()
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert_eq!(published, ["read", "edit"]);
    // The agent itself still offers it.
    assert!(agent.tool_specs().iter().any(|s| s.name == "extra"));
}

#[tokio::test]
async fn a_tool_sees_a_different_turn_number_in_each_turn_and_the_same_one_within_a_turn() {
    let dir = tempfile::tempdir().unwrap();
    let extra = Arc::new(Extra::default());
    let provider = MockProvider::new(vec![
        Script::tool_call("c1", "extra", json!({})),
        Script::tool_call("c2", "extra", json!({})),
        Script::text("done one"),
        Script::tool_call("c3", "extra", json!({})),
        Script::text("done two"),
    ]);
    let mut agent = agent(provider, dir.path(), extra.clone());
    run(&mut agent, "one").await;
    run(&mut agent, "two").await;
    let turns = extra.turns.lock().unwrap().clone();
    assert_eq!(turns.len(), 3);
    assert_eq!(turns[0], turns[1]);
    assert_ne!(turns[1], turns[2]);
}
