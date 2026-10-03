//! 1.6: three cost figures, kept apart: billed, the list-price estimate and, against a named
//! baseline, what was avoided.

use std::{sync::Arc, time::Duration};

use harness_core::{
    message::Usage,
    meter::{AccountKind, Avoided, Meter, RequestRecord},
};
use harness_usage::{
    ledger::Ledger,
    meter::UsageMeter,
    paths::Dirs,
    pricing::{Price, Pricing},
    store::{Group, Query, Store, avoided_line},
};

const OCTOBER: u64 = 1_790_942_400;

fn price(input: f64, output: f64) -> Price {
    Price {
        input: Some(input),
        output: Some(output),
        ..Price::default()
    }
}

/// A meter whose table prices `openai/gpt-5` at $1.25 in and $10 out per million tokens.
fn meter(data: &std::path::Path, baseline: Option<&str>) -> UsageMeter {
    let pricing = Pricing::load(
        &data.join("none.json"),
        vec![("openai/gpt-5".into(), price(1.25, 10.0))],
    );
    UsageMeter::open(data, data)
        .with_clock(Arc::new(|| OCTOBER))
        .with_pricing(pricing)
        .with_baseline(baseline.map(String::from))
}

fn request(model: &str, local: bool, input: u64, output: u64) -> RequestRecord {
    RequestRecord {
        session: "s".into(),
        role: "main".into(),
        model: model.into(),
        local,
        usage: Usage {
            input_tokens: input,
            output_tokens: output,
            ..Usage::default()
        },
        duration: Duration::from_millis(5),
        outcome: "ok".into(),
    }
}

// A session sends 1,000,000 uncached input tokens through `chatgpt/gpt-5`, which the table prices
// at $1.25: billed is $0.00, the list-price estimate is $1.25, and no avoided figure is shown.
#[test]
fn a_subscription_request_is_billed_nothing_and_estimated_at_list_price() {
    let data = tempfile::tempdir().unwrap();
    let cost =
        meter(data.path(), None).record_request(&request("chatgpt/gpt-5", false, 1_000_000, 0));
    assert_eq!(cost.account, AccountKind::Subscription);
    assert_eq!(cost.billed_usd, Some(0.0));
    assert!((cost.list_usd.unwrap() - 1.25).abs() < 1e-9);
    assert_eq!(cost.avoided, Avoided::NotApplicable);
    let record = &Ledger::new(&Dirs::under(data.path()).usage).read()[0];
    assert_eq!(record.billed_usd, Some(0.0));
    assert!((record.list_usd.unwrap() - 1.25).abs() < 1e-9);
    assert!(
        record.price.as_deref().unwrap().starts_with("override"),
        "{:?}",
        record.price
    );
}

#[test]
fn an_api_key_request_is_billed_at_the_table_price() {
    let data = tempfile::tempdir().unwrap();
    let cost = meter(data.path(), None).record_request(&request(
        "openai/gpt-5",
        false,
        1_000_000,
        100_000,
    ));
    assert_eq!(cost.account, AccountKind::ApiKey);
    assert!((cost.billed_usd.unwrap() - 2.25).abs() < 1e-9);
    assert_eq!(cost.billed_usd, cost.list_usd);
}

// `usage.baseline = "openai/gpt-5"` and 1,000,000 input tokens on `ollama/qwen3-coder`: avoided
// $1.25, labelled with the baseline.
#[test]
fn a_named_baseline_gives_the_avoided_figure_for_local_and_subscription_tokens() {
    let data = tempfile::tempdir().unwrap();
    let m = meter(data.path(), Some("openai/gpt-5"));
    let local = m.record_request(&request("ollama/qwen3-coder", true, 1_000_000, 0));
    assert_eq!(local.billed_usd, Some(0.0));
    assert_eq!(local.list_usd, Some(0.0));
    let Avoided::Usd(avoided) = local.avoided else {
        panic!("{:?}", local.avoided)
    };
    assert!((avoided - 1.25).abs() < 1e-9);
    let subscription = m.record_request(&request("chatgpt/gpt-5", false, 1_000_000, 0));
    assert!(matches!(subscription.avoided, Avoided::Usd(a) if (a - 1.25).abs() < 1e-9));
    // Tokens bought with an API key were not avoided.
    let paid = m.record_request(&request("openai/gpt-5", false, 1_000_000, 0));
    assert_eq!(paid.avoided, Avoided::NotApplicable);
}

