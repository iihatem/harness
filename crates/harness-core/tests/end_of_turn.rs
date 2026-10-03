//! The test gate: it runs once when a turn that changed files ends, and what it does when the
//! tests fail.

mod common;

use std::sync::Arc;

use common::{
    gates::{ScriptedBash, Setup, agent, agent_without_bash, failing, passing},
    run, run_with,
};
use harness_core::{
    engine::RuleSet,
    event::{AgentEvent, GateKind, GateStatus, TurnEndReason},
    gate::Gates,
    message::Message,
    permission::Mode,
    redact::Redactor,
    testing::{MockProvider, Script},
    turn::Steering,
};
use serde_json::json;
use tokio_util::sync::CancellationToken;

const TEST: &str = "cargo test";

fn edit(id: &str) -> Script {
    Script::tool_call(
        id,
        "edit",
        json!({"path": "src.rs", "content": "fn x() {}\n"}),
    )
}

fn gates(retries: u32) -> Gates {
    Gates {
        test: Some(TEST.into()),
        max_retries: retries,
        ..Gates::default()
    }
}

fn setup(retries: u32) -> Setup {
    Setup {
        gates: gates(retries),
        ..Setup::default()
    }
}

fn test_results(events: &[AgentEvent]) -> Vec<(GateStatus, Option<i32>)> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::GateResult {
                gate: GateKind::Test,
                status,
                exit_code,
                ..
            } => Some((*status, *exit_code)),
            _ => None,
        })
        .collect()
}

/// The last user message of each request sent to the model.
fn last_users(provider: &MockProvider) -> Vec<String> {
    provider
        .requests()
        .iter()
        .map(|r| {
            r.messages
                .iter()
                .rev()
                .find_map(|m| match m {
                    Message::User { content } => Some(content.clone()),
                    _ => None,
                })
                .unwrap_or_default()
        })
        .collect()
}

// Spec "Tests fail, then pass": completed; the test ran twice; none ran between the edit and
// the first end of the turn.
#[tokio::test]
async fn tests_that_fail_and_then_pass_finish_the_turn_completed() {
    let dir = tempfile::tempdir().unwrap();
    let (bash, ran) = ScriptedBash::new(vec![(TEST, vec![failing(1, "boom\n"), passing()])]);
    let provider = MockProvider::new(vec![
        edit("e1"),
        Script::text("done"),
        edit("e2"),
        Script::text("fixed"),
    ]);
    let mut agent = agent(provider.clone(), dir.path(), bash, setup(3));
    let (reason, events) = run(&mut agent, "change it").await;
    assert_eq!(reason, TurnEndReason::Completed);
    assert_eq!(ran.commands(), [TEST, TEST]);
    assert_eq!(
        test_results(&events),
        [(GateStatus::Failed, Some(1)), (GateStatus::Passed, Some(0))]
    );
    // Nothing ran between the first edit's result and the model ending its turn.
    let first_gate = events
        .iter()
        .position(|e| {
            matches!(
                e,
                AgentEvent::GateResult {
                    gate: GateKind::Test,
                    ..
                }
            )
        })
        .unwrap();
    let first_end = events
        .iter()
        .position(
            |e| matches!(e, AgentEvent::AssistantMessage { content, .. } if content == "done"),
        )
        .unwrap();
    assert!(first_end < first_gate);
    // The model got the failure as a gate result, then went on.
    let continued = &last_users(&provider)[2];
    assert!(continued.contains("test gate failed"), "{continued}");
    assert!(continued.contains("exit code 1"), "{continued}");
    assert!(continued.contains("boom"), "{continued}");
    // One turn: it finished once, after the last gate result.
    let finished: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::TurnFinished { .. }))
        .collect();
    assert_eq!(finished.len(), 1);
    assert!(matches!(
        events.last(),
        Some(AgentEvent::TurnFinished { .. })
    ));
}

// Spec "No files changed".
#[tokio::test]
async fn a_turn_that_changed_no_file_runs_no_test() {
    let dir = tempfile::tempdir().unwrap();
    let (bash, ran) = ScriptedBash::new(vec![]);
    let provider = MockProvider::new(vec![Script::text("it is 4")]);
    let mut agent = agent(provider, dir.path(), bash, setup(3));
    let (reason, events) = run(&mut agent, "what is 2+2").await;
    assert_eq!(reason, TurnEndReason::Completed);
    assert!(ran.commands().is_empty());
    assert!(test_results(&events).is_empty());
}

