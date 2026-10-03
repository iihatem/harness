//! What approvals ask, and what an approval for the session does.

mod common;

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use common::{agent, agent_with_sandbox, run};
use harness_core::{
    agent::{Agent, AgentConfig, ApprovalDecision, ApprovalKind, ApprovalRequest, Approver},
    engine::{EngineConfig, PermissionEngine, RuleSet},
    event::{AgentEvent, TurnEndReason},
    message::ToolSpec,
    permission::{Action, Mode},
    testing::{MockProvider, Script},
    tool::{Tool, ToolContext, ToolOutput, ToolRegistry},
};
use serde_json::{Value, json};
use tokio::sync::{Notify, mpsc};
use tokio_util::sync::CancellationToken;

/// Answers with `decision`, and records each request.
struct Answer {
    decision: ApprovalDecision,
    asked: Mutex<Vec<ApprovalRequest>>,
}

impl Answer {
    fn new(decision: ApprovalDecision) -> Arc<Answer> {
        Arc::new(Answer {
            decision,
            asked: Mutex::new(Vec::new()),
        })
    }

    fn asked(&self) -> Vec<(ApprovalKind, String)> {
        self.requests()
            .into_iter()
            .map(|r| (r.kind, r.reason))
            .collect()
    }

    fn requests(&self) -> Vec<ApprovalRequest> {
        self.asked.lock().unwrap().clone()
    }
}

#[async_trait]
impl Approver for Answer {
    async fn decide(&self, request: &ApprovalRequest) -> ApprovalDecision {
        self.asked.lock().unwrap().push(request.clone());
        self.decision.clone()
    }
}

#[tokio::test]
async fn an_action_is_asked_about_as_such_and_a_rerun_outside_the_sandbox_as_such() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call("c1", "touch", json!({"path": "a.txt"})),
        Script::tool_call("c2", "boxed", json!({})),
        Script::text("done"),
    ]);
    let answer = Answer::new(ApprovalDecision::Approve);
    let mut agent = agent_with_sandbox(provider, Mode::Ask, answer.clone(), dir.path());
    run(&mut agent, "go").await;
    let asked = answer.asked();
    assert_eq!(asked[0].0, ApprovalKind::Action);
    assert!(asked[0].1.contains("a.txt"), "{asked:?}");
    let rerun = asked.last().unwrap();
    assert_eq!(rerun.0, ApprovalKind::RunUnsandboxed);
    assert!(rerun.1.contains("without the sandbox"), "{asked:?}");
}

#[tokio::test]
async fn a_session_approval_that_cannot_be_kept_says_it_applied_once() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call("c1", "bash", json!({"command": "git reset --hard HEAD~1"})),
        Script::tool_call("c2", "bash", json!({"command": "git reset --hard HEAD~1"})),
        Script::text("done"),
    ]);
    let answer = Answer::new(ApprovalDecision::ApproveForSession);
    let mut agent = agent_with_sandbox(provider, Mode::Auto, answer.clone(), dir.path());
    let (_, events) = run(&mut agent, "go").await;
    // A destructive command is never approved for the session: both runs asked.
    assert_eq!(answer.asked().len(), 2);
    let warnings: Vec<&String> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Warning { message } => Some(message),
            _ => None,
        })
        .collect();
    assert_eq!(warnings.len(), 2, "{warnings:?}");
    assert!(
        warnings[0].starts_with("approved once: ")
            && warnings[0].ends_with("so harness will ask again next time"),
        "{warnings:?}"
    );
}

#[tokio::test]
async fn a_command_approved_for_the_session_is_not_asked_about_again() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call("c1", "bash", json!({"command": "cargo test"})),
        Script::tool_call("c2", "bash", json!({"command": "cargo test --all"})),
        Script::text("done"),
    ]);
    let answer = Answer::new(ApprovalDecision::ApproveForSession);
    let mut agent = agent_with_sandbox(provider, Mode::Ask, answer.clone(), dir.path());
    let (_, events) = run(&mut agent, "go").await;
    assert_eq!(answer.asked().len(), 1);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::Warning { .. })),
        "{events:#?}"
    );
}

