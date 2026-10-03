//! 1.7: ChatGPT's usage windows, read from the response headers, the `codex.rate_limits` stream
//! event and `GET /wham/usage`, against fixtures built from the field names the Codex client
//! reads. Every field is optional.

use futures::StreamExt;
use harness_core::message::{ChatRequest, Message};
use harness_core::meter::{WindowSnapshot, WindowSource};
use harness_core::provider::{Provider, ProviderEvent};
use harness_providers::codex_windows::{from_event, from_headers, from_usage_body};
use harness_providers::openai_responses::{OpenAiResponses, ResponsesStreamParser};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Value, json};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

fn fixture(name: &str) -> Value {
    let path = format!(
        "{}/tests/fixtures/codex-windows/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in pairs {
        map.insert(
            HeaderName::from_bytes(name.as_bytes()).unwrap(),
            HeaderValue::from_str(value).unwrap(),
        );
    }
    map
}

const NOW: u64 = 1_790_943_000;

fn labels(snapshot: &WindowSnapshot) -> Vec<(String, Option<f64>, Option<u64>)> {
    snapshot
        .windows
        .iter()
        .map(|w| (w.label(), w.used_percent, w.resets_at))
        .collect()
}

// A response carries a 300-minute window at 42% and a 10,080-minute window at 12%.
#[test]
fn both_windows_come_from_the_headers() {
    let map = headers(&[
        ("x-codex-primary-used-percent", "42.0"),
        ("x-codex-primary-window-minutes", "300"),
        ("x-codex-primary-reset-at", "1790946000"),
        ("x-codex-secondary-used-percent", "12"),
        ("x-codex-secondary-window-minutes", "10080"),
        ("x-codex-secondary-reset-at", "1791142400"),
    ]);
    let snapshot = from_headers(&map, NOW).unwrap();
    assert_eq!(snapshot.observed_at, NOW);
    assert_eq!(
        labels(&snapshot),
        [
            ("5h".to_string(), Some(42.0), Some(1_790_946_000)),
            ("7d".to_string(), Some(12.0), Some(1_791_142_400)),
        ]
    );
    assert!(
        snapshot
            .windows
            .iter()
            .all(|w| w.source == WindowSource::Header)
    );
}

#[test]
fn a_window_with_some_fields_missing_is_kept_with_what_is_known() {
    let map = headers(&[("x-codex-secondary-window-minutes", "10080")]);
    let snapshot = from_headers(&map, NOW).unwrap();
    assert_eq!(snapshot.windows.len(), 1);
    assert_eq!(snapshot.windows[0].label(), "7d");
    assert_eq!(snapshot.windows[0].used_percent, None);
    // A value that is not a number is a field that is missing.
    let map = headers(&[
        ("x-codex-primary-used-percent", "lots"),
        ("x-codex-primary-window-minutes", "300"),
    ]);
    assert_eq!(
        from_headers(&map, NOW).unwrap().windows[0].used_percent,
        None
    );
}

// The response has no rate-limit fields: nothing is reported, so the window stays unknown.
#[test]
fn no_rate_limit_headers_report_nothing() {
    assert_eq!(
        from_headers(&headers(&[("content-type", "text/event-stream")]), NOW),
        None
    );
    assert_eq!(from_headers(&HeaderMap::new(), NOW), None);
}

#[test]
fn the_stream_event_carries_the_same_windows() {
    let snapshot = from_event(&fixture("event_both.json"), NOW).unwrap();
    assert_eq!(
        labels(&snapshot),
        [
            ("5h".to_string(), Some(42.5), Some(1_790_946_000)),
            ("7d".to_string(), Some(12.0), Some(1_791_142_400)),
        ]
    );
    assert!(
        snapshot
            .windows
            .iter()
            .all(|w| w.source == WindowSource::Stream)
    );
    assert_eq!(from_event(&json!({"type": "codex.rate_limits"}), NOW), None);
    assert_eq!(
        from_event(
            &json!({"type": "codex.rate_limits", "rate_limits": {}}),
            NOW
        ),
        None
    );
}

#[test]
fn the_responses_parser_reports_the_event_and_the_headers() {
    let mut parser = ResponsesStreamParser::default();
    let events = parser
        .push(&fixture("event_both.json").to_string())
        .unwrap();
    assert!(
        matches!(&events[..], [ProviderEvent::RateLimits(s)] if s.windows.len() == 2),
        "{events:?}"
    );
}

