//! 1.9: `/budget` shows the budgets and raises the session's, between turns; a budget warning and
//! a budget stop say which budget and how to raise it.

mod common;

use std::sync::{Arc, Mutex};

use common::*;
use futures::future::BoxFuture;
use harness_core::{
    event::{AgentEvent, TurnEndReason},
    meter::{BudgetKind, BudgetNotice},
    permission::Mode,
    testing::{MockProvider, Script},
};
use harness_tui::app::{Host, Prepared};
use ratatui::{backend::TestBackend, crossterm::event::KeyCode};

#[derive(Default)]
struct Calls(
    Mutex<Vec<(String, Option<f64>)>>,
    std::sync::atomic::AtomicUsize,
);

struct Budgets(Arc<Calls>);

impl Host for Budgets {
    fn is_command(&self, _name: &str) -> bool {
        false
    }
    fn prepare(&mut self, _typed: &str) -> Prepared {
        unreachable!()
    }
    fn open_session(
        &self,
        _id: Option<&str>,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> BoxFuture<'static, Result<harness_tui::app::OpenedSession, String>> {
        let dir = tempfile::tempdir().unwrap().keep();
        Box::pin(async move {
            Ok(harness_tui::app::OpenedSession {
                session: harness_core::session::Session::create(&dir, &dir),
                checkpoints: None,
                warnings: Vec::new(),
            })
        })
    }
    fn session_changed(&self) {
        self.0.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
    fn budget(
        &self,
        session: &str,
        set: Option<f64>,
    ) -> BoxFuture<'static, Result<Vec<String>, String>> {
        self.0.0.lock().unwrap().push((session.to_string(), set));
        Box::pin(async move {
            Ok(match set {
                Some(usd) => vec![format!("session budget set to ${usd:.2} for this session")],
                None => vec![
                    "session   $0.42 of $1.00 (42%)".to_string(),
                    "daily     $3.10, no limit".to_string(),
                    "monthly   $9.00, no limit".to_string(),
                ],
            })
        })
    }
}

fn session(script: Vec<Script>) -> (tempfile::TempDir, Ui, Arc<Calls>) {
    let dir = tempfile::tempdir().unwrap();
    let agent = agent(MockProvider::new(script), dir.path(), Mode::Auto);
    let calls = Arc::new(Calls::default());
    let (ui, _) = start(
        agent,
        Box::new(Budgets(calls.clone())),
        options(dir.path(), Mode::Auto),
    );
    (dir, ui, calls)
}

type Ui = harness_tui::ui::Ui<TestBackend>;

async fn wait_for(ui: &mut Ui, text: &str) {
    for _ in 0..50 {
        if shows(ui, text) {
            return;
        }
        let _ = tokio::time::timeout(std::time::Duration::from_millis(100), ui.next()).await;
    }
    panic!("never showed {text:?}: {:#?}", everything(ui));
}

#[tokio::test(flavor = "multi_thread")]
async fn budget_shows_each_budget_with_its_spend() {
    let (_dir, mut ui, calls) = session(vec![]);
    type_text(&mut ui, "/budget");
    press(&mut ui, KeyCode::Enter);
    wait_for(&mut ui, "42%").await;
    assert!(shows(&ui, "daily"), "{:#?}", everything(&ui));
    let calls = calls.0.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1, None);
    assert!(!calls[0].0.is_empty(), "the session's id goes with it");
}

#[tokio::test(flavor = "multi_thread")]
async fn budget_with_an_amount_raises_the_session_budget() {
    let (_dir, mut ui, calls) = session(vec![]);
    type_text(&mut ui, "/budget $2.00");
    press(&mut ui, KeyCode::Enter);
    wait_for(&mut ui, "session budget set to $2.00").await;
    assert_eq!(calls.0.lock().unwrap()[0].1, Some(2.0));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_amount_that_is_not_a_positive_number_is_refused() {
    let (_dir, mut ui, calls) = session(vec![]);
    for bad in ["lots", "0", "-3", "1e999"] {
        type_text(&mut ui, &format!("/budget {bad}"));
        press(&mut ui, KeyCode::Enter);
    }
    assert!(
        shows_wrapped(&ui, "/budget takes an amount in USD"),
        "{:#?}",
        everything(&ui)
    );
    assert!(calls.0.lock().unwrap().is_empty());
}

// Typed during a turn, it waits in the input rather than change the running turn.
#[tokio::test(flavor = "multi_thread")]
async fn budget_works_between_turns() {
    let (_dir, mut ui, calls) = session(vec![Script::Hang(vec![])]);
    type_text(&mut ui, "go");
    press(&mut ui, KeyCode::Enter);
    type_text(&mut ui, "/budget 2");
    press(&mut ui, KeyCode::Enter);
    assert_eq!(ui.app().editor().text(), "/budget 2");
    assert!(
        shows_wrapped(&ui, "/budget works between turns"),
        "{:#?}",
        everything(&ui)
    );
    assert!(calls.0.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_budget_warning_and_a_stop_say_which_budget_and_how_to_raise_it() {
    let (_dir, mut ui, _) = session(vec![]);
    let notice = |spent| BudgetNotice {
        budget: BudgetKind::Session,
        spent_usd: spent,
        limit_usd: 1.0,
    };
    ui.app_mut().on_event(&AgentEvent::BudgetWarning {
        notice: notice(0.8),
    });
    ui.app_mut().on_event(&AgentEvent::BudgetReached {
        notice: notice(1.02),
    });
    ui.app_mut().on_event(&AgentEvent::TurnFinished {
        reason: TurnEndReason::Budget,
    });
    ui.draw().unwrap();
    assert!(
        shows_wrapped(&ui, "80% of the session budget of $1.00"),
        "{:#?}",
        everything(&ui)
    );
    assert!(
        shows_wrapped(&ui, "the session budget of $1.00 is reached"),
        "{:#?}",
        everything(&ui)
    );
    assert!(
        shows_wrapped(&ui, "/budget <usd>"),
        "{:#?}",
        everything(&ui)
    );
    assert!(shows_wrapped(&ui, "session_usd"), "{:#?}", everything(&ui));
    // The session accepts a new message after a budget stop.
    assert!(!ui.app().busy());
}

// B6: a figure given with `/budget <usd>` is for the session it was given in: the host is told
// when `/new` or `/resume` moves to another, to return to the configured one.
#[tokio::test(flavor = "multi_thread")]
async fn a_new_session_tells_the_host_so_the_budget_can_be_reset() {
    let (_dir, mut ui, calls) = session(vec![]);
    type_text(&mut ui, "/budget 5");
    press(&mut ui, KeyCode::Enter);
    wait_for(&mut ui, "session budget set to $5.00").await;
    assert_eq!(calls.1.load(std::sync::atomic::Ordering::SeqCst), 0);
    type_text(&mut ui, "/new");
    press(&mut ui, KeyCode::Enter);
    for _ in 0..50 {
        if calls.1.load(std::sync::atomic::Ordering::SeqCst) > 0 {
            break;
        }
        let _ = tokio::time::timeout(std::time::Duration::from_millis(100), ui.next()).await;
    }
    assert_eq!(calls.1.load(std::sync::atomic::Ordering::SeqCst), 1);
}
