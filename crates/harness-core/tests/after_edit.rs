//! The after-edit gate: its output is appended to the edit's result, through the bash tool's path.

mod common;

use std::sync::Arc;

use common::{
    finished_outputs,
    gates::{ScriptedBash, Setup, agent, failing, passing},
    run,
};
use harness_core::{
    agent::{ApprovalDecision, ApprovalRequest, Approver},
    engine::RuleSet,
    event::{AgentEvent, GateKind, GateStatus},
    gate::Gates,
    permission::Mode,
    redact::Redactor,
    testing::{MockProvider, Script},
    tool::ToolOutput,
};
use serde_json::json;

fn edits(then: &str) -> Arc<MockProvider> {
    MockProvider::new(vec![
        Script::tool_call(
            "e1",
            "edit",
            json!({"path": "app.py", "content": "import os\n"}),
        ),
        Script::text(then),
    ])
}

fn lint(command: &str) -> Gates {
    Gates {
        after_edit: Some(command.into()),
        ..Gates::default()
    }
}

fn setup(command: &str) -> Setup {
    Setup {
        gates: lint(command),
        ..Setup::default()
    }
}

// Spec "Lint failure after an edit": the edit's confirmation, then that output, in the same
// tool result.
#[tokio::test]
async fn a_failing_lint_is_appended_to_the_edit_result() {
    let dir = tempfile::tempdir().unwrap();
    let (bash, ran) = ScriptedBash::new(vec![(
        "ruff check .",
        vec![failing(1, "app.py:3:1: F401 unused import\n")],
    )]);
    let mut agent = agent(edits("fixed"), dir.path(), bash, setup("ruff check ."));
    let (_, events) = run(&mut agent, "edit it").await;
    let outputs = finished_outputs(&events);
    let (content, is_error) = &outputs[0];
    assert!(content.starts_with("edited\n"), "{content}");
    assert!(content.contains("after_edit gate"), "{content}");
    assert!(content.contains("`ruff check .`"), "{content}");
    assert!(content.contains("exit code 1"), "{content}");
    assert!(
        content.contains("app.py:3:1: F401 unused import"),
        "{content}"
    );
    // The edit itself succeeded: the model is not told it failed.
    assert!(!is_error);
    assert_eq!(ran.commands(), ["ruff check ."]);
    // The model got the same text in the tool message.
    let history = agent.history();
    let sent = format!("{history:?}");
    assert!(sent.contains("F401 unused import"), "{sent}");
}

// Spec "Passing check": no failure text, a short line saying the check ran.
#[tokio::test]
async fn a_passing_lint_adds_one_short_line() {
    let dir = tempfile::tempdir().unwrap();
    let (bash, ran) = ScriptedBash::new(vec![("ruff check .", vec![passing()])]);
    let mut agent = agent(edits("ok"), dir.path(), bash, setup("ruff check ."));
    let (_, events) = run(&mut agent, "edit it").await;
    let (content, _) = &finished_outputs(&events)[0];
    assert_eq!(content, "edited\n[after_edit gate passed: `ruff check .`]");
    assert!(!content.contains("all good"));
    assert_eq!(ran.commands().len(), 1);
}

// Spec "No gates configured": an edit with no `after_edit` command is unchanged.
#[tokio::test]
async fn without_an_after_edit_command_the_edit_result_is_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let (bash, ran) = ScriptedBash::new(vec![]);
    let mut agent = agent(edits("ok"), dir.path(), bash, Setup::default());
    let (_, events) = run(&mut agent, "edit it").await;
    assert_eq!(finished_outputs(&events)[0].0, "edited");
    assert!(ran.commands().is_empty());
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::GateResult { .. }))
    );
}

#[tokio::test]
async fn a_failed_edit_runs_no_gate() {
    let dir = tempfile::tempdir().unwrap();
    let (bash, ran) = ScriptedBash::new(vec![]);
    let provider = MockProvider::new(vec![
        Script::tool_call("e1", "edit", json!({"path": "app.py", "content": "FAIL"})),
        Script::text("sorry"),
    ]);
    let mut agent = agent(provider, dir.path(), bash, setup("ruff check ."));
    let (_, events) = run(&mut agent, "edit it").await;
    assert_eq!(finished_outputs(&events)[0], ("edit failed".into(), true));
    assert!(ran.commands().is_empty());
}

