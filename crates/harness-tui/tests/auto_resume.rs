//! 1.10: when a subscription limit ends a turn, the session offers once to resume at the reset;
//! a countdown that Esc cancels; the window is read again at the reset, and the message is sent
//! only if it shows capacity; at most two re-arms; never when `usage.auto_resume = "never"`.

mod common;

use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use common::*;
use futures::future::BoxFuture;
use harness_core::{
    event::{AgentEvent, TurnEndReason},
    message::ChatRequest,
    meter::{Window, WindowSnapshot, WindowSource},
    permission::Mode,
    provider::{FinishReason, Provider, ProviderEvent, ProviderStream},
};
use harness_tui::{
    app::{Host, Prepared},
    ui::Ui,
    usage::{AutoResume, UsageContext},
};
use ratatui::{backend::TestBackend, crossterm::event::KeyCode};

/// 2026-10-02 14:00:00 UTC; the limit resets at 14:30.
const NOW: u64 = 1_790_949_600;
const RESET: u64 = NOW + 1_800;

/// A provider that answers `ok`, counts its requests, and says where the windows stand from a
/// queue, one answer for each time it is asked.
struct Account {
    requests: AtomicUsize,
    polls: Mutex<VecDeque<WindowSnapshot>>,
    asked: AtomicUsize,
}

impl Account {
    fn new(polls: Vec<f64>) -> Arc<Account> {
        Arc::new(Account {
            requests: AtomicUsize::new(0),
            polls: Mutex::new(
                polls
                    .into_iter()
                    .map(|used| snapshot(used, RESET))
                    .collect(),
            ),
            asked: AtomicUsize::new(0),
        })
    }
}

fn snapshot(used: f64, resets_at: u64) -> WindowSnapshot {
    WindowSnapshot {
        windows: vec![Window {
            window_minutes: Some(300),
            used_percent: Some(used),
            resets_at: Some(resets_at),
            source: WindowSource::Poll,
        }],
        observed_at: NOW,
    }
}

impl Provider for Account {
    fn stream(&self, _request: ChatRequest) -> ProviderStream {
        self.requests.fetch_add(1, Ordering::SeqCst);
        Box::pin(futures::stream::iter(vec![
            Ok(ProviderEvent::TextDelta("resumed".into())),
            Ok(ProviderEvent::Finished(FinishReason::Stop)),
        ]))
    }

    fn windows(&self) -> Option<BoxFuture<'static, Result<WindowSnapshot, String>>> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        let next = self.polls.lock().unwrap().pop_front();
        Some(Box::pin(async move {
            next.ok_or_else(|| "no more answers".to_string())
        }))
    }
}

struct NoCommands;

impl Host for NoCommands {
    fn is_command(&self, _name: &str) -> bool {
        false
    }
    fn prepare(&mut self, _typed: &str) -> Prepared {
        unreachable!()
    }
}

struct Session {
    _dir: tempfile::TempDir,
    ui: Ui<TestBackend>,
    account: Arc<Account>,
    clock: Arc<AtomicU64>,
}

fn session(polls: Vec<f64>, auto_resume: AutoResume) -> Session {
    let dir = tempfile::tempdir().unwrap();
    let account = Account::new(polls);
    let agent = agent(account.clone(), dir.path(), Mode::Auto);
    let mut options = options(dir.path(), Mode::Auto);
    options.model = "chatgpt/gpt-5".into();
    let (mut ui, _) = start(agent, Box::new(NoCommands), options);
    let clock = Arc::new(AtomicU64::new(NOW));
    let reader = clock.clone();
    ui.app_mut()
        .set_clock(Arc::new(move || reader.load(Ordering::SeqCst)));
    ui.app_mut().set_usage_context(UsageContext {
        auto_resume,
        ..UsageContext::default()
    });
    Session {
        _dir: dir,
        ui,
        account,
        clock,
    }
}

impl Session {
    /// The usage limit ends a turn, as the runtime reports it.
    fn limit_ends_a_turn(&mut self, resets_at: u64) {
        let app = self.ui.app_mut();
        app.on_event(&AgentEvent::TurnStarted);
        app.on_event(&AgentEvent::LimitReached { resets_at });
        app.on_event(&AgentEvent::Error {
            kind: harness_core::event::ErrorKind::Provider,
            message: "the provider's usage limit is reached".into(),
        });
        app.on_event(&AgentEvent::TurnFinished {
            reason: TurnEndReason::Error,
        });
        self.ui.draw().unwrap();
    }

