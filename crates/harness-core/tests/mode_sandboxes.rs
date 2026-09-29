//! Switching modes in a session whose sandbox depends on the mode: plan and read-only get a
//! read-only sandbox, ask and auto a workspace-write one (or none), full-access none.

mod common;

use std::{path::Path, sync::Arc};

use async_trait::async_trait;
use common::run;
use harness_core::{
    agent::{Agent, AgentConfig, NonInteractive, Sandboxes},
    engine::{EngineConfig, PermissionEngine},
    event::AgentEvent,
    message::{Message, ToolSpec},
    permission::{Action, Decision, FsAccess, Mode, PermissionPolicy},
    testing::{MockProvider, Script},
    tool::{CommandSandbox, Tool, ToolContext, ToolOutput, ToolRegistry},
};
use serde_json::{Value, json};

#[derive(Debug)]
struct Named(&'static str);

impl CommandSandbox for Named {
    fn name(&self) -> &'static str {
        self.0
    }
    fn command(
        &self,
        _access: FsAccess,
        _workspace: &Path,
        program: &str,
        _args: &[&str],
    ) -> std::io::Result<tokio::process::Command> {
        Ok(tokio::process::Command::new(program))
    }
    fn is_denial(&self, _exit_code: Option<i32>, _output: &str) -> bool {
        false
    }
}

/// A shell-like tool that reports the sandbox and access it would run with.
struct Probe;

#[async_trait]
impl Tool for Probe {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bash".into(),
            description: "probe".into(),
            parameters: json!({"type": "object", "properties": {"command": {"type": "string"}}}),
        }
    }
    fn action(&self, args: &Value, _ctx: &ToolContext) -> Action {
        Action::Bash(args["command"].as_str().unwrap_or("ls").to_string())
    }
    async fn run(&self, _args: Value, ctx: &ToolContext) -> ToolOutput {
        ToolOutput::ok(format!(
            "sandbox={} access={:?}",
            ctx.sandbox.as_ref().map_or("none", |s| s.name()),
            ctx.access
        ))
    }
}

fn agent(provider: Arc<MockProvider>, dir: &Path, mode: Mode, sandboxes: Sandboxes) -> Agent {
    let start = sandboxes.for_mode(mode);
    let policy = Arc::new(PermissionEngine::new(EngineConfig {
        mode,
        workspace: dir.to_path_buf(),
        read_dirs: vec![],
        rules: Default::default(),
        sandbox_available: start.is_some(),
        writes_need_approval: false,
    }));
    Agent::new(
        provider,
        ToolRegistry::new(vec![Arc::new(Probe)]),
        policy,
        Arc::new(NonInteractive),
        AgentConfig::new("mock/m1", "m1", "system prompt", dir.join(".spill")),
        ToolContext::new(dir).with_sandbox(start, mode.fs_access()),
    )
    .with_sandboxes(sandboxes)
}

fn both() -> Sandboxes {
    Sandboxes {
        read_only: Some(Arc::new(Named("read-only box"))),
        workspace_write: Some(Arc::new(Named("write box"))),
    }
}

/// Runs `ls` through the probe and returns what it reported, or why it did not run.
async fn probe(agent: &mut Agent, provider_turn: &str) -> String {
    let (_, events) = run(agent, provider_turn).await;
    events
        .iter()
        .find_map(|e| match e {
            AgentEvent::ToolCallFinished { output, .. } => Some(output.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

fn script(turns: usize) -> Arc<MockProvider> {
    let mut script = Vec::new();
    for i in 0..turns {
        script.push(Script::tool_call(
            &format!("c{i}"),
            "bash",
            json!({"command": "ls"}),
        ));
        script.push(Script::text("ok"));
    }
    MockProvider::new(script)
}

#[tokio::test]
async fn each_mode_gets_its_own_sandbox() {
    let dir = tempfile::tempdir().unwrap();
    let mut agent = agent(script(4), dir.path(), Mode::Auto, both());
    assert_eq!(
        probe(&mut agent, "1").await,
        "sandbox=write box access=WorkspaceWrite"
    );
    agent.set_mode(Mode::Plan);
    assert_eq!(
        probe(&mut agent, "2").await,
        "sandbox=read-only box access=ReadOnly"
    );
    agent.set_mode(Mode::Ask);
    // `ls` is unlisted, so ask mode asks, and nobody answers here.
    assert!(
        probe(&mut agent, "3")
            .await
            .contains("no user is available")
    );
    agent.set_mode(Mode::FullAccess);
    assert_eq!(
        probe(&mut agent, "4").await,
        "sandbox=none access=WorkspaceWrite"
    );
}

#[tokio::test]
async fn leaving_full_access_brings_the_sandbox_back() {
    let dir = tempfile::tempdir().unwrap();
    let mut agent = agent(script(2), dir.path(), Mode::FullAccess, both());
    assert_eq!(
        probe(&mut agent, "1").await,
        "sandbox=none access=WorkspaceWrite"
    );
    agent.set_mode(Mode::Auto);
    assert_eq!(
        probe(&mut agent, "2").await,
        "sandbox=write box access=WorkspaceWrite"
    );
}

#[tokio::test]
async fn a_mode_without_a_sandbox_asks_for_every_command_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    // A workspace too broad to make writable: only the read-only sandbox exists.
    let sandboxes = Sandboxes {
        read_only: Some(Arc::new(Named("read-only box"))),
        workspace_write: None,
    };
    let provider = script(2);
    let mut agent = agent(provider.clone(), dir.path(), Mode::Plan, sandboxes);
    assert_eq!(
        probe(&mut agent, "1").await,
        "sandbox=read-only box access=ReadOnly"
    );
    agent.set_mode(Mode::Auto);
    assert!(
        probe(&mut agent, "2")
            .await
            .contains("no user is available")
    );
    let note = provider
        .requests()
        .last()
        .unwrap()
        .messages
        .iter()
        .find_map(|m| match m {
            Message::User { content } if content.contains("approval mode is now auto") => {
                Some(content.clone())
            }
            _ => None,
        })
        .unwrap();
    assert!(note.contains("no OS sandbox is active"), "{note}");
}

#[test]
fn the_engine_follows_the_sandbox_it_is_told_about() {
    let dir = tempfile::tempdir().unwrap();
    let engine = PermissionEngine::new(EngineConfig {
        mode: Mode::Auto,
        workspace: dir.path().to_path_buf(),
        read_dirs: vec![],
        rules: Default::default(),
        sandbox_available: true,
        writes_need_approval: false,
    });
    let ls = Action::Bash("ls".into());
    assert_eq!(engine.check(&ls), Decision::Allow);
    engine.set_sandbox_available(false);
    assert!(matches!(engine.check(&ls), Decision::Ask(_)));
    engine.set_mode(Mode::Plan);
    assert!(matches!(engine.check(&ls), Decision::Deny(_)));
    engine.set_sandbox_available(true);
    assert_eq!(engine.check(&ls), Decision::Allow);
}
