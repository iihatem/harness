//! M2a and M2b meet: the outcome record holds the turn's gate results, and a gate's continuation
//! is a model request like any other, so the budget check comes before it.

mod common;

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use common::{
    gates::{ScriptedBash, Setup, agent, failing, passing},
    run,
};
use harness_core::{
    engine::RuleSet,
    event::{AgentEvent, TurnEndReason},
    gate::Gates,
    meter::{
        AccountKind, Avoided, BudgetKind, BudgetNotice, BudgetStatus, Meter, RequestCost,
        RequestRecord, TurnRecord,
    },
    permission::Mode,
    testing::{MockProvider, Script},
};
use serde_json::json;

const TEST: &str = "cargo test";
const LINT: &str = "cargo clippy";

/// Counts the requests and the tool calls it is told about, keeps the turns, and refuses a
/// request once `stop_after` were made.
struct Meterd {
    requests: AtomicUsize,
    checks: AtomicUsize,
    stop_after: usize,
    turns: Mutex<Vec<TurnRecord>>,
}

impl Meterd {
    fn new(stop_after: usize) -> Arc<Meterd> {
        Arc::new(Meterd {
            requests: AtomicUsize::new(0),
            checks: AtomicUsize::new(0),
            stop_after,
            turns: Mutex::new(Vec::new()),
        })
    }
}

impl Meter for Meterd {
    fn record_request(&self, _request: &RequestRecord) -> RequestCost {
        self.requests.fetch_add(1, Ordering::SeqCst);
        RequestCost {
            account: AccountKind::ApiKey,
            billed_usd: Some(0.5),
            list_usd: Some(0.5),
            avoided: Avoided::NotApplicable,
        }
    }
    fn check_budget(&self, _session: &str, _account: AccountKind) -> BudgetStatus {
        self.checks.fetch_add(1, Ordering::SeqCst);
        let spent = self.requests.load(Ordering::SeqCst);
        if spent >= self.stop_after {
            return BudgetStatus {
                stop: Some(BudgetNotice {
                    budget: BudgetKind::Session,
                    spent_usd: spent as f64 * 0.5,
                    limit_usd: 1.0,
                }),
                ..BudgetStatus::default()
            };
        }
        BudgetStatus::default()
    }
    fn record_turn(&self, turn: &TurnRecord) {
        self.turns.lock().unwrap().push(turn.clone());
    }
}

fn edit(id: &str) -> Script {
    Script::tool_call(
        id,
        "edit",
        json!({"path": "src.rs", "content": "fn x() {}\n"}),
    )
}

fn setup() -> Setup {
    Setup {
        gates: Gates {
            test: Some(TEST.into()),
            after_edit: Some(LINT.into()),
            max_retries: 3,
            ..Gates::default()
        },
        ..Setup::default()
    }
}

// The outcome record's `gates` holds how many gate commands passed, failed and were skipped in
// the turn: two lints after two edits, and a test that failed and then passed.
#[tokio::test]
async fn the_turn_record_counts_the_gates_that_passed_and_failed() {
    let dir = tempfile::tempdir().unwrap();
    let (bash, _) = ScriptedBash::new(vec![
        (TEST, vec![failing(1, "boom\n"), passing()]),
        (LINT, vec![passing()]),
    ]);
    let provider = MockProvider::new(vec![
        edit("e1"),
        Script::text("done"),
        edit("e2"),
        Script::text("fixed"),
    ]);
    let meter = Meterd::new(usize::MAX);
    let mut agent = agent(provider, dir.path(), bash, setup()).with_meter(meter.clone());
    let (reason, _) = run(&mut agent, "change it").await;
    assert_eq!(reason, TurnEndReason::Completed);
    let turns = meter.turns.lock().unwrap().clone();
    assert_eq!(turns.len(), 1);
    assert_eq!(
        (
            turns[0].gates.passed,
            turns[0].gates.failed,
            turns[0].gates.skipped
        ),
        (3, 1, 0)
    );
    // The model's two edits are its tool calls; the gates' commands are not.
    assert_eq!(turns[0].tool_calls, 2);
    // Each reply was one request, the continuation after the failed test included.
    assert_eq!(meter.requests.load(Ordering::SeqCst), 4);
}

