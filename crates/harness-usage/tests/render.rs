//! 1.4: how a report reads.

use harness_usage::store::{Group, Money, Report, Row, Tokens, render};

fn row(key: &str, requests: u64, billed: Money, list: Money) -> Row {
    Row {
        key: key.into(),
        requests,
        failed: 0,
        tokens: Tokens {
            input: 12_345,
            cache_read: 8_000,
            cache_write: 0,
            output: 678,
            reasoning: 0,
        },
        billed,
        list,
    }
}

fn money(usd: f64, unknown: u64) -> Money {
    Money { usd, unknown }
}

#[test]
fn an_empty_ledger_says_earlier_versions_are_not_included() {
    let report = Report {
        by: Group::Model,
        rows: Vec::new(),
        total: row("total", 0, money(0.0, 0), money(0.0, 0)),
        ledger_empty: true,
    };
    let text = render(&report).join("\n");
    assert!(text.contains("No usage recorded yet"), "{text}");
    assert!(text.contains("M1 sessions is not included"), "{text}");
}

#[test]
fn a_row_shows_its_figures_and_what_has_no_price() {
    let rows = vec![
        row("openai/gpt-5", 3, money(1.2, 0), money(1.2, 0)),
        row("openrouter/some/new-model", 2, money(0.0, 2), money(0.0, 2)),
        row("ollama/qwen3-coder", 5, money(0.0, 0), money(0.0, 0)),
        row("openai/gpt-4o", 4, money(0.5, 1), money(0.5, 1)),
    ];
    let report = Report {
        by: Group::Model,
        total: row("total", 14, money(1.7, 3), money(1.7, 3)),
        rows,
        ledger_empty: false,
    };
    let lines = render(&report);
    let text = lines.join("\n");
    assert!(text.contains("model"), "{text}");
    assert!(text.contains("billed"), "{text}");
    assert!(text.contains("list price (estimate)"), "{text}");
    let line_of = |key: &str| lines.iter().find(|l| l.starts_with(key)).unwrap().clone();
    assert!(line_of("openai/gpt-5").contains("$1.20"), "{text}");
    // No price at all: said so, not $0.
    let unpriced = line_of("openrouter/some/new-model");
    assert!(unpriced.contains("price unknown"), "{unpriced}");
    assert!(!unpriced.contains("$0.00"), "{unpriced}");
    // A local model's cost is 0, and known.
    assert!(line_of("ollama/qwen3-coder").contains("$0.00"), "{text}");
    // Some priced, some not: the sum of the priced, and how many were not.
    let mixed = line_of("openai/gpt-4o");
    assert!(
        mixed.contains("$0.50") && mixed.contains("1 price unknown"),
        "{mixed}"
    );
    // Tokens have separators.
    assert!(text.contains("12,345"), "{text}");
}

#[test]
fn small_amounts_keep_their_digits() {
    let report = Report {
        by: Group::Day,
        rows: vec![row("2026-10-02", 1, money(0.0013, 0), money(0.0013, 0))],
        total: row("total", 1, money(0.0013, 0), money(0.0013, 0)),
        ledger_empty: false,
    };
    let text = render(&report).join("\n");
    assert!(text.contains("$0.0013"), "{text}");
    assert!(text.starts_with("Usage by day"), "{text}");
}
