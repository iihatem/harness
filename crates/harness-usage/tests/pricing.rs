//! 1.5: prices come from a shipped snapshot, a downloaded table and the user's overrides, in
//! that order of precedence; a model with no price has no cost.

use harness_core::message::Usage;
use harness_usage::pricing::{Price, PriceTable, Pricing, Source, embedded, trim_models_dev};

fn price(input: f64, output: f64) -> Price {
    Price {
        input: Some(input),
        output: Some(output),
        ..Price::default()
    }
}

fn pricing_with(
    downloaded: Option<&str>,
    overrides: Vec<(&str, Price)>,
) -> (tempfile::TempDir, Pricing) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pricing.json");
    if let Some(text) = downloaded {
        std::fs::write(&path, text).unwrap();
    }
    let overrides = overrides
        .into_iter()
        .map(|(g, p)| (g.to_string(), p))
        .collect();
    (dir, Pricing::load(&path, overrides))
}

const DOWNLOADED: &str = r#"{"date":"2026-10-03","source":"x","providers":{"openai":{"gpt-5":{"input":1.0,"output":8.0}}}}"#;

#[test]
fn the_shipped_snapshot_covers_the_supported_providers_and_says_its_date() {
    let table = embedded();
    assert_eq!(table.date.len(), 10, "{}", table.date);
    for provider in ["openai", "anthropic", "openrouter"] {
        assert!(!table.providers[provider].is_empty(), "{provider}");
    }
    let gpt5 = &table.providers["openai"]["gpt-5"];
    assert_eq!(gpt5.input, Some(1.25));
    assert_eq!(gpt5.output, Some(10.0));
    // Trimmed to what costs need: well under 100 KiB in the binary.
    assert!(harness_usage::pricing::EMBEDDED_JSON.len() < 100 * 1024);
}

#[test]
fn a_price_applies_to_the_disjoint_buckets_without_pricing_reasoning_twice() {
    // 10,000 prompt tokens of which 8,000 cached; 500 output tokens of which 200 reasoning, at
    // $1.25 in, $0.125 cached, $10 out per million tokens.
    let price = Price {
        input: Some(1.25),
        output: Some(10.0),
        cache_read: Some(0.125),
        ..Price::default()
    };
    let usage = Usage {
        input_tokens: 10_000,
        output_tokens: 500,
        cached_tokens: 8_000,
        reasoning_tokens: 200,
        ..Usage::default()
    };
    let cost = price.cost(&usage.buckets()).unwrap();
    let expected = 2_000.0 * 1.25 / 1e6 + 8_000.0 * 0.125 / 1e6 + 500.0 * 10.0 / 1e6;
    assert!((cost - expected).abs() < 1e-12, "{cost} vs {expected}");
}

#[test]
fn cache_tokens_without_a_price_of_their_own_cost_the_input_price() {
    let price = price(2.0, 4.0);
    let usage = Usage {
        input_tokens: 1_000_000,
        cached_tokens: 400_000,
        cache_write_tokens: 100_000,
        cache_write_1h_tokens: 50_000,
        ..Usage::default()
    };
    let cost = price.cost(&usage.buckets()).unwrap();
    assert!((cost - 2.0).abs() < 1e-9, "{cost}");
    // The 1-hour write has its own price when the table gives one, else the 5-minute one.
    let with_writes = Price {
        cache_write: Some(2.5),
        cache_write_1h: Some(4.0),
        ..price
    };
    let cost = with_writes.cost(&usage.buckets()).unwrap();
    let expected =
        500_000.0 * 2.0 / 1e6 + 400_000.0 * 2.0 / 1e6 + 50_000.0 * 2.5 / 1e6 + 50_000.0 * 4.0 / 1e6;
    assert!((cost - expected).abs() < 1e-9, "{cost} vs {expected}");
}

#[test]
fn a_price_with_no_input_or_output_gives_no_cost() {
    let only_input = Price {
        input: Some(1.0),
        ..Price::default()
    };
    assert_eq!(only_input.cost(&Usage::default().buckets()), None);
}

#[test]
fn a_downloaded_table_wins_over_the_shipped_one_and_the_shipped_one_fills_gaps() {
    let (_dir, pricing) = pricing_with(Some(DOWNLOADED), vec![]);
    let (price, source) = pricing.price_of("openai/gpt-5").unwrap();
    assert_eq!((price.input, price.output), (Some(1.0), Some(8.0)));
    assert_eq!(source, Source::Downloaded("2026-10-03".into()));
    // A model the download does not list is priced from the snapshot.
    let (price, source) = pricing.price_of("anthropic/claude-sonnet-4-5").unwrap();
    assert_eq!(price.input, Some(3.0));
    assert!(matches!(source, Source::Embedded(_)), "{source:?}");
}