// A turn's counts start again with the next turn.
#[tokio::test]
async fn the_gate_counts_do_not_carry_over_to_the_next_turn() {
    let dir = tempfile::tempdir().unwrap();
    let (bash, _) = ScriptedBash::new(vec![]);
    let provider = MockProvider::new(vec![edit("e1"), Script::text("one"), Script::text("two")]);
    let meter = Meterd::new(usize::MAX);
    let mut agent = agent(provider, dir.path(), bash, setup()).with_meter(meter.clone());
    run(&mut agent, "change it").await;
    run(&mut agent, "and now a question").await;
    let turns = meter.turns.lock().unwrap().clone();
    assert_eq!((turns[0].gates.passed, turns[0].gates.failed), (2, 0));
    assert_eq!(
        (
            turns[1].gates.passed,
            turns[1].gates.failed,
            turns[1].gates.skipped
        ),
        (0, 0, 0)
    );
}

// A gate that cannot run (plan mode) is `skipped`, the lint and the test both; a command a rule
// denies is too.
#[tokio::test]
async fn a_gate_that_did_not_run_is_counted_as_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let (bash, ran) = ScriptedBash::new(vec![]);
    let provider = MockProvider::new(vec![
        Script::tool_call("s1", "sneaky", json!({})),
        Script::text("planned"),
    ]);
    let meter = Meterd::new(usize::MAX);
    let mut agent = agent(
        provider,
        dir.path(),
        bash,
        Setup {
            mode: Mode::Plan,
            ..setup()
        },
    )
    .with_meter(meter.clone());
    run(&mut agent, "plan it").await;
    assert!(ran.commands().is_empty());
    let turns = meter.turns.lock().unwrap().clone();
    assert_eq!(
        (
            turns[0].gates.passed,
            turns[0].gates.failed,
            turns[0].gates.skipped
        ),
        (0, 0, 2)
    );
}

#[tokio::test]
async fn a_denied_gate_command_is_counted_as_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let (bash, ran) = ScriptedBash::new(vec![]);
    let provider = MockProvider::new(vec![edit("e1"), Script::text("a"), Script::text("b")]);
    let meter = Meterd::new(usize::MAX);
    let mut agent = agent(
        provider,
        dir.path(),
        bash,
        Setup {
            rules: RuleSet {
                deny: vec!["bash:cargo*".into()],
                ..RuleSet::default()
            },
            ..setup()
        },
    )
    .with_meter(meter.clone());
    run(&mut agent, "change it").await;
    assert!(ran.commands().is_empty());
    let turns = meter.turns.lock().unwrap().clone();
    assert_eq!(
        (
            turns[0].gates.passed,
            turns[0].gates.failed,
            turns[0].gates.skipped
        ),
        (0, 0, 2)
    );
}

// A failed test makes the model answer again, and that is a request: the budget is asked before
// it, and a budget that is reached ends the turn with `budget`, not with another request.
#[tokio::test]
async fn a_gate_continuation_is_stopped_by_the_budget_like_any_request() {
    let dir = tempfile::tempdir().unwrap();
    let (bash, ran) = ScriptedBash::new(vec![(TEST, vec![failing(1, "boom\n")])]);
    let provider = MockProvider::new(vec![
        edit("e1"),
        Script::text("done"),
        Script::text("never"),
    ]);
    // The budget is reached after the second request: the edit's and the reply's.
    let meter = Meterd::new(2);
    let mut agent = agent(
        provider.clone(),
        dir.path(),
        bash,
        Setup {
            gates: Gates {
                after_edit: None,
                ..setup().gates
            },
            ..setup()
        },
    )
    .with_meter(meter.clone());
    let (reason, events) = run(&mut agent, "change it").await;
    assert_eq!(reason, TurnEndReason::Budget);
    assert_eq!(ran.commands(), [TEST]);
    // The failed test was told to the model, but the request that would carry it was not sent.
    assert_eq!(provider.requests().len(), 2);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::BudgetReached { .. }))
    );
    let turns = meter.turns.lock().unwrap().clone();
    assert_eq!(turns[0].finish_reason, "budget");
    assert_eq!((turns[0].gates.failed, turns[0].gates.passed), (1, 0));
    // The check came before each of the three attempts.
    assert_eq!(meter.checks.load(Ordering::SeqCst), 3);
}
