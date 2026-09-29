use std::time::{Duration, Instant};

use harness_config::config::Protocol;
use harness_providers::discovery::{DiscoveredModel, Endpoint, list_models};
use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn endpoint(provider: &str, base_url: String) -> Endpoint {
    Endpoint {
        provider: provider.into(),
        base_url,
        api_key: None,
        protocol: Protocol::OpenaiChat,
    }
}

#[tokio::test]
async fn lists_models_sorted_and_skips_unreachable_or_slow_servers() {
    let fast = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"data": [{"id": "zeta"}, {"id": "alpha"}]})),
        )
        .mount(&fast)
        .await;
    let slow = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"data": [{"id": "late"}]}))
                .set_delay(Duration::from_secs(2)),
        )
        .mount(&slow)
        .await;

    let started = Instant::now();
    let found = list_models(
        &[
            endpoint("ollama", format!("{}/v1", fast.uri())),
            endpoint("lmstudio", "http://127.0.0.1:9/v1".into()),
            endpoint("llamacpp", format!("{}/v1", slow.uri())),
        ],
        Duration::from_millis(300),
    )
    .await;

    assert!(
        started.elapsed() < Duration::from_secs(1),
        "probes must run concurrently and time out"
    );
    assert_eq!(
        found,
        vec![
            DiscoveredModel {
                provider: "ollama".into(),
                name: "alpha".into()
            },
            DiscoveredModel {
                provider: "ollama".into(),
                name: "zeta".into()
            },
        ]
    );
    assert_eq!(found[0].id(), "ollama/alpha");
}

#[tokio::test]
async fn probes_run_concurrently() {
    let server1 = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"data": [{"id": "m"}]}))
                .set_delay(Duration::from_secs(2)),
        )
        .mount(&server1)
        .await;
    let server2 = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"data": [{"id": "m"}]}))
                .set_delay(Duration::from_secs(2)),
        )
        .mount(&server2)
        .await;
    let server3 = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"data": [{"id": "m"}]}))
                .set_delay(Duration::from_secs(2)),
        )
        .mount(&server3)
        .await;

    let started = Instant::now();
    let found = list_models(
        &[
            endpoint("p1", format!("{}/v1", server1.uri())),
            endpoint("p2", format!("{}/v1", server2.uri())),
            endpoint("p3", format!("{}/v1", server3.uri())),
        ],
        Duration::from_millis(300),
    )
    .await;

    let elapsed = started.elapsed();
    assert!(
        found.is_empty(),
        "all probes should time out with 300ms timeout against 2s servers"
    );
    assert!(
        elapsed < Duration::from_millis(700),
        "probes must run concurrently (got {:.3}s; sequential would take ≥ 900ms)",
        elapsed.as_secs_f64()
    );
}

#[tokio::test]
async fn error_statuses_yield_no_models() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    let found = list_models(
        &[endpoint("x", format!("{}/v1", server.uri()))],
        Duration::from_millis(300),
    )
    .await;
    assert!(found.is_empty());
}

#[tokio::test]
async fn anthropic_endpoints_are_listed_with_their_own_headers() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("x-api-key", "sk-ant-api03-k"))
        .and(header("anthropic-version", "2023-06-01"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"type": "model", "id": "claude-sonnet-4-5", "display_name": "Claude Sonnet 4.5"}],
            "has_more": false
        })))
        .mount(&server)
        .await;
    let found = list_models(
        &[Endpoint {
            provider: "anthropic".into(),
            base_url: format!("{}/v1", server.uri()),
            api_key: Some("sk-ant-api03-k".into()),
            protocol: Protocol::AnthropicMessages,
        }],
        Duration::from_secs(3),
    )
    .await;
    assert_eq!(
        found,
        vec![DiscoveredModel {
            provider: "anthropic".into(),
            name: "claude-sonnet-4-5".into()
        }]
    );
}

// Review B, M11: an endpoint carries a stored key, which its Debug leaves out.
#[test]
fn an_endpoints_debug_leaves_its_key_out() {
    let endpoint = Endpoint {
        api_key: Some("sk-stored-secret".into()),
        ..endpoint("openai", "https://api.openai.com/v1".into())
    };
    let shown = format!("{endpoint:?}");
    assert!(!shown.contains("sk-stored-secret"), "{shown}");
    assert!(shown.contains("openai"), "{shown}");
}