// Spec "Retry cap reached": 3 continuations, and after the 4th failure the turn finishes with
// `gate_failed`.
#[tokio::test]
async fn the_model_gets_max_retries_continuations_and_then_the_turn_fails() {
    let dir = tempfile::tempdir().unwrap();
    let (bash, ran) = ScriptedBash::new(vec![(
        TEST,
        vec![
            failing(1, "one\n"),
            failing(1, "two\n"),
            failing(1, "three\n"),
            failing(1, "four\n"),
        ],
    )]);
    let provider = MockProvider::new(vec![
        edit("e1"),
        Script::text("a"),
        Script::text("b"),
        Script::text("c"),
        Script::text("d"),
    ]);
    let mut agent = agent(provider.clone(), dir.path(), bash, setup(3));
    let (reason, events) = run(&mut agent, "change it").await;
    assert_eq!(reason, TurnEndReason::GateFailed);
    assert_eq!(ran.commands().len(), 4);
    // The edit, its result, and then the three continuations.
    assert_eq!(provider.requests().len(), 5);
    assert!(matches!(
        events.last(),
        Some(AgentEvent::TurnFinished {
            reason: TurnEndReason::GateFailed
        })
    ));
    // The last failure is the last thing shown, with its tail.
    let last = events
        .iter()
        .rev()
        .find_map(|e| match e {
            AgentEvent::GateResult { tail, .. } => tail.clone(),
            _ => None,
        })
        .unwrap();
    assert_eq!(last, "four\n");
}

// Spec "Identical failure": the second identical failure ends the turn at once.
#[tokio::test]
async fn an_identical_failure_ends_the_turn_without_another_continuation() {
    let dir = tempfile::tempdir().unwrap();
    let (bash, ran) = ScriptedBash::new(vec![(TEST, vec![failing(101, "same\n")])]);
    let provider = MockProvider::new(vec![edit("e1"), Script::text("a"), Script::text("b")]);
    let mut agent = agent(provider.clone(), dir.path(), bash, setup(3));
    let (reason, _) = run(&mut agent, "change it").await;
    assert_eq!(reason, TurnEndReason::GateFailed);
    assert_eq!(ran.commands().len(), 2);
    assert_eq!(provider.requests().len(), 3);
}

// Same tail with another exit code, or the same code with another tail, is a different failure.
#[tokio::test]
async fn a_failure_that_differs_in_code_or_tail_is_not_identical() {
    let dir = tempfile::tempdir().unwrap();
    let (bash, ran) = ScriptedBash::new(vec![(
        TEST,
        vec![
            failing(1, "same\n"),
            failing(2, "same\n"),
            failing(2, "other\n"),
            passing(),
        ],
    )]);
    let provider = MockProvider::new(vec![
        edit("e1"),
        Script::text("a"),
        Script::text("b"),
        Script::text("c"),
        Script::text("d"),
    ]);
    let mut agent = agent(provider, dir.path(), bash, setup(5));
    let (reason, _) = run(&mut agent, "change it").await;
    assert_eq!(reason, TurnEndReason::Completed);
    assert_eq!(ran.commands().len(), 4);
}

#[tokio::test]
async fn with_no_retries_the_first_failure_ends_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let (bash, _) = ScriptedBash::new(vec![(TEST, vec![failing(1, "no\n")])]);
    let provider = MockProvider::new(vec![edit("e1"), Script::text("a")]);
    let mut agent = agent(provider.clone(), dir.path(), bash, setup(0));
    let (reason, _) = run(&mut agent, "change it").await;
    assert_eq!(reason, TurnEndReason::GateFailed);
    assert_eq!(provider.requests().len(), 2);
}

// Spec "Long failing output": the exit code and the last 60 lines, the key redacted.
#[tokio::test]
async fn the_model_gets_the_last_60_lines_with_secrets_redacted() {
    let dir = tempfile::tempdir().unwrap();
    let mut long: String = (1..500).map(|n| format!("line {n}\n")).collect();
    long.push_str("token sk-canary-0123456789abcdef\n");
    let (bash, _) = ScriptedBash::new(vec![(TEST, vec![failing(101, &long), passing()])]);
    let provider = MockProvider::new(vec![edit("e1"), Script::text("a"), Script::text("b")]);
    let redactor = Arc::new(Redactor::default());
    redactor.add("sk-canary-0123456789abcdef");
    let mut agent = agent(provider.clone(), dir.path(), bash, setup(3)).with_redactor(redactor);
    let (reason, _) = run(&mut agent, "change it").await;
    assert_eq!(reason, TurnEndReason::Completed);
    let seen = &last_users(&provider)[2];
    assert!(seen.contains("exit code 101"), "{seen}");
    assert!(seen.contains("token [redacted]"), "{seen}");
    assert!(!seen.contains("canary"), "{seen}");
    assert!(seen.contains("line 499\n"), "{seen}");
    // 59 lines of output and the token line: line 441 is the first that is kept.
    assert!(
        seen.contains("line 441\n") && !seen.contains("line 440\n"),
        "{seen}"
    );
    assert!(seen.contains("440 earlier lines omitted"), "{seen}");
}

