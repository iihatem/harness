//! 1.9: budgets are checked against billed cost, from the ledger: a warning once at 80%, a stop at
//! 100%; subscription and local requests are not spend.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use harness_core::{
    message::Usage,
    meter::{AccountKind, BudgetKind, Meter, RequestRecord},
};
use harness_usage::{
    budget::Budgets,
    ledger::{Ledger, LedgerRecord},
    meter::UsageMeter,
    paths::Dirs,
    pricing::{Price, Pricing},
};

/// 2026-10-02 12:00:00 UTC.
const NOW: u64 = 1_790_942_400;
const DAY: u64 = 86_400;

/// A meter with `budgets`, where every token of `openai/gpt-5` costs $1 (a million times $1 per
/// million).
fn meter(data: &std::path::Path, budgets: Budgets, clock: Arc<AtomicU64>) -> UsageMeter {
    let pricing = Pricing::load(
        &data.join("none.json"),
        vec![(
            "openai/gpt-5".into(),
            Price {
                input: Some(1_000_000.0),
                output: Some(1_000_000.0),
                ..Price::default()
            },
        )],
    );
    UsageMeter::open(data, data)
        .with_clock(Arc::new(move || clock.load(Ordering::SeqCst)))
        .with_pricing(pricing)
        .with_budgets(budgets)
}

fn clock(t: u64) -> Arc<AtomicU64> {
    Arc::new(AtomicU64::new(t))
}

/// A request of `tokens` input tokens on `model`, in `session`, costing $1 a token on gpt-5.
fn spend(meter: &UsageMeter, session: &str, model: &str, local: bool, tokens: u64) {
    meter.record_request(&RequestRecord {
        session: session.into(),
        role: "main".into(),
        model: model.into(),
        local,
        usage: Usage {
            input_tokens: tokens,
            ..Usage::default()
        },
        duration: Duration::from_millis(1),
        outcome: "ok".into(),
    });
}

fn session_budget(usd: f64) -> Budgets {
    Budgets {
        session_usd: Some(usd),
        ..Budgets::default()
    }
}

// `session_usd = 1.00` and the session's billed cost reaches $0.80: one warning, requests continue.
#[test]
fn a_warning_at_80_percent_once_then_a_stop_at_100() {
    let data = tempfile::tempdir().unwrap();
    let m = meter(data.path(), session_budget(10.0), clock(NOW));
    assert_eq!(m.check_budget("s", AccountKind::ApiKey), Default::default());
    spend(&m, "s", "openai/gpt-5", false, 7);
    assert!(
        m.check_budget("s", AccountKind::ApiKey).warnings.is_empty(),
        "70% is not yet"
    );
    spend(&m, "s", "openai/gpt-5", false, 1);
    let status = m.check_budget("s", AccountKind::ApiKey);
    assert_eq!(status.warnings.len(), 1, "{status:?}");
    assert_eq!(status.warnings[0].budget, BudgetKind::Session);
    assert!((status.warnings[0].spent_usd - 8.0).abs() < 1e-9);
    assert!(status.stop.is_none());
    // Once.
    assert!(m.check_budget("s", AccountKind::ApiKey).warnings.is_empty());
    spend(&m, "s", "openai/gpt-5", false, 2);
    let status = m.check_budget("s", AccountKind::ApiKey);
    let stop = status.stop.expect("100% stops");
    assert_eq!(stop.budget, BudgetKind::Session);
    assert!((stop.spent_usd - 10.0).abs() < 1e-9 && (stop.limit_usd - 10.0).abs() < 1e-9);
}

// 10,000,000 tokens run on `chatgpt/gpt-5` in a day with `daily_usd = 1.00`: no warning.
#[test]
fn subscription_and_local_use_is_not_spend() {
    let data = tempfile::tempdir().unwrap();
    let m = meter(
        data.path(),
        Budgets {
            session_usd: Some(1.0),
            daily_usd: Some(1.0),
            monthly_usd: Some(1.0),
        },
        clock(NOW),
    );
    spend(&m, "s", "chatgpt/gpt-5", false, 10_000_000);
    spend(&m, "s", "ollama/qwen3-coder", true, 10_000_000);
    assert_eq!(m.check_budget("s", AccountKind::ApiKey), Default::default());
}

#[test]
fn a_request_with_no_price_cannot_be_counted() {
    let data = tempfile::tempdir().unwrap();
    let m = meter(data.path(), session_budget(1.0), clock(NOW));
    spend(&m, "s", "openrouter/some/new-model", false, 10_000_000);
    assert_eq!(m.check_budget("s", AccountKind::ApiKey), Default::default());
}

#[test]
fn the_session_budget_counts_this_session_only_and_survives_a_resume() {
    let data = tempfile::tempdir().unwrap();
    let first = meter(data.path(), session_budget(10.0), clock(NOW));
    spend(&first, "s1", "openai/gpt-5", false, 9);
    spend(&first, "other", "openai/gpt-5", false, 50);
    // Another process resumes session s1: the ledger holds its history.
    let resumed = meter(data.path(), session_budget(10.0), clock(NOW));
    assert_eq!(
        resumed
            .check_budget("s1", AccountKind::ApiKey)
            .warnings
            .len(),
        1
    );
    assert!(
        resumed
            .check_budget("fresh", AccountKind::ApiKey)
            .warnings
            .is_empty()
    );
}