#[tokio::test]
async fn a_tool_that_changes_no_file_runs_no_gate() {
    let dir = tempfile::tempdir().unwrap();
    let (bash, ran) = ScriptedBash::new(vec![]);
    let provider = MockProvider::new(vec![
        Script::tool_call("b1", "bash", json!({"command": "ls"})),
        Script::text("done"),
    ]);
    let mut agent = agent(provider, dir.path(), bash, setup("ruff check ."));
    run(&mut agent, "look").await;
    // Only the model's own command ran.
    assert_eq!(ran.commands(), ["ls"]);
}

// Spec "A gate times out" (the after-edit side): the command gets `timeout_s`, and the model is
// told it timed out.
#[tokio::test]
async fn the_command_is_given_the_timeout_and_a_timeout_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let (bash, ran) = ScriptedBash::new(vec![(
        "sleep 600",
        vec![ToolOutput::error(
            "command timed out after 5s and was terminated\npartial output\n",
        )],
    )]);
    let mut gates = lint("sleep 600");
    gates.timeout_s = 5;
    let mut agent = agent(
        edits("ok"),
        dir.path(),
        bash,
        Setup {
            gates,
            ..Setup::default()
        },
    );
    let (_, events) = run(&mut agent, "edit it").await;
    let (content, is_error) = &finished_outputs(&events)[0];
    assert!(content.contains("timed out after 5s"), "{content}");
    assert!(content.contains("partial output"), "{content}");
    assert!(!is_error);
    assert_eq!(*ran.timeouts.lock().unwrap(), [Some(5)]);
    let gate = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::GateResult { gate, status, .. } => Some((*gate, *status)),
            _ => None,
        })
        .unwrap();
    assert_eq!(gate, (GateKind::AfterEdit, GateStatus::TimedOut));
}

// Spec "Denied command": the command does not run, and the model is told it was blocked.
#[tokio::test]
async fn a_denied_lint_command_is_reported_as_blocked_and_not_run() {
    let dir = tempfile::tempdir().unwrap();
    let (bash, ran) = ScriptedBash::new(vec![]);
    let mut agent = agent(
        edits("ok"),
        dir.path(),
        bash,
        Setup {
            rules: RuleSet {
                deny: vec!["bash:ruff*".into()],
                ..RuleSet::default()
            },
            ..setup("ruff check .")
        },
    );
    let (_, events) = run(&mut agent, "edit it").await;
    let (content, is_error) = &finished_outputs(&events)[0];
    assert!(content.starts_with("edited\n"), "{content}");
    assert!(content.contains("blocked"), "{content}");
    assert!(content.contains("did not run"), "{content}");
    assert!(!is_error);
    assert!(ran.commands().is_empty());
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::GateResult {
            gate: GateKind::AfterEdit,
            status: GateStatus::Blocked,
            ..
        }
    )));
    // It counts as blocked for lack of approval: a headless run exits 3.
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::ActionBlocked { .. }))
    );
}

// Spec "A command being written by the user MUST NOT approve it": in `ask` mode the lint command
// needs approval like any command, and with nobody to ask it is blocked.
#[tokio::test]
async fn a_command_that_needs_approval_and_gets_none_is_blocked() {
    let dir = tempfile::tempdir().unwrap();
    let (bash, ran) = ScriptedBash::new(vec![]);
    let mut agent = agent(
        MockProvider::new(vec![
            Script::tool_call("e1", "edit", json!({"path": "app.py", "content": "x"})),
            Script::text("ok"),
        ]),
        dir.path(),
        bash,
        Setup {
            mode: Mode::Ask,
            rules: RuleSet {
                allow: vec!["write:**".into()],
                ..RuleSet::default()
            },
            ..setup("ruff check .")
        },
    );
    let (_, events) = run(&mut agent, "edit it").await;
    let (content, _) = &finished_outputs(&events)[0];
    assert!(content.contains("blocked"), "{content}");
    assert!(ran.commands().is_empty());
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::ActionBlocked { .. }))
    );
}

