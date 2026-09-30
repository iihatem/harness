mod common;

use std::sync::Arc;

use common::*;
use harness_core::agent::NonInteractive;
use harness_core::engine::RuleSet;
use harness_core::event::{AgentEvent, TurnEndReason};
use harness_core::message::Message;
use harness_core::permission::Mode;
use harness_core::testing::{MockProvider, Script};
use harness_core::turn::{InputPart, TurnInput, TurnModel};
use serde_json::json;

fn input(parts: Vec<InputPart>) -> TurnInput {
    TurnInput {
        parts,
        ..TurnInput::default()
    }
}

fn last_user_message(provider: &MockProvider, request: usize) -> String {
    provider.requests()[request]
        .messages
        .iter()
        .rev()
        .find_map(|m| match m {
            Message::User { content } => Some(content.clone()),
            _ => None,
        })
        .unwrap()
}

async fn run_input(
    agent: &mut harness_core::agent::Agent,
    input: TurnInput,
) -> (TurnEndReason, Vec<AgentEvent>) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let reason = agent
        .run_turn(input, &tx, tokio_util::sync::CancellationToken::new())
        .await;
    drop(tx);
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }
    (reason, events)
}

#[tokio::test]
async fn shell_parts_are_approved_and_run_before_the_message_is_sent() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![Script::text("looks fine")]);
    let approver = Arc::new(Recorder::default());
    let mut agent = agent_with_sandbox(provider.clone(), Mode::Ask, approver.clone(), dir.path());
    let parts = vec![
        InputPart::Text("Review:\n".into()),
        InputPart::Shell("git diff".into()),
        InputPart::Text("\nThanks".into()),
    ];
    let (reason, events) = run_input(&mut agent, input(parts)).await;
    assert_eq!(reason, TurnEndReason::Completed);
    assert_eq!(*approver.asked.lock().unwrap(), ["run `git diff`"]);
    let approval = events
        .iter()
        .position(|e| matches!(e, AgentEvent::ApprovalNeeded { .. }))
        .unwrap();
    let answer = events
        .iter()
        .position(|e| matches!(e, AgentEvent::AssistantMessage { .. }))
        .unwrap();
    assert!(approval < answer, "{events:?}");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolCallRequested { name, .. } if name == "bash"))
    );
    assert_eq!(
        last_user_message(&provider, 0),
        "Review:\nran `git diff` with WorkspaceWrite access\nThanks"
    );
}

#[tokio::test]
async fn a_refused_shell_part_is_noted_in_the_message() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![Script::text("I could not see the diff")]);
    let mut agent = agent_with_sandbox(
        provider.clone(),
        Mode::Ask,
        Arc::new(NonInteractive),
        dir.path(),
    );
    let (reason, events) =
        run_input(&mut agent, input(vec![InputPart::Shell("git diff".into())])).await;
    assert_eq!(reason, TurnEndReason::Completed);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::ActionBlocked { .. }))
    );
    let message = last_user_message(&provider, 0);
    assert!(
        message.starts_with("[`git diff` did not run successfully]\nblocked:"),
        "{message}"
    );
}

#[tokio::test]
async fn turn_rules_apply_to_their_turn_only() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call("c1", "bash", json!({"command": "openspec status"})),
        Script::text("done"),
        Script::tool_call("c2", "bash", json!({"command": "openspec status"})),
        Script::text("done"),
    ]);
    let mut agent = agent_with_sandbox(provider, Mode::Ask, Arc::new(NonInteractive), dir.path());
    let first = TurnInput {
        rules: RuleSet {
            allow: vec!["bash:openspec".into(), "bash:openspec *".into()],
            ..RuleSet::default()
        },
        ..TurnInput::from("propose")
    };
    let (_, events) = run_input(&mut agent, first).await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::ActionBlocked { .. })),
        "{events:?}"
    );
    assert!(
        finished_outputs(&events)[0]
            .0
            .contains("ran `openspec status`")
    );
    let (_, events) = run(&mut agent, "again").await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::ActionBlocked { .. })),
        "{events:?}"
    );
}

