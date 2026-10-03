//! The edit tool offered follows the model: it is chosen with the model, and a `/model` switch
//! changes it, with the tool section of the system prompt.

mod common;

use std::sync::Arc;

use async_trait::async_trait;
use common::run;
use harness_core::{
    agent::{Agent, AgentConfig, NonInteractive, SessionModel},
    engine::{EngineConfig, PermissionEngine},
    message::{RequestOptions, ToolSpec},
    permission::{Action, Mode},
    testing::{MockProvider, Script},
    tool::{Tool, ToolContext, ToolOutput, ToolRegistry},
};
use serde_json::{Value, json};

/// A tool called `name` that says it ran.
struct Named(&'static str);

#[async_trait]
impl Tool for Named {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.0.into(),
            description: format!("the {} tool", self.0),
            parameters: json!({"type": "object", "properties": {"input": {"type": "string"}}, "additionalProperties": false}),
        }
    }
    fn action(&self, _args: &Value, ctx: &ToolContext) -> Action {
        Action::Read(ctx.workspace.clone())
    }
    async fn run(&self, _args: Value, _ctx: &ToolContext) -> ToolOutput {
        ToolOutput::ok(format!("{} ran", self.0))
    }
}

const SECTION_A: &str = "Edit with `edit`.";
const SECTION_B: &str = "Edit with `apply_patch`.";

fn registry(names: &[&'static str]) -> ToolRegistry {
    ToolRegistry::new(
        names
            .iter()
            .map(|n| Arc::new(Named(n)) as Arc<dyn Tool>)
            .collect(),
    )
}

fn agent(provider: Arc<MockProvider>, dir: &std::path::Path) -> Agent {
    let policy = Arc::new(PermissionEngine::new(EngineConfig {
        mode: Mode::Auto,
        workspace: dir.to_path_buf(),
        read_dirs: vec![],
        rules: Default::default(),
        sandbox_available: true,
        writes_need_approval: false,
    }));
    let mut config = AgentConfig::new(
        "mock/a",
        "a",
        format!("You are an agent.\n{SECTION_A}\nEnd."),
        dir.join(".spill"),
    );
    config.edit_section = Some(SECTION_A.into());
    Agent::new(
        provider,
        registry(&["read", "edit", "bash"]),
        policy,
        Arc::new(NonInteractive),
        config,
        ToolContext::new(dir),
    )
}

fn model(
    provider: Arc<MockProvider>,
    id: &str,
    tools: Option<ToolRegistry>,
    section: Option<&str>,
) -> SessionModel {
    SessionModel {
        provider,
        id: id.into(),
        name: id.split('/').nth(1).unwrap().into(),
        context_window: 32_768,
        request: RequestOptions::default(),
        text_tool_calls: false,
        tools,
        edit_section: section.map(String::from),
    }
}

fn tool_names(provider: &MockProvider, request: usize) -> Vec<String> {
    provider.requests()[request]
        .tools
        .iter()
        .map(|t| t.name.clone())
        .collect()
}

// Spec "Switching model": the next request offers only the new model's edit tool, and the tool
// section of its system prompt describes it.
#[tokio::test]
async fn switching_model_changes_the_tool_and_the_tool_section() {
    let dir = tempfile::tempdir().unwrap();
    let first = MockProvider::new(vec![Script::text("one")]);
    let second = MockProvider::new(vec![Script::text("two")]);
    let mut agent = agent(first.clone(), dir.path());
    run(&mut agent, "hello").await;
    agent.switch_model(model(
        second.clone(),
        "mock/b",
        Some(registry(&["read", "apply_patch", "bash"])),
        Some(SECTION_B),
    ));
    run(&mut agent, "again").await;
    assert_eq!(tool_names(&first, 0), ["read", "edit", "bash"]);
    assert_eq!(tool_names(&second, 0), ["read", "apply_patch", "bash"]);
    let before = &first.requests()[0].system;
    let after = &second.requests()[0].system;
    assert!(before.contains(SECTION_A) && !before.contains(SECTION_B));
    assert!(after.contains(SECTION_B) && !after.contains(SECTION_A));
    // Only the section changed.
    assert_eq!(before.replace(SECTION_A, SECTION_B), *after);
}

// Spec: the prompt prefix stays stable within a model.
#[tokio::test]
async fn within_a_model_the_tools_and_the_prompt_do_not_change_between_requests() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call("t1", "edit", json!({"input": "x"})),
        Script::text("done"),
        Script::text("again"),
    ]);
    let mut agent = agent(provider.clone(), dir.path());
    run(&mut agent, "go").await;
    run(&mut agent, "more").await;
    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    for request in &requests[1..] {
        assert_eq!(request.system, requests[0].system);
        assert_eq!(
            serde_json::to_string(&request.tools).unwrap(),
            serde_json::to_string(&requests[0].tools).unwrap()
        );
    }
}