// The usage endpoint reports seconds, and when a window resets in a number of seconds from now.
#[test]
fn the_usage_endpoint_gives_both_windows() {
    let snapshot = from_usage_body(&fixture("usage_both.json"), NOW);
    assert_eq!(
        labels(&snapshot),
        [
            ("5h".to_string(), Some(42.0), Some(1_790_946_000)),
            ("7d".to_string(), Some(12.0), Some(1_791_142_400)),
        ]
    );
    assert!(
        snapshot
            .windows
            .iter()
            .all(|w| w.source == WindowSource::Poll)
    );
}

// The usage endpoint reports only a 10,080-minute window: only "7d" is shown, and no 5h window
// is invented.
#[test]
fn a_plan_with_one_window_has_one_window() {
    let snapshot = from_usage_body(&fixture("usage_weekly_only.json"), NOW);
    assert_eq!(labels(&snapshot).len(), 1);
    assert_eq!(snapshot.windows[0].label(), "7d");
}

#[test]
fn a_usage_body_with_no_fields_has_no_windows() {
    assert!(
        from_usage_body(&fixture("usage_empty.json"), NOW)
            .windows
            .is_empty()
    );
    assert!(from_usage_body(&json!({}), NOW).windows.is_empty());
    assert!(from_usage_body(&json!("nonsense"), NOW).windows.is_empty());
}

#[test]
fn a_reset_given_in_seconds_from_now_becomes_a_time() {
    let body = json!({"rate_limit": {"primary_window": {"used_percent": 5, "limit_window_seconds": 18000, "reset_after_seconds": 600}}});
    let snapshot = from_usage_body(&body, NOW);
    assert_eq!(snapshot.windows[0].resets_at, Some(NOW + 600));
}

fn request() -> ChatRequest {
    ChatRequest {
        model: "gpt-5".into(),
        system: "s".into(),
        messages: vec![Message::User {
            content: "hi".into(),
        }],
        ..ChatRequest::default()
    }
}

// The headers of a real response reach the runtime as an event, before the reply's text.
#[tokio::test]
async fn a_response_with_rate_limit_headers_yields_a_window_event() {
    let server = MockServer::start().await;
    let body = [
        json!({"type": "response.output_text.delta", "output_index": 0, "delta": "hi"}),
        json!({"type": "response.completed", "response": {"status": "completed"}}),
    ]
    .iter()
    .map(|e| format!("data: {e}\n\n"))
    .collect::<String>();
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-codex-primary-used-percent", "62")
                .insert_header("x-codex-primary-window-minutes", "300")
                .insert_header("x-codex-secondary-used-percent", "20")
                .insert_header("x-codex-secondary-window-minutes", "10080")
                .set_body_raw(body, "text/event-stream"),
        )
        .mount(&server)
        .await;
    let provider = OpenAiResponses::new(format!("{}/v1", server.uri()), None);
    let events: Vec<_> = provider.stream(request()).collect().await;
    let windows: Vec<&WindowSnapshot> = events
        .iter()
        .filter_map(|e| match e {
            Ok(ProviderEvent::RateLimits(s)) => Some(s),
            _ => None,
        })
        .collect();
    assert_eq!(windows.len(), 1, "{events:?}");
    assert_eq!(windows[0].windows[0].label(), "5h");
    assert_eq!(windows[0].windows[0].used_percent, Some(62.0));
    assert_eq!(windows[0].windows[1].label(), "7d");
    // Before any reply text.
    let first_text = events
        .iter()
        .position(|e| matches!(e, Ok(ProviderEvent::TextDelta(_))));
    let first_window = events
        .iter()
        .position(|e| matches!(e, Ok(ProviderEvent::RateLimits(_))));
    assert!(first_window < first_text);
}

// A response with no rate-limit headers (an API key, another backend) yields no window event.
#[tokio::test]
async fn a_response_without_headers_yields_no_window_event() {
    let server = MockServer::start().await;
    let body = format!(
        "data: {}\n\n",
        json!({"type": "response.completed", "response": {"status": "completed"}})
    );
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .mount(&server)
        .await;
    let provider = OpenAiResponses::new(format!("{}/v1", server.uri()), None);
    let events: Vec<_> = provider.stream(request()).collect().await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Ok(ProviderEvent::RateLimits(_)))),
        "{events:?}"
    );
}

#[test]
fn only_the_chatgpt_provider_can_be_polled_for_windows() {
    let provider = OpenAiResponses::new("http://127.0.0.1:9/v1", Some("key".into()));
    assert!(provider.windows().is_none());
}