// The configuration sets `[pricing."openai/gpt-5*"]` with an input price of 2.00 and the
// snapshot lists 1.25: 1,000,000 uncached input tokens on `openai/gpt-5` cost 2.00.
#[test]
fn a_user_override_wins_over_both() {
    let override_price = Price {
        input: Some(2.0),
        ..Price::default()
    };
    for downloaded in [None, Some(DOWNLOADED)] {
        let (_dir, pricing) = pricing_with(downloaded, vec![("openai/gpt-5*", override_price)]);
        let (price, source) = pricing.price_of("openai/gpt-5").unwrap();
        assert_eq!(source, Source::Override);
        let usage = Usage {
            input_tokens: 1_000_000,
            ..Usage::default()
        };
        assert!((price.cost(&usage.buckets()).unwrap() - 2.0).abs() < 1e-9);
        // Fields the override does not set come from the table below it.
        assert!(price.output.is_some());
    }
}

#[test]
fn the_most_specific_override_wins() {
    let (_dir, pricing) = pricing_with(
        None,
        vec![
            ("openai/*", price(9.0, 9.0)),
            ("openai/gpt-5", price(3.0, 3.0)),
            ("OPENAI/gpt-5-m*", price(5.0, 5.0)),
        ],
    );
    assert_eq!(pricing.price_of("openai/gpt-5").unwrap().0.input, Some(3.0));
    assert_eq!(
        pricing.price_of("openai/gpt-5-mini").unwrap().0.input,
        Some(5.0)
    );
    assert_eq!(pricing.price_of("openai/o3").unwrap().0.input, Some(9.0));
}

#[test]
fn a_model_in_no_source_has_no_price() {
    let (_dir, pricing) = pricing_with(None, vec![]);
    assert!(pricing.price_of("openrouter/some/new-model").is_none());
    assert!(pricing.price_of("mine/unlisted").is_none());
}

#[test]
fn a_subscription_model_is_priced_as_its_api_counterpart() {
    let (_dir, pricing) = pricing_with(None, vec![]);
    let (price, _) = pricing.price_of("chatgpt/gpt-5").unwrap();
    assert_eq!(price.input, Some(1.25));
    assert_eq!(pricing.price_of("chatgpt/not-a-model"), None);
}

#[test]
fn a_downloaded_file_that_cannot_be_read_falls_back_to_the_snapshot() {
    let (_dir, pricing) = pricing_with(Some("not json"), vec![]);
    let (_, source) = pricing.price_of("openai/gpt-5").unwrap();
    assert!(matches!(source, Source::Embedded(_)), "{source:?}");
}

#[test]
fn the_snapshot_date_and_where_it_comes_from_are_known() {
    let (_dir, none) = pricing_with(None, vec![]);
    assert!(matches!(none.snapshot(), Source::Embedded(d) if d == embedded().date));
    let (_dir, some) = pricing_with(Some(DOWNLOADED), vec![]);
    assert_eq!(some.snapshot(), Source::Downloaded("2026-10-03".into()));
}

const API: &str = r#"{
  "openai": {"id":"openai","models":{
      "gpt-5":{"id":"gpt-5","cost":{"input":1.25,"output":10,"cache_read":0.125},"limit":{"context":400000}},
      "gpt-image-1":{"id":"gpt-image-1"},
      "text-embedding-3":{"id":"text-embedding-3","cost":{"input":0.02}}}},
  "anthropic": {"id":"anthropic","models":{
      "claude-sonnet-4-5":{"id":"claude-sonnet-4-5","cost":{"input":3,"output":15,"cache_read":0.3,"cache_write":3.75,"context_over_200k":{"input":6}}}}},
  "openrouter": {"id":"openrouter","models":{
      "qwen/qwen3-coder":{"id":"qwen/qwen3-coder","cost":{"input":0.22,"output":0.95}}}},
  "deepinfra": {"id":"deepinfra","models":{"x":{"id":"x","cost":{"input":1,"output":1}}}}
}"#;

#[test]
fn models_dev_data_is_trimmed_to_the_supported_providers_and_priced_models() {
    let table: PriceTable = trim_models_dev(API, "2026-10-03").unwrap();
    assert_eq!(table.date, "2026-10-03");
    let providers: Vec<&String> = table.providers.keys().collect();
    assert_eq!(providers, ["anthropic", "openai", "openrouter"]);
    let openai: Vec<&String> = table.providers["openai"].keys().collect();
    // No cost, or no output price: no entry.
    assert_eq!(openai, ["gpt-5"]);
    let sonnet = &table.providers["anthropic"]["claude-sonnet-4-5"];
    assert_eq!(sonnet.cache_write, Some(3.75));
    assert_eq!(
        table.providers["openrouter"]["qwen/qwen3-coder"].output,
        Some(0.95)
    );
}

#[test]
fn data_that_is_not_pricing_data_is_refused() {
    for bad in [
        "not json",
        "[]",
        "{}",
        r#"{"openai":{"models":{}}}"#,
        r#"{"openai":{"models":{"a":{"cost":{"input":-1,"output":1}}}}}"#,
    ] {
        assert!(trim_models_dev(bad, "2026-10-03").is_err(), "{bad}");
    }
}
