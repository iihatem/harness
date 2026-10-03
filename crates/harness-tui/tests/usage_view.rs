//! 1.8: the status line's window and session cost, the 80% and 95% window warnings, and `/usage`
//! with windows, the three figures and the price snapshot's date, on ratatui's `TestBackend`.

mod common;

use std::{sync::Arc, time::Duration};

use common::*;
use futures::future::BoxFuture;
use harness_core::{
    event::AgentEvent,
    message::ChatRequest,
    meter::{AccountKind, Avoided, RequestCost, Window, WindowSnapshot, WindowSource},
    permission::Mode,
    provider::{FinishReason, Provider, ProviderEvent, ProviderStream},
    testing::{MockProvider, Script},
};
use harness_tui::{
    app::{Host, Prepared},
    ui::Ui,
    usage::UsageContext,
};
use ratatui::backend::TestBackend;

/// 2026-10-02 14:00:00 UTC.
const NOW: u64 = 1_790_949_600;

struct NoCommands;

impl Host for NoCommands {
    fn is_command(&self, _name: &str) -> bool {
        false
    }
    fn prepare(&mut self, _typed: &str) -> Prepared {
        unreachable!()
    }
}

fn window(minutes: u64, used: f64, resets_in: u64) -> Window {
    Window {
        window_minutes: Some(minutes),
        used_percent: Some(used),
        resets_at: Some(NOW + resets_in),
        source: WindowSource::Header,
    }
}

fn snapshot(five_hour: f64) -> WindowSnapshot {
    WindowSnapshot {
        windows: vec![window(300, five_hour, 1_800), window(10_080, 20.0, 200_000)],
        observed_at: NOW,
    }
}

fn session(model: &str) -> (tempfile::TempDir, Ui<TestBackend>) {
    let dir = tempfile::tempdir().unwrap();
    let agent = agent(
        MockProvider::new(vec![Script::text("ok")]),
        dir.path(),
        Mode::Auto,
    );
    let mut options = options(dir.path(), Mode::Auto);
    options.model = model.into();
    let (mut ui, _) = start(agent, Box::new(NoCommands), options);
    ui.app_mut().set_clock(Arc::new(|| NOW));
    (dir, ui)
}

fn limits(ui: &mut Ui<TestBackend>, snapshot: WindowSnapshot) {
    ui.app_mut().on_event(&AgentEvent::RateLimits { snapshot });
    ui.draw().unwrap();
}

fn metered(ui: &mut Ui<TestBackend>, model: &str, cost: RequestCost) {
    ui.app_mut().on_event(&AgentEvent::Metered {
        model: model.into(),
        cost,
    });
    ui.draw().unwrap();
}

fn api_key(usd: Option<f64>) -> RequestCost {
    RequestCost {
        account: AccountKind::ApiKey,
        billed_usd: usd,
        list_usd: usd,
        avoided: Avoided::NotApplicable,
    }
}

fn status_row(ui: &Ui<TestBackend>) -> String {
    screen(ui)
        .into_iter()
        .find(|r| r.contains(" · auto · "))
        .unwrap_or_else(|| panic!("no status line: {:#?}", screen(ui)))
}

fn warnings(ui: &Ui<TestBackend>) -> Vec<String> {
    everything(ui)
        .into_iter()
        .filter(|r| r.starts_with("warning: ") && r.contains("window"))
        .collect()
}

// The active provider is ChatGPT with a 5-hour window at 62% and a 7-day window at 20%, and the
// session has made no billed request: the status line shows the 5h window and no cost.
#[tokio::test]
async fn the_status_line_shows_the_most_used_window_by_its_length() {
    let (_dir, mut ui) = session("chatgpt/gpt-5");
    limits(&mut ui, snapshot(62.0));
    let status = status_row(&ui);
    assert!(status.contains("5h 62%"), "{status}");
    assert!(!status.contains("7d"), "{status}");
    assert!(!status.contains('$'), "{status}");
}

#[tokio::test]
async fn a_window_nobody_has_reported_is_unknown_never_zero() {
    let (_dir, ui) = session("chatgpt/gpt-5");
    let status = status_row(&ui);
    assert!(status.contains("window unknown"), "{status}");
    assert!(!status.contains("0%"), "{status}");
    // A snapshot with no usable window is the same.
    let (_dir, mut ui) = session("chatgpt/gpt-5");
    limits(
        &mut ui,
        WindowSnapshot {
            windows: vec![],
            observed_at: NOW,
        },
    );
    assert!(status_row(&ui).contains("window unknown"));
}