#[test]
fn a_baseline_with_no_price_is_unknown_not_zero() {
    let data = tempfile::tempdir().unwrap();
    let m = meter(data.path(), Some("openrouter/no/such-model"));
    let cost = m.record_request(&request("ollama/qwen3-coder", true, 1_000_000, 0));
    assert_eq!(cost.avoided, Avoided::Unknown);
}

#[test]
fn a_model_with_no_price_has_a_null_cost_and_local_models_cost_nothing() {
    let data = tempfile::tempdir().unwrap();
    let m = meter(data.path(), None);
    let cost = m.record_request(&request("openrouter/some/new-model", false, 1_000, 10));
    assert_eq!((cost.billed_usd, cost.list_usd), (None, None));
    let records = Ledger::new(&Dirs::under(data.path()).usage).read();
    assert_eq!(
        (
            records[0].billed_usd,
            records[0].list_usd,
            records[0].price.clone()
        ),
        (None, None, None)
    );
    let local = m.record_request(&request("ollama/qwen3-coder", true, 1_000, 10));
    assert_eq!((local.billed_usd, local.list_usd), (Some(0.0), Some(0.0)));
    // A subscription model with no counterpart in the table has an unknown list price, and its
    // billed cost is still 0.
    let sub = m.record_request(&request("chatgpt/not-a-model", false, 1_000, 10));
    assert_eq!((sub.billed_usd, sub.list_usd), (Some(0.0), None));
}

#[test]
fn a_request_that_sent_no_tokens_cost_nothing_even_when_unpriced() {
    let data = tempfile::tempdir().unwrap();
    let m = meter(data.path(), None);
    let mut failed = request("openrouter/some/new-model", false, 0, 0);
    failed.outcome = "error:unavailable".into();
    let cost = m.record_request(&failed);
    assert_eq!((cost.billed_usd, cost.list_usd), (Some(0.0), Some(0.0)));
}

// A session mixing local, subscription and API-key turns reports each figure as specified.
#[test]
fn a_mixed_history_keeps_the_three_figures_apart() {
    let data = tempfile::tempdir().unwrap();
    let dirs = Dirs::under(data.path());
    let m = meter(data.path(), Some("openai/gpt-5"));
    m.record_request(&request("ollama/qwen3-coder", true, 1_000_000, 0));
    m.record_request(&request("chatgpt/gpt-5", false, 1_000_000, 0));
    m.record_request(&request("openai/gpt-5", false, 1_000_000, 0));
    let mut store = Store::open(&dirs).unwrap();
    store.sync().unwrap();
    let report = store.report(&Query::all(Group::Model)).unwrap();
    // Billed: only the API-key request.
    assert!(
        (report.total.billed.usd - 1.25).abs() < 1e-9,
        "{:?}",
        report.total
    );
    // List-price estimate: every hosted request at the table price (the local one is 0).
    assert!(
        (report.total.list.usd - 2.5).abs() < 1e-9,
        "{:?}",
        report.total
    );
    // Avoided: the baseline's price for the tokens that ran on local or subscription models.
    let pricing = Pricing::load(
        &data.path().join("none.json"),
        vec![("openai/gpt-5".into(), price(1.25, 10.0))],
    );
    let avoided = report.avoided(&pricing, Some("openai/gpt-5"));
    assert!(
        matches!(avoided, Avoided::Usd(a) if (a - 2.5).abs() < 1e-9),
        "{avoided:?}"
    );
    // The figures are never added: billed + list + avoided is not a number anything shows.
    assert_eq!(report.avoided(&pricing, None), Avoided::NotApplicable);
    assert_eq!(
        report.avoided(&pricing, Some("nobody/none")),
        Avoided::Unknown
    );
}

#[test]
fn the_avoided_line_names_the_baseline_and_never_shows_zero_for_unknown() {
    assert_eq!(
        avoided_line("openai/gpt-5", &Avoided::Usd(1.25)),
        Some("Avoided vs openai/gpt-5: $1.25 (tokens that ran on local or subscription models, at the baseline's list price; plan and hardware fees not counted)".to_string())
    );
    let unknown = avoided_line("x/y", &Avoided::Unknown).unwrap();
    assert!(unknown.contains("price unknown"), "{unknown}");
    assert!(!unknown.contains("$0"), "{unknown}");
    assert_eq!(avoided_line("x/y", &Avoided::NotApplicable), None);
}
