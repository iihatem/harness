//! A child's effective mode is the narrower of the mode it started with and its parent's live
//! one, and it never widens beyond its start; every command it runs gets the sandbox and the
//! filesystem access of that mode at that moment.

use std::{path::Path, sync::Arc};

use async_trait::async_trait;
use harness_core::{
    agent::{Agent, AgentConfig, NonInteractive, Sandboxes},
    agents::{
        ChildWorkspace,
        perm::{ChildPolicy, child_engine_config},
    },
    engine::{EngineConfig, PermissionEngine, RuleSet},
    event::AgentEvent,
    message::ToolSpec,
    permission::{Action, FsAccess, Mode, PermissionPolicy},
    testing::{MockProvider, Script},
    tool::{CommandSandbox, Tool, ToolContext, ToolOutput, ToolRegistry},
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

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

fn both() -> Sandboxes {
    Sandboxes {
        read_only: Some(Arc::new(Named("read-only box"))),
        workspace_write: Some(Arc::new(Named("write box"))),
    }
}

fn config(mode: Mode, dir: &Path) -> EngineConfig {
    EngineConfig {
        mode,
        workspace: dir.to_path_buf(),
        read_dirs: vec![],
        rules: RuleSet::default(),
        sandbox_available: true,
        writes_need_approval: false,
    }
}

/// A child started while its parent was in `parent_start`, asking for `own`, as the supervisor
/// builds it; the parent's policy is returned to switch.
fn child(parent_start: Mode, own: Option<Mode>, dir: &Path) -> (Arc<PermissionEngine>, Agent) {
    let parent = Arc::new(PermissionEngine::new(config(parent_start, dir)));
    let engine = child_engine_config(
        &config(parent_start, dir),
        parent_start,
        own,
        &RuleSet::default(),
        &ChildWorkspace::parent(),
    );
    let mode = engine.mode;
    let policy = Arc::new(ChildPolicy::new(
        PermissionEngine::new(engine),
        parent.clone(),
        Some(mode),
    ));
    let sandboxes = both();
    let agent = Agent::new(
        MockProvider::new(vec![
            Script::tool_call("c1", "bash", json!({"command": "sed -i s/a/b/ notes.txt"})),
            Script::text("ok"),
        ]),
        ToolRegistry::new(vec![Arc::new(Probe)]),
        policy,
        Arc::new(NonInteractive),
        AgentConfig::new("mock/m1", "m1", "system prompt", dir.join(".spill")),
        ToolContext::new(dir).with_sandbox(sandboxes.for_mode(mode), mode.fs_access()),
    )
    .with_sandboxes(sandboxes);
    (parent, agent)
}

async fn first_output(agent: &mut Agent) -> String {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    agent
        .run_turn("go".to_string(), &tx, CancellationToken::new())
        .await;
    drop(tx);
    while let Some(event) = rx.recv().await {
        if let AgentEvent::ToolCallFinished { output, .. } = event {
            return output;
        }
    }
    String::new()
}

#[tokio::test]
async fn a_child_whose_parent_switches_to_plan_runs_its_next_command_read_only_sandboxed() {
    let dir = tempfile::tempdir().unwrap();
    let (parent, mut agent) = child(Mode::Auto, None, dir.path());
    parent.set_mode(Mode::Plan);
    let out = first_output(&mut agent).await;
    assert_eq!(out, "sandbox=read-only box access=ReadOnly");
}

#[tokio::test]
async fn a_child_started_in_plan_stays_read_only_when_the_parent_widens() {
    let dir = tempfile::tempdir().unwrap();
    let (parent, mut agent) = child(Mode::Plan, None, dir.path());
    parent.set_mode(Mode::Auto);
    let out = first_output(&mut agent).await;
    assert_eq!(out, "sandbox=read-only box access=ReadOnly");
}

#[tokio::test]
async fn a_child_returns_to_its_own_mode_when_the_parent_goes_back_up() {
    let dir = tempfile::tempdir().unwrap();
    let (parent, mut agent) = child(Mode::Auto, None, dir.path());
    parent.set_mode(Mode::Plan);
    parent.set_mode(Mode::Auto);
    let out = first_output(&mut agent).await;
    assert_eq!(out, "sandbox=write box access=WorkspaceWrite");
}
