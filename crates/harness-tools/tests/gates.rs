//! Gate commands through the real bash tool: its timeout, its exit codes and its output.

use std::sync::Arc;

use harness_core::{
    agent::{Agent, AgentConfig, NonInteractive},
    engine::{EngineConfig, PermissionEngine},
    event::{AgentEvent, GateKind, GateStatus},
    gate::Gates,
    permission::Mode,
    testing::{MockProvider, Script},
    tool::ToolContext,
};
use serde_json::json;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

fn agent(dir: &std::path::Path, provider: Arc<MockProvider>, gates: Gates) -> Agent {
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
        harness_tools::builtin(),
        policy,
        Arc::new(NonInteractive),
        AgentConfig::new("mock/m1", "m1", "system prompt", dir.join(".spill")),
        ToolContext::new(dir),
    )
    .with_gates(gates)
}

/// A turn that reads and edits `app.py`, and the tool results of it.
async fn edit_turn(gates: Gates) -> (Vec<String>, Vec<AgentEvent>) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("app.py"), "import os\nprint(1)\n").unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call("r1", "read", json!({"path": "app.py"})),
        Script::tool_call(
            "e1",
            "edit",
            json!({"path": "app.py", "old_string": "print(1)", "new_string": "print(2)"}),
        ),
        Script::text("done"),
    ]);
    let mut agent = agent(dir.path(), provider, gates);
    let (tx, mut rx) = mpsc::unbounded_channel();
    agent
        .run_turn("change it".to_string(), &tx, CancellationToken::new())
        .await;
    drop(tx);
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }
    let outputs = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolCallFinished { output, .. } => Some(output.clone()),
            _ => None,
        })
        .collect();
    (outputs, events)
}

// Spec "Lint failure after an edit", with the real bash tool: exit code 1 and its output.
#[tokio::test]
async fn a_lint_that_exits_1_is_appended_to_the_real_edit_result() {
    let (outputs, _) = edit_turn(Gates {
        after_edit: Some("echo 'app.py:3:1: F401 unused import'; exit 1".into()),
        ..Gates::default()
    })
    .await;
    // The results are the read's, then the edit's.
    let edit = &outputs[1];
    assert!(
        edit.contains("-print(1)") && edit.contains("+print(2)"),
        "{edit}"
    );
    assert!(edit.contains("exit code 1"), "{edit}");
    assert!(edit.contains("app.py:3:1: F401 unused import"), "{edit}");
}

#[tokio::test]
async fn a_lint_that_exits_0_adds_a_pass_line() {
    let (outputs, _) = edit_turn(Gates {
        after_edit: Some("echo fine".into()),
        ..Gates::default()
    })
    .await;
    assert!(
        outputs[1].ends_with("[after_edit gate passed: `echo fine`]"),
        "{}",
        outputs[1]
    );
}

// Spec "A gate times out": the command stops after `timeout_s`, and the model is told.
#[tokio::test]
async fn a_lint_that_runs_too_long_is_stopped_and_reported() {
    let started = std::time::Instant::now();
    let (outputs, events) = edit_turn(Gates {
        after_edit: Some("echo started; sleep 60".into()),
        timeout_s: 1,
        ..Gates::default()
    })
    .await;
    assert!(started.elapsed() < std::time::Duration::from_secs(30));
    assert!(outputs[1].contains("timed out after 1s"), "{}", outputs[1]);
    assert!(outputs[1].contains("started"), "{}", outputs[1]);
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::GateResult {
            gate: GateKind::AfterEdit,
            status: GateStatus::TimedOut,
            ..
        }
    )));
}

/// A turn that reads and edits `app.py` and ends twice, as a model does that is told its tests
/// fail and answers without changing anything; the user messages of the last request.
async fn test_turn(gates: Gates) -> (harness_core::event::TurnEndReason, Vec<AgentEvent>, String) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("app.py"), "print(1)\n").unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call("r1", "read", json!({"path": "app.py"})),
        Script::tool_call(
            "e1",
            "edit",
            json!({"path": "app.py", "old_string": "print(1)", "new_string": "print(2)"}),
        ),
        Script::text("done"),
        Script::text("still done"),
        Script::text("and again"),
    ]);
    let mut agent = agent(dir.path(), provider.clone(), gates);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let reason = agent
        .run_turn("change it".to_string(), &tx, CancellationToken::new())
        .await;
    drop(tx);
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }
    let last = provider.requests().last().unwrap().messages.clone();
    let users: Vec<String> = last
        .iter()
        .filter_map(|m| match m {
            harness_core::message::Message::User { content } => Some(content.clone()),
            _ => None,
        })
        .collect();
    (reason, events, users.join("\n---\n"))
}

// Spec "A gate times out": `sleep 600` with `timeout_s = 5` (here 1): the command stops, and the
// model receives a gate result saying it timed out.
#[tokio::test]
async fn a_test_command_that_runs_too_long_is_stopped_and_the_model_is_told() {
    let started = std::time::Instant::now();
    let (reason, events, seen) = test_turn(Gates {
        test: Some("sleep 600".into()),
        timeout_s: 1,
        ..Gates::default()
    })
    .await;
    assert!(started.elapsed() < std::time::Duration::from_secs(30));
    assert!(seen.contains("test gate timed out after 1s"), "{seen}");
    // Timing out twice with nothing printed is an identical failure.
    assert_eq!(reason, harness_core::event::TurnEndReason::GateFailed);
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::GateResult {
            gate: GateKind::Test,
            status: GateStatus::TimedOut,
            ..
        }
    )));
}

// Spec "Tests fail, then pass", with the real bash tool.
#[tokio::test]
async fn a_real_failing_test_command_sends_its_exit_code_and_tail() {
    let (reason, _, seen) = test_turn(Gates {
        test: Some("echo '2 tests failed'; exit 3".into()),
        ..Gates::default()
    })
    .await;
    assert!(seen.contains("exit code 3"), "{seen}");
    assert!(seen.contains("2 tests failed"), "{seen}");
    assert_eq!(reason, harness_core::event::TurnEndReason::GateFailed);
}

#[tokio::test]
async fn a_passing_real_test_command_finishes_the_turn() {
    let (reason, events, _) = test_turn(Gates {
        test: Some("true".into()),
        ..Gates::default()
    })
    .await;
    assert_eq!(reason, harness_core::event::TurnEndReason::Completed);
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::GateResult {
            gate: GateKind::Test,
            status: GateStatus::Passed,
            ..
        }
    )));
}