struct Approves(std::sync::Mutex<Vec<String>>);

#[async_trait::async_trait]
impl Approver for Approves {
    async fn decide(&self, request: &ApprovalRequest) -> ApprovalDecision {
        self.0.lock().unwrap().push(request.reason.clone());
        ApprovalDecision::Approve
    }
}

#[tokio::test]
async fn an_approved_command_runs() {
    let dir = tempfile::tempdir().unwrap();
    let (bash, ran) = ScriptedBash::new(vec![]);
    let approver = Arc::new(Approves(Default::default()));
    let mut agent = agent(
        edits("ok"),
        dir.path(),
        bash,
        Setup {
            mode: Mode::Ask,
            rules: RuleSet {
                allow: vec!["write:**".into()],
                ..RuleSet::default()
            },
            approver: approver.clone(),
            ..setup("ruff check .")
        },
    );
    run(&mut agent, "edit it").await;
    assert_eq!(approver.0.lock().unwrap().len(), 1);
    assert_eq!(ran.commands(), ["ruff check ."]);
}

// Spec "Gates are skipped in plan and read-only modes".
#[tokio::test]
async fn read_only_modes_run_no_gate() {
    for mode in [Mode::Plan, Mode::ReadOnly] {
        let dir = tempfile::tempdir().unwrap();
        let (bash, ran) = ScriptedBash::new(vec![]);
        let provider = MockProvider::new(vec![
            Script::tool_call("s1", "sneaky", json!({})),
            Script::text("ok"),
        ]);
        let mut agent = agent(
            provider,
            dir.path(),
            bash,
            Setup {
                mode,
                ..setup("ruff check .")
            },
        );
        let (_, events) = run(&mut agent, "go").await;
        assert_eq!(finished_outputs(&events)[0].0, "done", "{mode}");
        assert!(ran.commands().is_empty(), "{mode}");
    }
}

// Spec "Long failing output", the part that is not about the turn's end: what the model gets is
// redacted, as in all other output.
#[tokio::test]
async fn the_output_the_model_gets_is_redacted() {
    let dir = tempfile::tempdir().unwrap();
    let (bash, _) = ScriptedBash::new(vec![(
        "lint",
        vec![failing(1, "key sk-canary-0123456789abcdef used\n")],
    )]);
    let redactor = Arc::new(Redactor::default());
    redactor.add("sk-canary-0123456789abcdef");
    let mut agent = agent(edits("ok"), dir.path(), bash, setup("lint")).with_redactor(redactor);
    let (_, _) = run(&mut agent, "edit it").await;
    let sent = format!("{:?}", agent.history());
    assert!(sent.contains("[redacted]"), "{sent}");
    assert!(!sent.contains("canary"), "{sent}");
}

// The tail is the last `output_tail_lines` lines, with a note on what was left out.
#[tokio::test]
async fn only_the_tail_of_a_long_output_is_appended() {
    let dir = tempfile::tempdir().unwrap();
    let long: String = (1..=100).map(|n| format!("line {n}\n")).collect();
    let (bash, _) = ScriptedBash::new(vec![("lint", vec![failing(2, &long)])]);
    let mut gates = lint("lint");
    gates.output_tail_lines = 3;
    let mut agent = agent(
        edits("ok"),
        dir.path(),
        bash,
        Setup {
            gates,
            ..Setup::default()
        },
    );
    let (_, events) = run(&mut agent, "edit it").await;
    let (content, _) = &finished_outputs(&events)[0];
    assert!(content.contains("line 100\n"), "{content}");
    assert!(content.contains("line 98\n"), "{content}");
    assert!(!content.contains("line 97\n"), "{content}");
    assert!(content.contains("97 earlier lines omitted"), "{content}");
    // The full output was saved as large tool output is.
    let saved = content
        .split("saved to ")
        .nth(1)
        .and_then(|rest| rest.split(']').next())
        .unwrap();
    let text = std::fs::read_to_string(saved).unwrap();
    assert!(text.contains("line 1\n") && text.contains("line 100\n"));
}