    fn key(&mut self, code: KeyCode) {
        press(&mut self.ui, code);
    }

    /// Lets the session run for `ms` of (paused) time, taking in what comes.
    async fn run_for(&mut self, ms: u64) {
        let until = tokio::time::Instant::now() + Duration::from_millis(ms);
        while tokio::time::Instant::now() < until {
            let _ = tokio::time::timeout(Duration::from_millis(300), self.ui.next()).await;
        }
        self.ui.draw().unwrap();
    }

    fn shows(&self, text: &str) -> bool {
        shows_wrapped(&self.ui, text)
    }
}

// The 5h window is exhausted with reset at 14:30: the offer is made once.
#[tokio::test(start_paused = true)]
async fn the_offer_names_the_reset_time_and_is_answered_with_y_or_n() {
    let mut s = session(vec![0.0], AutoResume::Ask);
    s.limit_ends_a_turn(RESET);
    assert!(
        s.shows("Resume automatically at 14:30 UTC? (y/n)"),
        "{:#?}",
        everything(&s.ui)
    );
    // Nothing is waiting until the user answers.
    assert!(!s.shows("Resuming automatically"));
    s.key(KeyCode::Char('n'));
    assert!(!s.shows("Resuming automatically"));
    s.ui.draw().unwrap();
    // Remembered for the session: no second offer.
    s.limit_ends_a_turn(RESET);
    let offers = everything(&s.ui)
        .iter()
        .filter(|r| r.contains("Resume automatically at"))
        .count();
    assert_eq!(offers, 1, "{:#?}", everything(&s.ui));
}

// The user answers `y`, and at 14:30 the window shows 0% used: a countdown, then the message.
#[tokio::test(start_paused = true)]
async fn an_accepted_offer_counts_down_and_resumes_when_the_window_has_capacity() {
    // The first answer is for the poll as the session starts, the second for the reset.
    let mut s = session(vec![0.0, 0.0], AutoResume::Ask);
    s.limit_ends_a_turn(RESET);
    s.key(KeyCode::Char('y'));
    s.ui.draw().unwrap();
    assert!(
        s.shows("Resuming automatically at 14:30 UTC"),
        "{:#?}",
        screen(&s.ui)
    );
    assert!(s.shows("Esc to cancel"), "{:#?}", screen(&s.ui));
    // Not before the reset.
    s.clock.store(RESET - 60, Ordering::SeqCst);
    s.run_for(600).await;
    assert_eq!(
        s.account.asked.load(Ordering::SeqCst),
        1,
        "only the poll at session start"
    );
    assert_eq!(s.account.requests.load(Ordering::SeqCst), 0);
    // At the reset the window is read again, and the fixed message is sent as a user message.
    s.clock.store(RESET, Ordering::SeqCst);
    s.run_for(1_000).await;
    s.ui.settle().await.unwrap();
    assert!(
        s.shows("Continue where you left off."),
        "{:#?}",
        everything(&s.ui)
    );
    assert_eq!(s.account.requests.load(Ordering::SeqCst), 1);
    assert!(s.shows("resumed"), "{:#?}", everything(&s.ui));
}

#[tokio::test(start_paused = true)]
async fn esc_during_the_countdown_cancels_it_and_nothing_is_sent() {
    let mut s = session(vec![0.0], AutoResume::Ask);
    s.limit_ends_a_turn(RESET);
    s.key(KeyCode::Char('y'));
    s.key(KeyCode::Esc);
    s.ui.draw().unwrap();
    assert!(!s.shows("Esc to cancel"), "{:#?}", screen(&s.ui));
    s.clock.store(RESET + 10, Ordering::SeqCst);
    s.run_for(2_000).await;
    assert_eq!(s.account.requests.load(Ordering::SeqCst), 0);
    assert!(!s.shows("Continue where you left off."));
    assert!(!s.ui.app().busy());
}