// A switch to a model with the same format changes nothing but the model.
#[tokio::test]
async fn a_switch_without_tools_or_a_section_keeps_both() {
    let dir = tempfile::tempdir().unwrap();
    let first = MockProvider::new(vec![Script::text("one")]);
    let second = MockProvider::new(vec![Script::text("two")]);
    let mut agent = agent(first.clone(), dir.path());
    run(&mut agent, "hello").await;
    agent.switch_model(model(second.clone(), "mock/b", None, None));
    run(&mut agent, "again").await;
    assert_eq!(second.requests()[0].system, first.requests()[0].system);
    assert_eq!(tool_names(&second, 0), tool_names(&first, 0));
}

// The tool a model no longer has is unknown to it, and the new one is valid.
#[tokio::test]
async fn after_a_switch_the_old_edit_tool_is_unknown_and_the_new_one_runs() {
    let dir = tempfile::tempdir().unwrap();
    let first = MockProvider::new(vec![Script::text("one")]);
    let second = MockProvider::new(vec![
        Script::tool_call("t1", "edit", json!({})),
        Script::tool_call("t2", "apply_patch", json!({"input": "p"})),
        Script::text("done"),
    ]);
    let mut agent = agent(first, dir.path());
    run(&mut agent, "hello").await;
    agent.switch_model(model(
        second,
        "mock/b",
        Some(registry(&["read", "apply_patch", "bash"])),
        Some(SECTION_B),
    ));
    let (_, events) = run(&mut agent, "again").await;
    let outputs = common::finished_outputs(&events);
    assert!(
        outputs[0].1 && outputs[0].0.contains("unknown tool `edit`"),
        "{outputs:?}"
    );
    assert_eq!(outputs[1], ("apply_patch ran".to_string(), false));
    assert_eq!(agent.tool_specs().len(), 3);
}

// Spec "Switching model": the tool section also reaches the context report.
#[tokio::test]
async fn the_context_report_counts_the_new_tools() {
    let dir = tempfile::tempdir().unwrap();
    let mut agent = agent(MockProvider::new(vec![]), dir.path());
    let before = agent.context_usage().tools;
    agent.switch_model(model(
        MockProvider::new(vec![]),
        "mock/b",
        Some(registry(&[
            "read",
            "a_tool_with_a_much_longer_name_and_nothing_else",
            "bash",
            "grep",
            "glob",
        ])),
        None,
    ));
    assert!(agent.context_usage().tools > before);
}

// A per-turn model (a command's `model:`, a role) brings its own edit format for that turn: its
// tool set and the prompt's edit section. The session's come back when the turn ends.
#[tokio::test]
async fn a_turn_model_uses_its_own_edit_format_and_the_sessions_comes_back() {
    use harness_core::turn::{TurnInput, TurnModel};
    let dir = tempfile::tempdir().unwrap();
    let session = MockProvider::new(vec![Script::text("one"), Script::text("three")]);
    let other = MockProvider::new(vec![
        Script::tool_call("t1", "apply_patch", json!({"input": "p"})),
        Script::text("two"),
    ]);
    let mut agent = agent(session.clone(), dir.path());
    let input = TurnInput {
        model: Some(TurnModel {
            provider: other.clone(),
            id: "mock/b".into(),
            name: "b".into(),
            local: false,
            tools: Some(registry(&["read", "apply_patch", "bash"])),
            edit_section: Some(SECTION_B.into()),
        }),
        ..TurnInput::from("patch it")
    };
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    agent
        .run_turn(input, &tx, tokio_util::sync::CancellationToken::new())
        .await;
    let mut events = Vec::new();
    while let Ok(e) = rx.try_recv() {
        events.push(e);
    }
    // Both requests of the turn offered the turn model's tools and prompt section.
    for request in other.requests() {
        let names: Vec<_> = request.tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["read", "apply_patch", "bash"]);
        assert!(request.system.contains(SECTION_B) && !request.system.contains(SECTION_A));
    }
    // Its edit tool ran, though the session's model has none of that name.
    assert_eq!(
        common::finished_outputs(&events)[0],
        ("apply_patch ran".to_string(), false)
    );
    // The next turn is the session model's again, byte for byte as before.
    run(&mut agent, "again").await;
    assert_eq!(tool_names(&session, 0), ["read", "edit", "bash"]);
    assert!(session.requests()[0].system.contains(SECTION_A));
    assert!(!session.requests()[0].system.contains(SECTION_B));
    assert_eq!(agent.tool_specs().len(), 3);
    assert_eq!(tool_names(&session, 0), ["read", "edit", "bash"]);
}