// Spec "Denied command" / "Sandbox applies": the test command is under the permission rules.
#[tokio::test]
async fn a_denied_test_command_does_not_run_and_the_model_is_told() {
    let dir = tempfile::tempdir().unwrap();
    let (bash, ran) = ScriptedBash::new(vec![]);
    let provider = MockProvider::new(vec![edit("e1"), Script::text("a"), Script::text("b")]);
    let mut agent = agent(
        provider.clone(),
        dir.path(),
        bash,
        Setup {
            rules: RuleSet {
                deny: vec!["bash:cargo*".into()],
                ..RuleSet::default()
            },
            ..setup(3)
        },
    );
    let (reason, events) = run(&mut agent, "change it").await;
    assert!(ran.commands().is_empty());
    assert_eq!(test_results(&events), [(GateStatus::Blocked, None)]);
    let told = &last_users(&provider)[2];
    assert!(told.contains("blocked"), "{told}");
    // A command nobody may run is not asked again, and is no test failure: the turn completes,
    // and a headless run exits 3 for the blocked action.
    assert_eq!(reason, TurnEndReason::Completed);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::ActionBlocked { .. }))
    );
}

// Spec "Plan mode": no command at the end, and the turn's gate result is `skipped`.
#[tokio::test]
async fn plan_and_read_only_skip_the_test_and_say_so() {
    for mode in [Mode::Plan, Mode::ReadOnly] {
        let dir = tempfile::tempdir().unwrap();
        let (bash, ran) = ScriptedBash::new(vec![]);
        let provider = MockProvider::new(vec![
            Script::tool_call("s1", "sneaky", json!({})),
            Script::text("planned"),
        ]);
        let mut agent = agent(provider, dir.path(), bash, Setup { mode, ..setup(3) });
        let (reason, events) = run(&mut agent, "plan it").await;
        assert_eq!(reason, TurnEndReason::Completed, "{mode}");
        assert!(ran.commands().is_empty(), "{mode}");
        assert_eq!(
            test_results(&events),
            [(GateStatus::Skipped, None)],
            "{mode}"
        );
    }
}

// `/init` runs its turn with a read-only shell: the test would run in a read-only sandbox and
// fail, so it is skipped like in a read-only mode.
#[tokio::test]
async fn a_turn_with_a_read_only_shell_skips_the_test() {
    let dir = tempfile::tempdir().unwrap();
    let (bash, ran) = ScriptedBash::new(vec![]);
    let provider = MockProvider::new(vec![edit("e1"), Script::text("written")]);
    let mut agent = agent(provider, dir.path(), bash, setup(3));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let input = harness_core::turn::TurnInput {
        read_only_shell: true,
        ..harness_core::turn::TurnInput::from("init")
    };
    let reason = agent.run_turn(input, &tx, CancellationToken::new()).await;
    drop(tx);
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }
    assert_eq!(reason, TurnEndReason::Completed);
    assert!(ran.commands().is_empty());
    assert_eq!(test_results(&events), [(GateStatus::Skipped, None)]);
}

// One dim line per turn is noise: the skip is said once while the mode stays.
#[tokio::test]
async fn the_skip_is_said_once_not_after_every_reply() {
    let dir = tempfile::tempdir().unwrap();
    let (bash, _) = ScriptedBash::new(vec![]);
    let provider = MockProvider::new(vec![Script::text("one"), Script::text("two")]);
    let mut agent = agent(
        provider,
        dir.path(),
        bash,
        Setup {
            mode: Mode::Plan,
            ..setup(3)
        },
    );
    let (_, first) = run(&mut agent, "a question").await;
    let (_, second) = run(&mut agent, "another").await;
    assert_eq!(test_results(&first), [(GateStatus::Skipped, None)]);
    assert!(test_results(&second).is_empty());
}

