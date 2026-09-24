use std::time::{Duration, Instant};

use harness_providers::discovery::{DiscoveredModel, Endpoint, list_models};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn endpoint(provider: &str, base_url: String) -> Endpoint {
    Endpoint {
        provider: provider.into(),
        base_url,
        api_key: None,
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