#[tokio::test]
async fn other_providers_show_no_window() {
    let (_dir, mut ui) = session("openai/gpt-5");
    limits(&mut ui, snapshot(62.0));
    let status = status_row(&ui);
    assert!(
        !status.contains("5h") && !status.contains("window"),
        "{status}"
    );
}

#[tokio::test]
async fn a_snapshot_observed_more_than_15_minutes_ago_is_marked_stale() {
    let (_dir, mut ui) = session("chatgpt/gpt-5");
    let mut old = snapshot(62.0);
    old.observed_at = NOW - 20 * 60;
    limits(&mut ui, old);
    assert!(
        status_row(&ui).contains("5h 62% (stale)"),
        "{}",
        status_row(&ui)
    );
    let mut recent = snapshot(62.0);
    recent.observed_at = NOW - 10 * 60;
    limits(&mut ui, recent);
    assert!(!status_row(&ui).contains("stale"));
}

#[tokio::test]
async fn the_session_cost_appears_once_a_billed_request_was_made() {
    let (_dir, mut ui) = session("openai/gpt-5");
    assert!(!status_row(&ui).contains('$'));
    // A subscription or local request is not billed: no cost yet.
    metered(
        &mut ui,
        "chatgpt/gpt-5",
        RequestCost {
            account: AccountKind::Subscription,
            billed_usd: Some(0.0),
            list_usd: Some(1.25),
            avoided: Avoided::NotApplicable,
        },
    );
    assert!(!status_row(&ui).contains('$'), "{}", status_row(&ui));
    metered(&mut ui, "openai/gpt-5", api_key(Some(0.30)));
    metered(&mut ui, "openai/gpt-5", api_key(Some(0.12)));
    assert!(status_row(&ui).contains("$0.42"), "{}", status_row(&ui));
}

// The session runs on an API-key model with no known price: the cost shows "price unknown", not $0.
#[tokio::test]
async fn a_billed_request_with_no_price_shows_price_unknown() {
    let (_dir, mut ui) = session("openrouter/some/new-model");
    metered(&mut ui, "openrouter/some/new-model", api_key(None));
    let status = status_row(&ui);
    assert!(status.contains("price unknown"), "{status}");
    assert!(!status.contains("$0"), "{status}");
}

// The 5h window moves from 78% to 83%: one warning naming it, and the next request is sent
// normally; then from 83% to 86%: no further 80% warning.
#[tokio::test]
async fn crossing_80_percent_warns_once_and_never_blocks() {
    let (_dir, mut ui) = session("chatgpt/gpt-5");
    limits(&mut ui, snapshot(78.0));
    assert!(warnings(&ui).is_empty(), "{:?}", warnings(&ui));
    limits(&mut ui, snapshot(83.0));
    let first = warnings(&ui);
    assert_eq!(first.len(), 1, "{first:?}");
    assert!(
        first[0].contains("5h") && first[0].contains("83%"),
        "{first:?}"
    );
    limits(&mut ui, snapshot(86.0));
    assert_eq!(warnings(&ui).len(), 1, "{:?}", warnings(&ui));
    // The window never delays or refuses a request.
    type_text(&mut ui, "go");
    press(&mut ui, crossterm_enter());
    ui.settle().await.unwrap();
    assert!(shows(&ui, "ok"), "{:#?}", everything(&ui));
}

fn crossterm_enter() -> ratatui::crossterm::event::KeyCode {
    ratatui::crossterm::event::KeyCode::Enter
}

#[tokio::test]
async fn crossing_95_percent_warns_once_more_and_a_new_window_warns_again() {
    let (_dir, mut ui) = session("chatgpt/gpt-5");
    limits(&mut ui, snapshot(83.0));
    limits(&mut ui, snapshot(96.0));
    let both = warnings(&ui);
    assert_eq!(both.len(), 2, "{both:?}");
    assert!(
        both[1].contains("95%") || both[1].contains("96%"),
        "{both:?}"
    );
    limits(&mut ui, snapshot(97.0));
    assert_eq!(warnings(&ui).len(), 2);
    // The 5h window reset: a new window, which warns again at 85%.
    let mut next = snapshot(85.0);
    next.windows[0].resets_at = Some(NOW + 20_000);
    limits(&mut ui, next);
    assert_eq!(warnings(&ui).len(), 3, "{:?}", warnings(&ui));
}

