//! Delegated runs (Claude Code, Codex) in the ledger: their own account kind, the program's
//! estimate as the list figure, never a bill on a subscription, and a bill on an API key.

use std::{sync::Arc, time::Duration};

use harness_core::{
    message::Usage,
    meter::{AccountKind, Avoided, BudgetStatus, Meter, RequestDetail, RequestRecord},
};
use harness_usage::{
    budget::Budgets, ledger::Ledger, meter::UsageMeter, paths::Dirs, pricing::Pricing,
};

const OCTOBER: u64 = 1_790_942_400;

fn meter(data: &std::path::Path) -> UsageMeter {
    UsageMeter::open(data, data)
        .with_clock(Arc::new(|| OCTOBER))
        .with_pricing(Pricing::load(&data.join("none.json"), vec![]))
}

fn request(model: &str) -> RequestRecord {
    RequestRecord {
        session: "s".into(),
        role: "main".into(),
        model: model.into(),
        local: false,
        usage: Usage {
            input_tokens: 1_000,
            output_tokens: 100,
            ..Usage::default()
        },
        duration: Duration::from_millis(5),
        outcome: "ok".into(),
    }
}

fn delegated(estimate: Option<f64>) -> RequestDetail {
    RequestDetail {
        account: Some(AccountKind::Delegated),
        estimated_usd: estimate,
    }
}

#[test]
fn a_delegated_run_is_billed_nothing_and_the_programs_estimate_is_the_list_figure() {
    let data = tempfile::tempdir().unwrap();
    let cost = meter(data.path()).record_request_detailed(
        &request("claude-code/sonnet"),
        None,
        &delegated(Some(0.37)),
    );
    assert_eq!(cost.account, AccountKind::Delegated);
    assert_eq!(cost.billed_usd, Some(0.0));
    assert_eq!(cost.list_usd, Some(0.37));
    assert_eq!(cost.avoided, Avoided::NotApplicable);
    let record = &Ledger::new(&Dirs::under(data.path()).usage).read()[0];
    assert_eq!(record.account, AccountKind::Delegated);
    assert_eq!(record.billed_usd, Some(0.0));
    assert_eq!(record.list_usd, Some(0.37));
    assert!(record.price.is_none(), "no table priced it: {record:?}");
    let line = serde_json::to_string(record).unwrap();
    assert!(line.contains("\"account\":\"delegated\""), "{line}");
}

#[test]
fn a_run_with_no_estimate_and_no_price_is_unpriced_never_zero() {
    let data = tempfile::tempdir().unwrap();
    let cost =
        meter(data.path()).record_request_detailed(&request("codex/gpt-5"), None, &delegated(None));
    assert_eq!(cost.account, AccountKind::Delegated);
    assert_eq!(cost.billed_usd, Some(0.0));
    assert_eq!(cost.list_usd, None, "price unknown, not $0");
}

#[test]
fn a_delegated_run_on_an_api_key_is_billed_the_estimate_and_counts_for_a_budget() {
    let data = tempfile::tempdir().unwrap();
    let meter = meter(data.path()).with_budgets(Budgets {
        session_usd: Some(1.0),
        daily_usd: None,
        monthly_usd: None,
    });
    let detail = RequestDetail {
        account: Some(AccountKind::ApiKey),
        estimated_usd: Some(1.5),
    };
    let cost = meter.record_request_detailed(&request("claude-code/opus"), None, &detail);
    assert_eq!(cost.account, AccountKind::ApiKey);
    assert_eq!(cost.billed_usd, Some(1.5));
    assert_eq!(cost.list_usd, Some(1.5));
    let status: BudgetStatus = meter.check_budget("s", AccountKind::ApiKey);
    assert!(status.stop.is_some(), "the budget is reached: {status:?}");
    // A subscription run is not a paid request: paused, not stopped.
    let status = meter.check_budget("s", AccountKind::Delegated);
    assert!(
        status.stop.is_none() && status.paused.is_some(),
        "{status:?}"
    );
}

#[test]
fn a_subscription_run_adds_nothing_to_a_budget() {
    let data = tempfile::tempdir().unwrap();
    let meter = meter(data.path()).with_budgets(Budgets {
        session_usd: Some(1.0),
        daily_usd: None,
        monthly_usd: None,
    });
    meter.record_request_detailed(&request("claude-code/opus"), None, &delegated(Some(50.0)));
    let status = meter.check_budget("s", AccountKind::ApiKey);
    assert!(
        status.stop.is_none() && status.paused.is_none(),
        "{status:?}"
    );
}

#[test]
fn without_detail_a_delegated_models_id_still_says_delegated() {
    let data = tempfile::tempdir().unwrap();
    let cost = meter(data.path()).record_request(&request("codex/gpt-5"));
    assert_eq!(cost.account, AccountKind::Delegated);
    assert_eq!(cost.billed_usd, Some(0.0));
}