#[tokio::test]
async fn a_turn_model_answers_only_its_turn() {
    let dir = tempfile::tempdir().unwrap();
    let session_model = MockProvider::new(vec![Script::text("from m1")]);
    let command_model = MockProvider::new(vec![Script::text("from m2")]);
    let mut agent = agent(
        session_model.clone(),
        Mode::Auto,
        Arc::new(NonInteractive),
        dir.path(),
    );
    let first = TurnInput {
        model: Some(TurnModel {
            provider: command_model.clone(),
            id: "mock/m2".into(),
            name: "m2".into(),
            local: false,
        }),
        ..TurnInput::from("one")
    };
    let (_, events) = run_input(&mut agent, first).await;
    assert!(events.contains(&AgentEvent::AssistantMessage {
        content: "from m2".into(),
        model: "mock/m2".into()
    }));
    assert_eq!(command_model.requests()[0].model, "m2");
    let (_, events) = run(&mut agent, "two").await;
    assert!(events.contains(&AgentEvent::AssistantMessage {
        content: "from m1".into(),
        model: "mock/m1".into()
    }));
    assert_eq!(session_model.requests().len(), 1);
}

#[tokio::test]
async fn a_read_only_shell_turn_runs_commands_read_only() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call("c1", "bash", json!({"command": "ls"})),
        Script::text("done"),
        Script::tool_call("c2", "bash", json!({"command": "ls"})),
        Script::text("done"),
    ]);
    let mut agent = agent_with_sandbox(provider, Mode::Auto, Arc::new(NonInteractive), dir.path());
    let first = TurnInput {
        read_only_shell: true,
        ..TurnInput::from("look around")
    };
    let (_, events) = run_input(&mut agent, first).await;
    assert!(finished_outputs(&events)[0].0.contains("ReadOnly access"));
    let (_, events) = run(&mut agent, "again").await;
    assert!(
        finished_outputs(&events)[0]
            .0
            .contains("WorkspaceWrite access")
    );
}

/// Approves, and cancels `token` as it does: the user pressed Ctrl+C at the prompt.
struct ApproveThenInterrupt {
    token: tokio_util::sync::CancellationToken,
    asked: std::sync::Mutex<Vec<String>>,
}
#[async_trait::async_trait]
impl harness_core::agent::Approver for ApproveThenInterrupt {
    async fn decide(
        &self,
        request: &harness_core::agent::ApprovalRequest,
    ) -> harness_core::agent::ApprovalDecision {
        self.asked.lock().unwrap().push(request.reason.clone());
        self.token.cancel();
        harness_core::agent::ApprovalDecision::Approve
    }
}

// Review C, minor 5: after an interrupt, later shell parts are neither run nor asked about.
#[tokio::test]
async fn shell_parts_after_an_interrupt_are_not_run() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![Script::text("never")]);
    let token = tokio_util::sync::CancellationToken::new();
    let approver = Arc::new(ApproveThenInterrupt {
        token: token.clone(),
        asked: Default::default(),
    });
    let mut agent = agent_with_sandbox(provider.clone(), Mode::Ask, approver.clone(), dir.path());
    let parts = vec![
        InputPart::Shell("git diff".into()),
        InputPart::Text("\n".into()),
        InputPart::Shell("git status".into()),
    ];
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let reason = agent.run_turn(input(parts), &tx, token).await;
    drop(tx);
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }
    assert_eq!(reason, TurnEndReason::Interrupted);
    assert_eq!(*approver.asked.lock().unwrap(), ["run `git diff`"]);
    let requested = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolCallRequested { .. }))
        .count();
    assert_eq!(requested, 1, "{events:?}");
    assert!(provider.requests().is_empty());
    match agent.history().last() {
        Some(Message::User { content }) => assert_eq!(
            content,
            "ran `git diff` with WorkspaceWrite access\n[`git status` not run: interrupted]"
        ),
        other => panic!("{other:?}"),
    }
}
