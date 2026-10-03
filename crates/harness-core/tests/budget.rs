//! 1.9: before each model request the runtime asks the meter about the budgets; at 100% the
//! request is not sent and the turn ends with reason `budget`.

mod common;

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use common::*;
use harness_core::{
    agent::NonInteractive,
    event::{AgentEvent, TurnEndReason},
    meter::{
        AccountKind, Avoided, BudgetKind, BudgetNotice, BudgetStatus, Meter, RequestCost,
        RequestRecord,
    },
    permission::Mode,
    testing::{MockProvider, Script},
};
use serde_json::json;

/// A meter whose budget is reached after `stop_after` requests, and which warns before that.
struct Capped {
    requests: AtomicUsize,
    stop_after: usize,
    warned: Mutex<bool>,
}

impl Capped {
    fn new(stop_after: usize) -> Arc<Capped> {
        Arc::new(Capped {
            requests: AtomicUsize::new(0),
            stop_after,
            warned: Mutex::new(false),
        })
    }
}

impl Meter for Capped {
    fn record_request(&self, _request: &RequestRecord) -> RequestCost {
        self.requests.fetch_add(1, Ordering::SeqCst);
        RequestCost {
            account: AccountKind::ApiKey,
            billed_usd: Some(0.51),
            list_usd: Some(0.51),
            avoided: Avoided::NotApplicable,
        }
    }

    fn check_budget(&self, _session: &str, account: AccountKind) -> BudgetStatus {
        let spent = self.requests.load(Ordering::SeqCst);
        let notice = |spent: usize| BudgetNotice {
            budget: BudgetKind::Session,
            spent_usd: spent as f64 * 0.51,
            limit_usd: 1.0,
        };
        if spent >= self.stop_after {
            // Only a request that would add billed cost is refused.
            return if account == AccountKind::ApiKey {
                BudgetStatus {
                    stop: Some(notice(spent)),
                    ..BudgetStatus::default()
                }
            } else {
                BudgetStatus {
                    paused: Some(notice(spent)),
                    ..BudgetStatus::default()
                }
            };
        }
        let mut warned = self.warned.lock().unwrap();
        if spent >= 1 && !*warned {
            *warned = true;
            return BudgetStatus {
                warnings: vec![notice(spent)],
                ..BudgetStatus::default()
            };
        }
        BudgetStatus::default()
    }
}

fn echo(id: &str) -> Script {
    Script::tool_call(id, "echo", json!({"text": "x"}))
}

// `session_usd = 1.00`, a turn has made several tool calls, and billed cost reaches $1.02 before
// the next request: that request is not sent, the turn finishes with reason `budget`, and the
// event names the session budget.
#[tokio::test]
async fn a_multi_request_turn_stops_before_the_request_that_would_cross_the_budget() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        echo("c1"),
        echo("c2"),
        echo("c3"),
        Script::text("never"),
    ]);
    let meter = Capped::new(2);
    let mut agent = agent(
        provider.clone(),
        Mode::Auto,
        Arc::new(NonInteractive),
        dir.path(),
    )
    .with_meter(meter);
    let (reason, events) = run(&mut agent, "go").await;
    assert_eq!(reason, TurnEndReason::Budget);
    assert_eq!(
        provider.requests().len(),
        2,
        "the third request was not sent"
    );
    let reached: Vec<&BudgetNotice> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::BudgetReached { notice } => Some(notice),
            _ => None,
        })
        .collect();
    assert_eq!(reached.len(), 1, "{events:?}");
    assert_eq!(reached[0].budget, BudgetKind::Session);
    assert!((reached[0].spent_usd - 1.02).abs() < 1e-9);
    // The budget event comes before the turn's stats and its end, and no error is raised.
    let n = events.len();
    let at = events
        .iter()
        .position(|e| matches!(e, AgentEvent::BudgetReached { .. }))
        .unwrap();
    assert!(at > n - 4, "{events:?}");
    assert!(matches!(
        events[n - 1],
        AgentEvent::TurnFinished {
            reason: TurnEndReason::Budget
        }
    ));
    assert!(!events.iter().any(|e| matches!(e, AgentEvent::Error { .. })));
}

