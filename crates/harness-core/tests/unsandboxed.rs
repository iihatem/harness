//! A sandbox that can no longer run a command (Linux, with git protection required, after the
//! session dropped to the basic tier): the command runs outside it only once approved, and a
//! headless run counts it as blocked.

mod common;

use std::{
    path::Path,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use common::run;
use harness_core::{
    agent::{
        Agent, AgentConfig, ApprovalDecision, ApprovalKind, ApprovalRequest, Approver,
        NonInteractive,
    },
    engine::{EngineConfig, PermissionEngine, RuleSet},
    event::AgentEvent,
    message::ToolSpec,
    permission::{Action, FsAccess, Mode},
    testing::{MockProvider, Script},
    tool::{CommandSandbox, Tool, ToolContext, ToolOutput, ToolRegistry},
};
use serde_json::{Value, json};

/// Refuses commands that may write, as the Linux sandbox does after such a drop.
#[derive(Debug)]
struct Dropped;

impl CommandSandbox for Dropped {
    fn name(&self) -> &'static str {
        "dropped"
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
    fn cannot_run(&self, access: FsAccess) -> Option<String> {
        (access == FsAccess::WorkspaceWrite).then(|| "the sandbox dropped to the basic tier".into())
    }
}

/// Reports whether it ran outside the sandbox.
struct Shell;

#[async_trait]
impl Tool for Shell {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bash".into(),
            description: "shell".into(),
            parameters: json!({"type": "object", "properties": {"command": {"type": "string"}}}),
        }
    }
    fn action(&self, args: &Value, _ctx: &ToolContext) -> Action {
        Action::Bash(args["command"].as_str().unwrap_or_default().to_string())
    }
    async fn run(&self, _args: Value, ctx: &ToolContext) -> ToolOutput {
        ToolOutput::ok(format!("ran, unsandboxed={}", ctx.unsandboxed))
    }
}

/// Answers every request with `decision`, and records what it was asked.
struct Answer(ApprovalDecision, Mutex<Vec<ApprovalRequest>>);

#[async_trait]
impl Approver for Answer {
    async fn decide(&self, request: &ApprovalRequest) -> ApprovalDecision {
        self.1.lock().unwrap().push(request.clone());
        self.0.clone()
    }
}

fn agent(mode: Mode, approver: Arc<dyn Approver>, deny: &[&str], dir: &Path) -> Agent {
    agent_for("ls", mode, approver, deny, &[], dir)
}

fn agent_for(
    command: &str,
    mode: Mode,
    approver: Arc<dyn Approver>,
    deny: &[&str],
    confirm: &[&str],
    dir: &Path,
) -> Agent {
    let provider = MockProvider::new(vec![
        Script::tool_call("c1", "bash", json!({ "command": command })),
        Script::text("done"),
    ]);
    let own = |rules: &[&str]| rules.iter().map(|r| r.to_string()).collect();
    let policy = Arc::new(PermissionEngine::new(EngineConfig {
        mode,
        workspace: dir.to_path_buf(),
        read_dirs: vec![],
        rules: RuleSet {
            deny: own(deny),
            confirm: own(confirm),
            ..RuleSet::default()
        },
        sandbox_available: true,
        writes_need_approval: false,
    }));
    Agent::new(
        provider,
        ToolRegistry::new(vec![Arc::new(Shell)]),
        policy,
        approver,
        AgentConfig::new("mock/m1", "m1", "system prompt", dir.join(".spill")),
        ToolContext::new(dir).with_sandbox(Some(Arc::new(Dropped)), mode.fs_access()),
    )
}

fn output(events: &[AgentEvent]) -> (String, bool) {
    events
        .iter()
        .find_map(|e| match e {
            AgentEvent::ToolCallFinished {
                output, is_error, ..
            } => Some((output.clone(), *is_error)),
            _ => None,
        })
        .unwrap()
}