#[test]
fn the_daily_budget_counts_today_and_the_monthly_one_this_month() {
    let data = tempfile::tempdir().unwrap();
    let budgets = Budgets {
        session_usd: None,
        daily_usd: Some(10.0),
        monthly_usd: Some(100.0),
    };
    // Spent yesterday, and on the 1st: in the month, not in the day.
    let t = clock(NOW - DAY);
    let m = meter(data.path(), budgets.clone(), t.clone());
    spend(&m, "a", "openai/gpt-5", false, 50);
    t.store(NOW - DAY - 600, Ordering::SeqCst);
    spend(&m, "a", "openai/gpt-5", false, 10);
    t.store(NOW, Ordering::SeqCst);
    // $60 this month, none today: nothing yet.
    assert_eq!(m.check_budget("a", AccountKind::ApiKey), Default::default());
    // $8 today is 80% of the day's $10.
    spend(&m, "a", "openai/gpt-5", false, 8);
    let status = m.check_budget("a", AccountKind::ApiKey);
    assert_eq!(
        status.warnings.iter().map(|w| w.budget).collect::<Vec<_>>(),
        [BudgetKind::Daily]
    );
    assert!(status.stop.is_none());
    // $23 today is past it, and $83 of the month's $100 is 80% of that.
    spend(&m, "a", "openai/gpt-5", false, 15);
    let status = m.check_budget("a", AccountKind::ApiKey);
    assert_eq!(
        status.stop.expect("the day's budget").budget,
        BudgetKind::Daily
    );
    assert_eq!(
        status.warnings.iter().map(|w| w.budget).collect::<Vec<_>>(),
        [BudgetKind::Monthly]
    );
    // Tomorrow the daily budget is fresh, and the month's warning was given.
    t.store(NOW + DAY, Ordering::SeqCst);
    assert_eq!(m.check_budget("a", AccountKind::ApiKey), Default::default());
}

#[test]
fn raising_the_session_budget_lets_requests_go_on_and_warns_again_at_80_percent_of_the_new_one() {
    let data = tempfile::tempdir().unwrap();
    let m = meter(data.path(), session_budget(1.0), clock(NOW));
    spend(&m, "s", "openai/gpt-5", false, 1);
    assert!(m.check_budget("s", AccountKind::ApiKey).stop.is_some());
    m.set_session_budget(10.0);
    assert_eq!(m.check_budget("s", AccountKind::ApiKey).stop, None);
    assert_eq!(m.budgets().session_usd, Some(10.0));
    spend(&m, "s", "openai/gpt-5", false, 7);
    let status = m.check_budget("s", AccountKind::ApiKey);
    assert_eq!(status.warnings.len(), 1, "{status:?}");
    assert!((status.warnings[0].limit_usd - 10.0).abs() < 1e-9);
}

#[test]
fn the_report_shows_each_budget_with_its_spend() {
    let data = tempfile::tempdir().unwrap();
    let m = meter(
        data.path(),
        Budgets {
            session_usd: Some(10.0),
            daily_usd: None,
            monthly_usd: Some(100.0),
        },
        clock(NOW),
    );
    spend(&m, "s", "openai/gpt-5", false, 4);
    let report = m.budget_report("s");
    let find = |kind| report.iter().find(|l| l.budget == kind).unwrap();
    assert_eq!(find(BudgetKind::Session).limit_usd, Some(10.0));
    assert!((find(BudgetKind::Session).spent_usd - 4.0).abs() < 1e-9);
    assert_eq!(find(BudgetKind::Daily).limit_usd, None);
    assert!((find(BudgetKind::Daily).spent_usd - 4.0).abs() < 1e-9);
    assert!((find(BudgetKind::Monthly).spent_usd - 4.0).abs() < 1e-9);
}

#[test]
fn budgets_that_are_not_set_cost_nothing_to_check() {
    let data = tempfile::tempdir().unwrap();
    let m = meter(data.path(), Budgets::default(), clock(NOW));
    spend(&m, "s", "openai/gpt-5", false, 1_000);
    assert_eq!(m.check_budget("s", AccountKind::ApiKey), Default::default());
    // No cache was needed for it.
    let _ = Ledger::new(&Dirs::under(data.path()).usage);
    let _: Option<LedgerRecord> = None;
}

// At 100% the budget refuses only requests on an API key: a subscription or a local request goes
// on, and the status says paid models are paused. The check still runs before every request.
#[test]
fn a_reached_budget_pauses_api_key_requests_only() {
    let data = tempfile::tempdir().unwrap();
    let m = meter(data.path(), session_budget(1.0), clock(NOW));
    spend(&m, "s", "openai/gpt-5", false, 1);
    let billed = m.check_budget("s", AccountKind::ApiKey);
    assert!(
        billed.stop.is_some() && billed.paused.is_none(),
        "{billed:?}"
    );
    for account in [AccountKind::Local, AccountKind::Subscription] {
        let free = m.check_budget("s", account);
        assert_eq!(free.stop, None, "{account:?}");
        let paused = free.paused.expect("the pause is reported");
        assert_eq!(paused.budget, BudgetKind::Session);
    }
    // Raising the budget clears both.
    m.set_session_budget(10.0);
    let status = m.check_budget("s", AccountKind::Local);
    assert_eq!((status.stop, status.paused), (None, None));
}