#[tokio::test]
async fn a_jump_straight_past_both_thresholds_gives_one_warning() {
    let (_dir, mut ui) = session("chatgpt/gpt-5");
    limits(&mut ui, snapshot(40.0));
    limits(&mut ui, snapshot(97.0));
    assert_eq!(warnings(&ui).len(), 1, "{:?}", warnings(&ui));
}

fn usage_ctx(baseline: Option<&str>) -> UsageContext {
    UsageContext {
        baseline: baseline.map(String::from),
        prices: "embedded 2026-10-03".into(),
        ..UsageContext::default()
    }
}

// ChatGPT with a 5-hour window at 62% resetting at 14:30, a baseline named, and `/usage`.
#[tokio::test]
async fn usage_shows_the_window_the_three_figures_and_the_price_date() {
    let (_dir, mut ui) = session("chatgpt/gpt-5");
    ui.app_mut()
        .set_usage_context(usage_ctx(Some("openai/gpt-5")));
    limits(&mut ui, snapshot(62.0));
    metered(
        &mut ui,
        "chatgpt/gpt-5",
        RequestCost {
            account: AccountKind::Subscription,
            billed_usd: Some(0.0),
            list_usd: Some(1.25),
            avoided: Avoided::Usd(1.25),
        },
    );
    metered(&mut ui, "openai/gpt-5", api_key(Some(0.50)));
    type_text(&mut ui, "/usage");
    press(&mut ui, ratatui::crossterm::event::KeyCode::Enter);
    let flat = everything(&ui).join("\n");
    let row = |text: &str| {
        everything(&ui)
            .into_iter()
            .find(|r| r.contains(text))
            .unwrap_or_else(|| panic!("no row with {text:?}: {flat}"))
    };
    let window = row("62% used");
    assert!(
        window.contains("5h") && window.contains("14:30"),
        "{window}"
    );
    assert!(row("7d").contains("20% used"));
    // Billed, estimated and avoided are three lines, never one sum.
    assert!(row("billed").contains("$0.50"), "{flat}");
    let list = row("list price");
    assert!(
        list.contains("$1.75") && list.contains("estimate"),
        "{list}"
    );
    let avoided = row("avoided");
    assert!(
        avoided.contains("$1.25") && avoided.contains("openai/gpt-5"),
        "{avoided}"
    );
    assert!(
        row("price snapshot").contains("embedded 2026-10-03"),
        "{flat}"
    );
}

#[tokio::test]
async fn usage_shows_no_avoided_figure_without_a_baseline_and_says_price_unknown() {
    let (_dir, mut ui) = session("openrouter/some/new-model");
    ui.app_mut().set_usage_context(usage_ctx(None));
    metered(&mut ui, "openrouter/some/new-model", api_key(None));
    metered(&mut ui, "openai/gpt-5", api_key(Some(0.50)));
    type_text(&mut ui, "/usage");
    press(&mut ui, ratatui::crossterm::event::KeyCode::Enter);
    let rows = everything(&ui);
    assert!(!rows.iter().any(|r| r.contains("avoided")), "{rows:#?}");
    // Each model without a known price shows it, and its tokens are not priced at $0.
    let unknown = rows
        .iter()
        .find(|r| r.starts_with("openrouter/some/new-model") && r.contains("price unknown"))
        .unwrap_or_else(|| panic!("{rows:#?}"));
    assert!(!unknown.contains("$0"), "{unknown}");
    let billed = rows
        .iter()
        .find(|r| r.contains("billed") && r.contains('$'))
        .unwrap();
    assert!(
        billed.contains("$0.50") && billed.contains("price unknown"),
        "{billed}"
    );
}

#[tokio::test]
async fn usage_says_a_subscription_window_is_unknown_when_nothing_was_reported() {
    let (_dir, mut ui) = session("chatgpt/gpt-5");
    type_text(&mut ui, "/usage");
    press(&mut ui, ratatui::crossterm::event::KeyCode::Enter);
    assert!(shows(&ui, "window unknown"), "{:#?}", everything(&ui));
}

/// A provider that answers every request with `ok` and, asked, reports a 7d window.
struct Polled;

