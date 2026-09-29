//! What approvals ask, and what an approval for the session does.

mod common;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use common::{agent_with_sandbox, run};
use harness_core::{
    agent::{ApprovalDecision, ApprovalKind, ApprovalRequest, Approver},
    event::AgentEvent,
    permission::Mode,
    testing::{MockProvider, Script},
};
use serde_json::json;

/// Answers with `decision`, and records each request's kind and reason.
struct Answer {
    decision: ApprovalDecision,
    asked: Mutex<Vec<(ApprovalKind, String)>>,
}

impl Answer {
    fn new(decision: ApprovalDecision) -> Arc<Answer> {
        Arc::new(Answer {
            decision,
            asked: Mutex::new(Vec::new()),
        })
    }

    fn asked(&self) -> Vec<(ApprovalKind, String)> {
        self.asked.lock().unwrap().clone()
    }
}

#[async_trait]
impl Approver for Answer {
    async fn decide(&self, request: &ApprovalRequest) -> ApprovalDecision {
        self.asked
            .lock()
            .unwrap()
            .push((request.kind, request.reason.clone()));
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