#[tokio::test]
async fn headless_the_command_is_blocked_and_does_not_run() {
    let dir = tempfile::tempdir().unwrap();
    let mut agent = agent(Mode::Auto, Arc::new(NonInteractive), &[], dir.path());
    let (_, events) = run(&mut agent, "go").await;
    let (text, is_error) = output(&events);
    assert!(
        is_error && text.starts_with("blocked: the sandbox dropped"),
        "{text}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::ActionBlocked { .. })),
        "a headless run exits 3"
    );
}

#[tokio::test]
async fn approved_it_runs_outside_the_sandbox_once_asked_once() {
    let dir = tempfile::tempdir().unwrap();
    let answer = Arc::new(Answer(
        ApprovalDecision::ApproveForSession,
        Mutex::default(),
    ));
    // Ask mode would ask about `ls` anyway: still one question.
    let mut agent = agent(Mode::Ask, answer.clone(), &[], dir.path());
    let (_, events) = run(&mut agent, "go").await;
    assert_eq!(output(&events), ("ran, unsandboxed=true".into(), false));
    let asked = answer.1.lock().unwrap().clone();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].kind, ApprovalKind::RunUnsandboxed);
    assert_eq!(
        asked[0].reason,
        "run `ls`, and the sandbox dropped to the basic tier: run it without the sandbox?"
    );
}

// Review D M7: the question keeps why the command needed approval anyway, and a command cut
// short says so.
#[tokio::test]
async fn the_question_keeps_the_reason_to_ask_and_marks_a_cut_command() {
    let dir = tempfile::tempdir().unwrap();
    let answer = Arc::new(Answer(ApprovalDecision::Approve, Mutex::default()));
    let mut confirmed = agent_for(
        "make deploy",
        Mode::Auto,
        answer.clone(),
        &[],
        &["bash:make deploy*"],
        dir.path(),
    );
    run(&mut confirmed, "go").await;
    let long = format!("echo {}", "x".repeat(200));
    let mut cut = agent_for(&long, Mode::Auto, answer.clone(), &[], &[], dir.path());
    run(&mut cut, "go").await;
    let asked = answer.1.lock().unwrap().clone();
    assert_eq!(asked.len(), 2, "{asked:#?}");
    assert!(
        asked[0].reason.contains("make deploy")
            && asked[0].reason.contains("confirm")
            && asked[0]
                .reason
                .ends_with("the sandbox dropped to the basic tier: run it without the sandbox?"),
        "{}",
        asked[0].reason
    );
    assert!(
        asked[1].reason.contains("xxx…` without the sandbox?"),
        "{}",
        asked[1].reason
    );
}

#[tokio::test]
async fn declined_it_does_not_run() {
    let dir = tempfile::tempdir().unwrap();
    let answer = Arc::new(Answer(
        ApprovalDecision::Deny {
            feedback: Some("not now".into()),
        },
        Mutex::default(),
    ));
    let mut agent = agent(Mode::Auto, answer, &[], dir.path());
    let (_, events) = run(&mut agent, "go").await;
    assert_eq!(
        output(&events),
        (
            "the user declined to run it without the sandbox: not now".into(),
            true
        )
    );
}

#[tokio::test]
async fn read_only_commands_and_denied_ones_are_not_asked_about() {
    let dir = tempfile::tempdir().unwrap();
    let answer = Arc::new(Answer(ApprovalDecision::Approve, Mutex::default()));
    // Plan mode's read-only sandbox can still run it.
    let mut agent_ro = agent(Mode::Plan, answer.clone(), &[], dir.path());
    let (_, events) = run(&mut agent_ro, "go").await;
    assert_eq!(output(&events), ("ran, unsandboxed=false".into(), false));
    // A deny rule wins before anything is asked.
    let mut denied = agent(Mode::Auto, answer.clone(), &["bash:ls*"], dir.path());
    let (_, events) = run(&mut denied, "go").await;
    assert!(output(&events).0.starts_with("denied:"));
    assert!(answer.1.lock().unwrap().is_empty());
}