// Review D M5: the prompt says "approved once" for what a session approval cannot keep, so the
// request says whether it would be kept.
#[tokio::test]
async fn a_request_says_whether_a_session_approval_would_be_kept() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call("c1", "bash", json!({"command": "git reset --hard HEAD~1"})),
        Script::tool_call("c2", "bash", json!({"command": "cargo test"})),
        Script::tool_call("c3", "boxed", json!({})),
        Script::text("done"),
    ]);
    let answer = Answer::new(ApprovalDecision::Approve);
    let mut agent = agent_with_sandbox(provider, Mode::Ask, answer.clone(), dir.path());
    run(&mut agent, "go").await;
    let kept: Vec<(String, bool)> = answer
        .requests()
        .into_iter()
        .map(|r| (r.call_id, r.kept_for_session))
        .collect();
    assert_eq!(
        kept,
        [
            ("c1".to_string(), false),
            ("c2".to_string(), true),
            ("c3".to_string(), true),
            // Running outside the sandbox is never approved for the session.
            ("c3".to_string(), false),
        ]
    );
}

/// Never answers; says when it is asked.
#[derive(Default)]
struct Silent {
    asked: Notify,
}

#[async_trait]
impl Approver for Silent {
    async fn decide(&self, _request: &ApprovalRequest) -> ApprovalDecision {
        self.asked.notify_one();
        std::future::pending().await
    }
}

// Review D I2: a turn stopped while its approval waits ends at once, whether or not anyone
// answers, and what it asked about does not run.
#[tokio::test]
async fn stopping_a_turn_while_its_approval_waits_ends_it_and_runs_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call("c1", "touch", json!({"path": "a.txt"})),
        Script::text("never asked"),
    ]);
    let silent = Arc::new(Silent::default());
    let mut agent = agent(provider.clone(), Mode::Ask, silent.clone(), dir.path());
    let cancel = CancellationToken::new();
    let (events, mut received) = mpsc::unbounded_channel();
    let turn = tokio::spawn({
        let cancel = cancel.clone();
        async move { agent.run_turn("go".to_string(), &events, cancel).await }
    });
    silent.asked.notified().await;
    cancel.cancel();
    let reason = tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("the turn ends once stopped")
        .unwrap();
    assert_eq!(reason, TurnEndReason::Interrupted);
    assert!(!dir.path().join("a.txt").exists());
    assert_eq!(provider.requests().len(), 1);
    let mut finished = None;
    while let Ok(event) = received.try_recv() {
        if let AgentEvent::ToolCallFinished { output, .. } = event {
            finished = Some(output);
        }
    }
    assert_eq!(
        finished.as_deref(),
        Some("interrupted by the user before this tool ran")
    );
}

/// Stops the turn while it runs, then fails as if the sandbox had blocked it.
struct StopsThenBlocked;

#[async_trait]
impl Tool for StopsThenBlocked {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "stops".into(),
            description: "stops".into(),
            parameters: json!({"type": "object"}),
        }
    }
    fn action(&self, _args: &Value, _ctx: &ToolContext) -> Action {
        Action::Bash("curl https://example.com".into())
    }
    async fn run(&self, _args: Value, ctx: &ToolContext) -> ToolOutput {
        ctx.cancel.cancel();
        let mut out = ToolOutput::error("exit code 6\ncurl: (6) Could not resolve host");
        out.sandbox_denied = true;
        out
    }
}

// Review D I2: once the turn is stopped, harness asks nothing more, such as whether to run the
// command again without the sandbox.
#[tokio::test]
async fn nothing_is_asked_once_the_turn_is_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call("c1", "stops", json!({})),
        Script::text("never asked"),
    ]);
    let policy = Arc::new(PermissionEngine::new(EngineConfig {
        mode: Mode::Auto,
        workspace: dir.path().to_path_buf(),
        read_dirs: vec![],
        rules: RuleSet::default(),
        sandbox_available: true,
        writes_need_approval: false,
    }));
    let answer = Answer::new(ApprovalDecision::Approve);
    let mut agent = Agent::new(
        provider,
        ToolRegistry::new(vec![Arc::new(StopsThenBlocked)]),
        policy,
        answer.clone(),
        AgentConfig::new("mock/m1", "m1", "system prompt", dir.path().join(".spill")),
        ToolContext::new(dir.path()),
    );
    let (reason, events) = run(&mut agent, "go").await;
    assert_eq!(reason, TurnEndReason::Interrupted);
    assert!(answer.asked().is_empty(), "{:?}", answer.asked());
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::ApprovalNeeded { .. })),
        "{events:#?}"
    );
}
