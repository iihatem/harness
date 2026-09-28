//! A slash command's shell expansion runs through the same path as the model's `bash` calls: the
//! permission check, the sandbox, and the sandbox's guard.

use std::{
    path::Path,
    sync::{Arc, Mutex},
};

use harness_core::{
    agent::{Agent, AgentConfig, NonInteractive},
    engine::{EngineConfig, PermissionEngine, RuleSet},
    event::AgentEvent,
    message::Message,
    permission::{FsAccess, Mode},
    testing::{MockProvider, Script},
    tool::{
        CommandGuard, CommandSandbox, GuardReport, SandboxedCommand, ToolContext, ToolRegistry,
    },
    turn::{InputPart, TurnInput},
};
use harness_tools::BashTool;
use tokio_util::sync::CancellationToken;

/// Runs commands directly and records each script it prepares, and each pid its guards are told
/// of. Its guard always reports that it undid a change.
#[derive(Debug, Default)]
struct UndoingSandbox {
    prepared: Mutex<Vec<String>>,
    started: Arc<Mutex<Vec<u32>>>,
}

struct UndoingGuard {
    started: Arc<Mutex<Vec<u32>>>,
}

impl CommandGuard for UndoingGuard {
    fn started(&mut self, pid: u32) {
        self.started.lock().unwrap().push(pid);
    }

    fn finish(self: Box<Self>) -> Option<GuardReport> {
        Some(GuardReport {
            message: "[the sandbox undid changes: .git/hooks/pre-commit]".into(),
            blocked: true,
        })
    }
}

impl CommandSandbox for UndoingSandbox {
    fn name(&self) -> &'static str {
        "undoing"
    }

    fn command(
        &self,
        _access: FsAccess,
        _workspace: &Path,
        program: &str,
        args: &[&str],
    ) -> std::io::Result<tokio::process::Command> {
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(args).process_group(0);
        Ok(cmd)
    }

    fn is_denial(&self, _exit_code: Option<i32>, _output: &str) -> bool {
        false
    }

    fn prepare(
        &self,
        access: FsAccess,
        workspace: &Path,
        program: &str,
        args: &[&str],
    ) -> std::io::Result<SandboxedCommand> {
        let script = args.last().copied().unwrap_or_default();
        self.prepared.lock().unwrap().push(script.to_string());
        Ok(SandboxedCommand {
            command: self.command(access, workspace, program, args)?,
            guard: Some(Box::new(UndoingGuard {
                started: self.started.clone(),
            })),
        })
    }
}

/// Runs one turn made of `parts` in `auto` mode with `deny` rules, through the real `bash` tool.
async fn run(
    parts: Vec<InputPart>,
    deny: &[&str],
) -> (Arc<UndoingSandbox>, Vec<AgentEvent>, String) {
    let dir = tempfile::tempdir().unwrap();
    let sandbox = Arc::new(UndoingSandbox::default());
    let ctx =
        ToolContext::new(dir.path()).with_sandbox(Some(sandbox.clone()), FsAccess::WorkspaceWrite);
    let policy = Arc::new(PermissionEngine::new(EngineConfig {
        mode: Mode::Auto,
        workspace: dir.path().to_path_buf(),
        read_dirs: vec![],
        rules: RuleSet {
            deny: deny.iter().map(|d| d.to_string()).collect(),
            ..RuleSet::default()
        },
        sandbox_available: true,
        writes_need_approval: false,
    }));
    let provider = MockProvider::new(vec![Script::text("ok")]);
    let config = AgentConfig::new("mock/m", "m", "system", dir.path().join(".spill"));
    let mut agent = Agent::new(
        provider.clone(),
        ToolRegistry::new(vec![Arc::new(BashTool)]),
        policy,
        Arc::new(NonInteractive),
        config,
        ctx,
    );
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let input = TurnInput {
        parts,
        ..TurnInput::default()
    };
    agent.run_turn(input, &tx, CancellationToken::new()).await;
    drop(tx);
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }
    let Message::User { content } = &provider.requests()[0].messages[0] else {
        panic!("the first message is the user's");
    };
    (sandbox, events, content.clone())
}

#[tokio::test]
async fn a_shell_part_runs_in_the_sandbox_and_its_guard_can_block_it() {
    let parts = vec![
        InputPart::Text("Hooks: ".into()),
        InputPart::Shell("echo planted".into()),
    ];
    let (sandbox, events, message) = run(parts, &[]).await;
    assert_eq!(
        *sandbox.prepared.lock().unwrap(),
        ["exec 2>&1\necho planted"]
    );
    // The guard was told which process the shell part runs in, as for the model's own commands.
    let started = sandbox.started.lock().unwrap().clone();
    assert_eq!(started.len(), 1, "{started:?}");
    assert!(started[0] > 0);
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::ActionBlocked { reason, .. } if reason.contains("git-metadata guard")
        )),
        "{events:?}"
    );
    assert!(
        message.starts_with("Hooks: [`echo planted` did not run successfully]"),
        "{message}"
    );
    assert!(message.contains("[the sandbox undid changes: .git/hooks/pre-commit]"));
}

#[tokio::test]
async fn a_shell_part_a_deny_rule_matches_never_runs() {
    let (sandbox, _events, message) = run(
        vec![InputPart::Shell("rm -rf build".into())],
        &["bash:rm *"],
    )
    .await;
    assert!(sandbox.prepared.lock().unwrap().is_empty());
    assert!(sandbox.started.lock().unwrap().is_empty());
    assert!(message.contains("denied"), "{message}");
}