// The window still shows 100% used at the reset time: the countdown re-arms, and after the
// second re-arm it stops without sending.
#[tokio::test(start_paused = true)]
async fn a_window_that_is_still_full_re_arms_twice_and_then_gives_up() {
    let mut s = session(vec![0.0, 100.0, 100.0, 100.0], AutoResume::Ask);
    s.limit_ends_a_turn(RESET);
    s.key(KeyCode::Char('y'));
    let mut at = RESET;
    for _ in 0..3 {
        s.clock.store(at, Ordering::SeqCst);
        s.run_for(1_000).await;
        at += 600;
        s.clock.store(at, Ordering::SeqCst);
    }
    s.run_for(1_000).await;
    assert_eq!(s.account.requests.load(Ordering::SeqCst), 0);
    assert!(s.shows("still full"), "{:#?}", everything(&s.ui));
    assert!(!s.shows("Esc to cancel"), "{:#?}", screen(&s.ui));
    // Three readings after the one at the start: the reset and two re-arms.
    assert_eq!(s.account.asked.load(Ordering::SeqCst), 4);
    // And no more.
    s.clock.store(at + 10_000, Ordering::SeqCst);
    s.run_for(2_000).await;
    assert_eq!(s.account.asked.load(Ordering::SeqCst), 4);
}

#[tokio::test(start_paused = true)]
async fn a_window_that_is_full_at_first_and_free_on_a_re_arm_resumes() {
    let mut s = session(vec![0.0, 100.0, 5.0], AutoResume::Ask);
    s.limit_ends_a_turn(RESET);
    s.key(KeyCode::Char('y'));
    s.clock.store(RESET, Ordering::SeqCst);
    s.run_for(1_000).await;
    assert_eq!(s.account.requests.load(Ordering::SeqCst), 0);
    s.clock.store(RESET + 600, Ordering::SeqCst);
    s.run_for(1_000).await;
    s.ui.settle().await.unwrap();
    assert_eq!(s.account.requests.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn with_never_no_offer_is_made() {
    let mut s = session(vec![0.0], AutoResume::Never);
    s.limit_ends_a_turn(RESET);
    assert!(!s.shows("Resume automatically"), "{:#?}", everything(&s.ui));
    assert!(!s.shows("(y/n)"));
}

#[tokio::test(start_paused = true)]
async fn an_answer_of_yes_is_remembered_and_the_next_limit_counts_down_at_once() {
    let mut s = session(vec![0.0, 0.0, 0.0], AutoResume::Ask);
    s.limit_ends_a_turn(RESET);
    s.key(KeyCode::Char('y'));
    s.clock.store(RESET, Ordering::SeqCst);
    s.run_for(1_000).await;
    s.ui.settle().await.unwrap();
    // Another limit later: no question this time.
    s.clock.store(RESET + 100, Ordering::SeqCst);
    s.limit_ends_a_turn(RESET + 18_000);
    assert!(
        s.shows("Resuming automatically at 19:30 UTC"),
        "{:#?}",
        screen(&s.ui)
    );
    let offers = everything(&s.ui)
        .iter()
        .filter(|r| r.contains("(y/n)"))
        .count();
    assert_eq!(offers, 1);
}

#[tokio::test(start_paused = true)]
async fn sending_a_message_cancels_the_countdown() {
    let mut s = session(vec![0.0], AutoResume::Ask);
    s.limit_ends_a_turn(RESET);
    s.key(KeyCode::Char('y'));
    type_text(&mut s.ui, "something else");
    press(&mut s.ui, KeyCode::Enter);
    s.ui.settle().await.unwrap();
    assert!(!s.shows("Esc to cancel"), "{:#?}", screen(&s.ui));
    s.clock.store(RESET + 10, Ordering::SeqCst);
    s.run_for(2_000).await;
    assert!(!s.shows("Continue where you left off."));
}

#[tokio::test(start_paused = true)]
async fn a_limit_with_nothing_to_wait_for_makes_no_offer() {
    // The reset time has passed already.
    let mut s = session(vec![0.0], AutoResume::Ask);
    s.limit_ends_a_turn(NOW - 10);
    assert!(!s.shows("Resume automatically"), "{:#?}", everything(&s.ui));
    // A turn that ended another way makes none either.
    let app = s.ui.app_mut();
    app.on_event(&AgentEvent::TurnStarted);
    app.on_event(&AgentEvent::TurnFinished {
        reason: TurnEndReason::Completed,
    });
    s.ui.draw().unwrap();
    assert!(!s.shows("Resume automatically"));
}