// Spec "Send-now input during the test run": the model receives the gate result and that
// message together.
#[tokio::test]
async fn send_now_input_during_the_test_run_arrives_with_the_gate_result() {
    let dir = tempfile::tempdir().unwrap();
    let steering = Steering::new();
    let sender = steering.clone();
    let (bash, _) = ScriptedBash::new(vec![(TEST, vec![failing(1, "flaky\n"), passing()])]);
    let first = std::sync::atomic::AtomicBool::new(true);
    let bash = bash.during(TEST, move || {
        if first.swap(false, std::sync::atomic::Ordering::SeqCst) {
            sender.send("skip the flaky test");
        }
    });
    let provider = MockProvider::new(vec![edit("e1"), Script::text("a"), Script::text("b")]);
    let mut agent = agent(provider.clone(), dir.path(), bash, setup(3)).with_steering(steering);
    let (reason, _) = run(&mut agent, "change it").await;
    assert_eq!(reason, TurnEndReason::Completed);
    let together = &last_users(&provider)[2];
    let at = together.find("flaky").expect("the gate result");
    assert!(together[at..].contains("skip the flaky test"), "{together}");
    assert!(together.contains("test gate failed"), "{together}");
}

// A passing run does not leave send-now input behind: the model gets it, as it would after a
// tool result.
#[tokio::test]
async fn send_now_input_during_a_passing_test_run_is_still_delivered() {
    let dir = tempfile::tempdir().unwrap();
    let steering = Steering::new();
    let sender = steering.clone();
    let (bash, ran) = ScriptedBash::new(vec![(TEST, vec![passing()])]);
    let bash = bash.during(TEST, move || sender.send("also say hi"));
    let provider = MockProvider::new(vec![edit("e1"), Script::text("a"), Script::text("hi")]);
    let mut agent =
        agent(provider.clone(), dir.path(), bash, setup(3)).with_steering(steering.clone());
    let (reason, _) = run(&mut agent, "change it").await;
    assert_eq!(reason, TurnEndReason::Completed);
    assert_eq!(last_users(&provider)[2], "also say hi");
    assert!(steering.is_empty());
    // The model changed nothing more, so the tests did not run again.
    assert_eq!(ran.commands().len(), 1);
}

#[tokio::test]
async fn stopping_the_turn_during_the_test_run_interrupts_it() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let stop = cancel.clone();
    let (bash, _) = ScriptedBash::new(vec![(TEST, vec![failing(1, "x\n")])]);
    let bash = bash.during(TEST, move || stop.cancel());
    let provider = MockProvider::new(vec![edit("e1"), Script::text("a"), Script::text("b")]);
    let mut agent = agent(provider.clone(), dir.path(), bash, setup(3));
    let (reason, events) = run_with(&mut agent, "change it", cancel).await;
    assert_eq!(reason, TurnEndReason::Interrupted);
    assert_eq!(provider.requests().len(), 2);
    // A stopped run is not a failed one: no "tests failed" line.
    assert!(
        test_results(&events).is_empty(),
        "{:?}",
        test_results(&events)
    );
}

// The test runs only at the end of a turn, once per end, however many edits came before it.
#[tokio::test]
async fn several_edits_run_the_test_once_at_the_end() {
    let dir = tempfile::tempdir().unwrap();
    let (bash, ran) = ScriptedBash::new(vec![]);
    let provider = MockProvider::new(vec![
        edit("e1"),
        edit("e2"),
        edit("e3"),
        Script::text("done"),
    ]);
    let mut agent = agent(provider, dir.path(), bash, setup(3));
    run(&mut agent, "change it").await;
    assert_eq!(ran.commands(), [TEST]);
}

// Each turn starts afresh: a turn that changed nothing, after one that did, runs nothing.
#[tokio::test]
async fn the_next_turn_does_not_inherit_the_changes() {
    let dir = tempfile::tempdir().unwrap();
    let (bash, ran) = ScriptedBash::new(vec![]);
    let provider = MockProvider::new(vec![edit("e1"), Script::text("done"), Script::text("4")]);
    let mut agent = agent(provider, dir.path(), bash, setup(3));
    run(&mut agent, "change it").await;
    run(&mut agent, "what is 2+2").await;
    assert_eq!(ran.commands(), [TEST]);
}

// Review Focus: gates configured for a session with no `bash` tool cannot run, and do not break it.
#[tokio::test]
async fn gates_with_no_bash_tool_run_nothing_and_the_turn_goes_on() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![edit("e1"), Script::text("done")]);
    let mut agent = agent_without_bash(
        provider,
        dir.path(),
        Setup {
            gates: Gates {
                after_edit: Some("lint".into()),
                test: Some(TEST.into()),
                ..Gates::default()
            },
            ..Setup::default()
        },
    );
    let (reason, events) = run(&mut agent, "change it").await;
    assert_eq!(reason, TurnEndReason::Completed);
    assert_eq!(common::finished_outputs(&events)[0].0, "edited");
    assert!(test_results(&events).is_empty());
}
