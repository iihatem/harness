//! Test doubles for the gate tests: an edit tool that reports the file it changed, and a `bash`
//! that returns what a test scripted for each command and records what it was asked to run.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use harness_core::{
    agent::{Agent, AgentConfig, Approver, NonInteractive},
    engine::{EngineConfig, PermissionEngine, RuleSet},
    gate::Gates,
    message::ToolSpec,
    permission::{Action, Mode},
    testing::MockProvider,
    tool::{Tool, ToolContext, ToolOutput, ToolRegistry},
};
use serde_json::{Value, json};

/// Writes `content` to `path`, and says it changed that file.
pub struct Edit;

#[async_trait]
impl Tool for Edit {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit".into(),
            description: "edits a file".into(),
            parameters: json!({"type": "object", "properties": {"path": {"type": "string"}, "content": {"type": "string"}}, "required": ["path", "content"]}),
        }
    }
    fn action(&self, args: &Value, ctx: &ToolContext) -> Action {
        Action::Write(ctx.resolve(args["path"].as_str().unwrap_or_default()))
    }
    fn changed_paths(&self, args: &Value, ctx: &ToolContext) -> Vec<PathBuf> {
        vec![ctx.resolve(args["path"].as_str().unwrap_or_default())]
    }
    async fn run(&self, args: Value, ctx: &ToolContext) -> ToolOutput {
        let path = ctx.resolve(args["path"].as_str().unwrap_or_default());
        if args["content"] == "FAIL" {
            return ToolOutput::error("edit failed");
        }
        std::fs::write(&path, args["content"].as_str().unwrap_or_default()).unwrap();
        ToolOutput::ok("edited")
    }
}

/// Does nothing but read, and says it changed a file, as a tool run in a mode that cannot write
/// would not: for what gates do when the mode forbids them.
pub struct Sneaky;

#[async_trait]
impl Tool for Sneaky {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "sneaky".into(),
            description: "claims a change".into(),
            parameters: json!({"type": "object"}),
        }
    }
    fn action(&self, _args: &Value, ctx: &ToolContext) -> Action {
        Action::Read(ctx.workspace.clone())
    }
    fn changed_paths(&self, _args: &Value, ctx: &ToolContext) -> Vec<PathBuf> {
        vec![ctx.workspace.join("x.txt")]
    }
    async fn run(&self, _args: Value, _ctx: &ToolContext) -> ToolOutput {
        ToolOutput::ok("done")
    }
}

/// What the commands run through it did, in order.
#[derive(Default)]
pub struct Ran {
    pub commands: Mutex<Vec<String>>,
    pub timeouts: Mutex<Vec<Option<u64>>>,
}

impl Ran {
    pub fn commands(&self) -> Vec<String> {
        self.commands.lock().unwrap().clone()
    }
}

/// A `bash` that answers each command from a script: the outputs for a command, one per run, the
/// last one repeating.
pub struct ScriptedBash {
    pub ran: Arc<Ran>,
    outputs: Mutex<HashMap<String, Vec<ToolOutput>>>,
    /// Called as a command starts, for what the user does while it runs.
    hooks: HashMap<String, Box<dyn Fn() + Send + Sync>>,
}

impl ScriptedBash {
    pub fn new(outputs: Vec<(&str, Vec<ToolOutput>)>) -> (ScriptedBash, Arc<Ran>) {
        let ran = Arc::new(Ran::default());
        (
            ScriptedBash {
                ran: ran.clone(),
                outputs: Mutex::new(
                    outputs
                        .into_iter()
                        .map(|(command, outputs)| (command.to_string(), outputs))
                        .collect(),
                ),
                hooks: HashMap::new(),
            },
            ran,
        )
    }

    /// Calls `hook` each time `command` starts.
    pub fn during(mut self, command: &str, hook: impl Fn() + Send + Sync + 'static) -> Self {
        self.hooks.insert(command.to_string(), Box::new(hook));
        self
    }
}

#[async_trait]
impl Tool for ScriptedBash {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bash".into(),
            description: "runs a command".into(),
            parameters: json!({"type": "object", "properties": {"command": {"type": "string"}, "timeout_secs": {"type": "integer"}}, "required": ["command"]}),
        }
    }
    fn action(&self, args: &Value, _ctx: &ToolContext) -> Action {
        Action::Bash(args["command"].as_str().unwrap_or_default().to_string())
    }
    async fn run(&self, args: Value, _ctx: &ToolContext) -> ToolOutput {
        let command = args["command"].as_str().unwrap_or_default().to_string();
        self.ran.commands.lock().unwrap().push(command.clone());
        self.ran
            .timeouts
            .lock()
            .unwrap()
            .push(args["timeout_secs"].as_u64());
        if let Some(hook) = self.hooks.get(&command) {
            hook();
        }
        let mut outputs = self.outputs.lock().unwrap();
        match outputs.get_mut(&command) {
            Some(list) if list.len() > 1 => list.remove(0),
            Some(list) => list[0].clone(),
            None => ToolOutput::ok("exit code 0\n"),
        }
    }
}

pub fn failing(code: i32, output: &str) -> ToolOutput {
    ToolOutput::error(format!("exit code {code}\n{output}"))
}

pub fn passing() -> ToolOutput {
    ToolOutput::ok("exit code 0\nall good\n")
}

pub struct Setup {
    pub mode: Mode,
    pub rules: RuleSet,
    pub approver: Arc<dyn Approver>,
    pub gates: Gates,
}

impl Default for Setup {
    fn default() -> Self {
        Setup {
            mode: Mode::Auto,
            rules: RuleSet::default(),
            approver: Arc::new(NonInteractive),
            gates: Gates::default(),
        }
    }
}

/// An agent with `edit` and a scripted `bash`, with the policy of a mode that has a sandbox.
pub fn agent(
    provider: Arc<MockProvider>,
    dir: &std::path::Path,
    bash: ScriptedBash,
    setup: Setup,
) -> Agent {
    let policy = Arc::new(PermissionEngine::new(EngineConfig {
        mode: setup.mode,
        workspace: dir.to_path_buf(),
        read_dirs: vec![],
        rules: setup.rules,
        sandbox_available: true,
        writes_need_approval: false,
    }));
    Agent::new(
        provider,
        ToolRegistry::new(vec![Arc::new(Edit), Arc::new(Sneaky), Arc::new(bash)]),
        policy,
        setup.approver,
        AgentConfig::new("mock/m1", "m1", "system prompt", dir.join(".spill")),
        ToolContext::new(dir).with_sandbox(None, setup.mode.fs_access()),
    )
    .with_gates(setup.gates)
}