impl Provider for Polled {
    fn stream(&self, _request: ChatRequest) -> ProviderStream {
        Box::pin(futures::stream::iter(vec![
            Ok(ProviderEvent::TextDelta("ok".into())),
            Ok(ProviderEvent::Finished(FinishReason::Stop)),
        ]))
    }

    fn windows(&self) -> Option<BoxFuture<'static, Result<WindowSnapshot, String>>> {
        Some(Box::pin(async {
            Ok(WindowSnapshot {
                windows: vec![Window {
                    window_minutes: Some(10_080),
                    used_percent: Some(12.0),
                    resets_at: Some(NOW + 100_000),
                    source: WindowSource::Poll,
                }],
                observed_at: NOW,
            })
        }))
    }
}

/// A host that has a ledger.
struct WithLedger;

impl Host for WithLedger {
    fn is_command(&self, _name: &str) -> bool {
        false
    }
    fn prepare(&mut self, _typed: &str) -> Prepared {
        unreachable!()
    }
    fn usage_report(&self, args: &str) -> BoxFuture<'static, Vec<String>> {
        let args = args.to_string();
        Box::pin(async move { vec![format!("ledger report for `{args}`")] })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn usage_asks_the_provider_for_its_windows_and_adds_the_ledger() {
    let dir = tempfile::tempdir().unwrap();
    let agent = agent(Arc::new(Polled), dir.path(), Mode::Auto);
    let mut options = options(dir.path(), Mode::Auto);
    options.model = "chatgpt/gpt-5".into();
    let (mut ui, _) = start(agent, Box::new(WithLedger), options);
    ui.app_mut().set_clock(Arc::new(|| NOW));
    type_text(&mut ui, "/usage day");
    press(&mut ui, ratatui::crossterm::event::KeyCode::Enter);
    for _ in 0..50 {
        if shows(&ui, "ledger report for") && shows(&ui, "12% used") {
            break;
        }
        let _ = tokio::time::timeout(Duration::from_millis(100), ui.next()).await;
    }
    assert!(
        shows(&ui, "ledger report for `day`"),
        "{:#?}",
        everything(&ui)
    );
    assert!(shows(&ui, "12% used"), "{:#?}", everything(&ui));
}

// When ChatGPT is the active provider the session asks for the windows once as it starts.
#[tokio::test(flavor = "multi_thread")]
async fn the_windows_are_asked_for_once_when_the_session_starts() {
    let dir = tempfile::tempdir().unwrap();
    let agent = agent(Arc::new(Polled), dir.path(), Mode::Auto);
    let mut options = options(dir.path(), Mode::Auto);
    options.model = "chatgpt/gpt-5".into();
    let (mut ui, _) = start(agent, Box::new(NoCommands), options);
    ui.app_mut().set_clock(Arc::new(|| NOW));
    tokio::time::timeout(Duration::from_secs(5), ui.until(|app| app.has_windows()))
        .await
        .expect("the poll answered")
        .unwrap();
    ui.draw().unwrap();
    assert!(status_row(&ui).contains("7d 12%"), "{}", status_row(&ui));
}

// B-minor 1: when the provider is asked, the windows are shown once (what it answered), not once
// from what the session knew and again after.
#[tokio::test(flavor = "multi_thread")]
async fn usage_shows_the_windows_once() {
    let dir = tempfile::tempdir().unwrap();
    let agent = agent(Arc::new(Polled), dir.path(), Mode::Auto);
    let mut options = options(dir.path(), Mode::Auto);
    options.model = "chatgpt/gpt-5".into();
    let (mut ui, _) = start(agent, Box::new(WithLedger), options);
    ui.app_mut().set_clock(Arc::new(|| NOW));
    type_text(&mut ui, "/usage");
    press(&mut ui, ratatui::crossterm::event::KeyCode::Enter);
    for _ in 0..50 {
        if shows(&ui, "ledger report for") && shows(&ui, "12% used") {
            break;
        }
        let _ = tokio::time::timeout(Duration::from_millis(100), ui.next()).await;
    }
    let blocks = everything(&ui)
        .iter()
        .filter(|r| r.contains("Subscription windows"))
        .count();
    assert_eq!(blocks, 1, "{:#?}", everything(&ui));
    assert!(!shows(&ui, "window unknown"), "{:#?}", everything(&ui));
}