#[tokio::test]
async fn a_warning_is_an_event_and_the_requests_go_on() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![echo("c1"), Script::text("done")]);
    let mut agent = agent(
        provider.clone(),
        Mode::Auto,
        Arc::new(NonInteractive),
        dir.path(),
    )
    .with_meter(Capped::new(10));
    let (reason, events) = run(&mut agent, "go").await;
    assert_eq!(reason, TurnEndReason::Completed);
    assert_eq!(provider.requests().len(), 2);
    let warnings = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::BudgetWarning { .. }))
        .count();
    assert_eq!(warnings, 1, "{events:?}");
}

// The next message after a stop runs normally once the budget is raised: the meter says so.
#[tokio::test]
async fn a_new_turn_asks_again_and_runs_when_the_budget_allows() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![Script::text("one"), Script::text("two")]);
    let meter = Capped::new(1);
    let mut agent = agent(
        provider.clone(),
        Mode::Auto,
        Arc::new(NonInteractive),
        dir.path(),
    )
    .with_meter(meter.clone());
    let (reason, _) = run(&mut agent, "first").await;
    assert_eq!(
        reason,
        TurnEndReason::Completed,
        "no request had spent anything yet"
    );
    let (reason, _) = run(&mut agent, "second").await;
    assert_eq!(reason, TurnEndReason::Budget);
    assert_eq!(provider.requests().len(), 1);
}

// At 100% a model of the user's own is not billed, so its requests go on; the user is told once
// per turn that paid models are paused, and the check still runs before every request.
#[tokio::test]
async fn a_local_request_goes_on_at_100_percent_and_the_pause_is_said_once_per_turn() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        echo("c1"),
        echo("c2"),
        Script::text("done"),
        Script::text("again"),
    ]);
    let meter = Capped::new(0);
    let mut agent = agent(
        provider.clone(),
        Mode::Auto,
        Arc::new(NonInteractive),
        dir.path(),
    )
    .with_meter(meter);
    agent.config_mut().request.local = true;
    let (reason, events) = run(&mut agent, "go").await;
    assert_eq!(reason, TurnEndReason::Completed, "{events:?}");
    assert_eq!(provider.requests().len(), 3, "no request was refused");
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::BudgetReached { .. }))
    );
    let paused: Vec<&String> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Warning { message } if message.contains("paused") => Some(message),
            _ => None,
        })
        .collect();
    assert_eq!(
        paused.len(),
        1,
        "once per turn, not per request: {events:?}"
    );
    assert!(paused[0].contains("session budget"), "{}", paused[0]);
    // The next turn says it again.
    let (_, events) = run(&mut agent, "more").await;
    let again = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::Warning { message } if message.contains("paused")))
        .count();
    assert_eq!(again, 1);
}

#[test]
fn the_messages_name_the_budget_and_how_to_raise_it() {
    let notice = BudgetNotice {
        budget: BudgetKind::Session,
        spent_usd: 1.02,
        limit_usd: 1.0,
    };
    let reached = notice.reached_message();
    assert!(reached.contains("session budget of $1.00"), "{reached}");
    assert!(reached.contains("$1.02"), "{reached}");
    assert!(reached.contains("/budget"), "{reached}");
    assert!(reached.contains("session_usd"), "{reached}");
    let daily = BudgetNotice {
        budget: BudgetKind::Daily,
        ..notice
    };
    assert!(daily.reached_message().contains("daily_usd"));
    let warning = BudgetNotice {
        spent_usd: 0.80,
        ..notice
    };
    assert!(
        warning.warning_message().contains("80%"),
        "{}",
        warning.warning_message()
    );
    assert!(
        warning
            .warning_message()
            .contains("session budget of $1.00")
    );
    let paused = notice.paused_message();
    assert!(paused.contains("paused"), "{paused}");
    assert!(paused.contains("session budget of $1.00"), "{paused}");
    assert!(paused.contains("API"), "{paused}");
}
