//! ChatGPT's usage windows, as the Codex client reads them (github.com/openai/codex,
//! `codex-rs/codex-api/src/rate_limits.rs` and `backend-client`): from the response headers
//! `x-codex-{primary,secondary}-{used-percent,window-minutes,reset-at}`, from the
//! `codex.rate_limits` stream event, and from the usage endpoint's body. These formats are not
//! documented by OpenAI and can change, so every field is optional, a window is kept with what is
//! known of it, and nothing here fails: what cannot be read is left out. Kept apart from the
//! adapters so that a change breaks the display, not a session.

use harness_core::meter::{Window, WindowSnapshot, WindowSource};
use reqwest::header::HeaderMap;
use serde_json::Value;

/// A header's value as a number; a value that is not one is a field that is missing.
fn header_number(headers: &HeaderMap, name: &str) -> Option<f64> {
    headers
        .get(name)?
        .to_str()
        .ok()?
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|n| n.is_finite())
}

/// The windows in a response's headers, observed at `now`; `None` when it has none.
pub fn from_headers(headers: &HeaderMap, now: u64) -> Option<WindowSnapshot> {
    let windows: Vec<Window> = ["primary", "secondary"]
        .iter()
        .filter_map(|which| {
            let used = header_number(headers, &format!("x-codex-{which}-used-percent"));
            let minutes = header_number(headers, &format!("x-codex-{which}-window-minutes"));
            let resets = header_number(headers, &format!("x-codex-{which}-reset-at"));
            (used.is_some() || minutes.is_some() || resets.is_some()).then(|| Window {
                window_minutes: minutes.map(|m| m as u64),
                used_percent: used,
                resets_at: resets.map(|r| r as u64),
                source: WindowSource::Header,
            })
        })
        .collect();
    (!windows.is_empty()).then_some(WindowSnapshot {
        windows,
        observed_at: now,
    })
}

fn number(value: &Value) -> Option<f64> {
    value.as_f64().filter(|n| n.is_finite())
}

/// The windows in a `codex.rate_limits` stream event, observed at `now`; `None` when it has none.
pub fn from_event(event: &Value, now: u64) -> Option<WindowSnapshot> {
    let limits = event.get("rate_limits")?;
    let windows: Vec<Window> = ["primary", "secondary"]
        .iter()
        .filter_map(|which| {
            let window = limits.get(which)?.as_object()?;
            let used = window.get("used_percent").and_then(number);
            let minutes = window.get("window_minutes").and_then(number);
            let resets = window.get("reset_at").and_then(number);
            (used.is_some() || minutes.is_some() || resets.is_some()).then(|| Window {
                window_minutes: minutes.map(|m| m as u64),
                used_percent: used,
                resets_at: resets.map(|r| r as u64),
                source: WindowSource::Stream,
            })
        })
        .collect();
    (!windows.is_empty()).then_some(WindowSnapshot {
        windows,
        observed_at: now,
    })
}

/// The windows in the usage endpoint's body (`rate_limit.primary_window`, `secondary_window`,
/// with the length in seconds), observed at `now`. A body with none gives a snapshot with no
/// windows: the account was asked, and said nothing.
pub fn from_usage_body(body: &Value, now: u64) -> WindowSnapshot {
    let windows = ["primary_window", "secondary_window"]
        .iter()
        .filter_map(|which| {
            let window = body.get("rate_limit")?.get(which)?.as_object()?;
            let used = window.get("used_percent").and_then(number);
            let seconds = window.get("limit_window_seconds").and_then(number);
            let resets = window
                .get("reset_at")
                .and_then(number)
                .map(|r| r as u64)
                .or_else(|| {
                    window
                        .get("reset_after_seconds")
                        .and_then(number)
                        .map(|after| now + after as u64)
                });
            (used.is_some() || seconds.is_some() || resets.is_some()).then(|| Window {
                window_minutes: seconds.map(|s| (s / 60.0).round() as u64),
                used_percent: used,
                resets_at: resets,
                source: WindowSource::Poll,
            })
        })
        .collect();
    WindowSnapshot {
        windows,
        observed_at: now,
    }
}
