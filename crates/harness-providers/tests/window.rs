//! The context window a local server really runs a model with, and the window harness uses.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use harness_config::config::{Protocol, ProviderConfig};
use harness_providers::profiles::{self, FALLBACK_CONTEXT_WINDOW};
use harness_providers::window::{Server, effective_window, running_context};
use serde_json::json;
use wiremock::matchers::{body_string_contains, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PROBE: Duration = Duration::from_millis(500);
const LOAD: Duration = Duration::from_secs(5);

fn base(server: &MockServer) -> String {
    format!("{}/v1", server.uri())
}

fn ps(models: serde_json::Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({ "models": models }))
}

#[tokio::test]
async fn ollama_reports_a_loaded_models_context() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/ps"))
        .respond_with(ps(json!([
            {"name": "llama3.1:latest", "model": "llama3.1:latest", "context_length": 131072},
            {"name": "qwen3-coder:30b", "model": "qwen3-coder:30b", "context_length": 4096}
        ])))
        .mount(&server)
        .await;
    let found = |model: &'static str| {
        let base = base(&server);
        async move { running_context(Server::Ollama, &base, model, PROBE, LOAD).await }
    };
    assert_eq!(found("qwen3-coder:30b").await, Some(4096));
    // A name without a tag is Ollama's `latest`.
    assert_eq!(found("llama3.1").await, Some(131_072));
}

#[tokio::test]
async fn ollama_loads_a_model_that_is_not_running_yet() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/ps"))
        .respond_with(ps(json!([])))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/generate"))
        .and(body_string_contains("qwen3-coder:30b"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"done": true})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/ps"))
        .respond_with(ps(
            json!([{"name": "qwen3-coder:30b", "context_length": 8192}]),
        ))
        .with_priority(2)
        .mount(&server)
        .await;
    assert_eq!(
        running_context(
            Server::Ollama,
            &base(&server),
            "qwen3-coder:30b",
            PROBE,
            LOAD
        )
        .await,
        Some(8192)
    );
}

#[tokio::test]
async fn llama_cpp_reports_its_slot_context() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/props"))
        .and(query_param("model", "qwen"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "default_generation_settings": {"id": 0, "n_ctx": 16384, "params": {}},
            "total_slots": 1
        })))
        .mount(&server)
        .await;
    assert_eq!(
        running_context(Server::LlamaCpp, &base(&server), "qwen", PROBE, LOAD).await,
        Some(16_384)
    );
}

#[tokio::test]
async fn lm_studio_reports_a_loaded_instance_only() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models": [
            {"key": "qwen/qwen3-coder-30b", "loaded_instances": [
                {"id": "qwen/qwen3-coder-30b", "config": {"context_length": 4096}}
            ], "max_context_length": 262144},
            {"key": "google/gemma-3-12b", "loaded_instances": [], "max_context_length": 131072}
        ]})))
        .mount(&server)
        .await;
    let base = base(&server);
    assert_eq!(
        running_context(Server::LmStudio, &base, "qwen/qwen3-coder-30b", PROBE, LOAD).await,
        Some(4096)
    );
    assert_eq!(
        running_context(Server::LmStudio, &base, "google/gemma-3-12b", PROBE, LOAD).await,
        None
    );
}

// Review Focus: a server that is not running, or does not answer, costs no more than the probe's
// time limit.
#[tokio::test]
async fn a_server_that_does_not_answer_reports_nothing_in_time() {
    let started = Instant::now();
    assert_eq!(
        running_context(Server::LlamaCpp, "http://127.0.0.1:9/v1", "m", PROBE, LOAD).await,
        None
    );
    let slow = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(5)))
        .mount(&slow)
        .await;
    assert_eq!(
        running_context(Server::LlamaCpp, &base(&slow), "m", PROBE, LOAD).await,
        None
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
}

// Spec: "Ollama running with a small context".
#[test]
fn a_small_running_context_wins_and_is_warned_about() {
    let profile = profiles::resolve("ollama/qwen3-coder:30b", true, &BTreeMap::new());
    let window = effective_window(
        "ollama/qwen3-coder:30b",
        &profile,
        Some(4096),
        Some(Server::Ollama),
    );
    assert_eq!(window.tokens, 4096);
    assert_eq!(window.warnings.len(), 1, "{:?}", window.warnings);
    let warning = &window.warnings[0];
    assert!(warning.contains("4096-token context window"), "{warning}");
    assert!(warning.contains("OLLAMA_CONTEXT_LENGTH=32768"), "{warning}");
}

#[test]
fn the_smaller_of_server_and_profile_is_used() {
    let profile = profiles::resolve("llamacpp/qwen3-coder", true, &BTreeMap::new());
    let bigger = effective_window(
        "llamacpp/qwen3-coder",
        &profile,
        Some(1_000_000),
        Some(Server::LlamaCpp),
    );
    assert_eq!(bigger.tokens, 262_144);
    assert!(bigger.warnings.is_empty(), "{:?}", bigger.warnings);
    let smaller = effective_window(
        "llamacpp/qwen3-coder",
        &profile,
        Some(16_384),
        Some(Server::LlamaCpp),
    );
    assert_eq!(smaller.tokens, 16_384);
    assert!(
        smaller.warnings[0].contains("llama-server -c 32768"),
        "{:?}",
        smaller.warnings
    );
}

// Spec: "Unknown context window".
#[test]
fn an_unknown_window_falls_back_with_one_warning() {
    let profile = profiles::resolve("mine/new-model", true, &BTreeMap::new());
    let window = effective_window("mine/new-model", &profile, None, None);
    assert_eq!(window.tokens, FALLBACK_CONTEXT_WINDOW);
    assert_eq!(window.warnings.len(), 1, "{:?}", window.warnings);
    assert!(
        window.warnings[0].contains("unknown; assuming 8192 tokens"),
        "{:?}",
        window.warnings
    );
}

#[test]
fn a_profile_below_its_minimum_is_warned_about_without_a_server() {
    let mine = BTreeMap::from([(
        "mine/*".to_string(),
        harness_config::config::ProfileSettings {
            context_window: Some(8_192),
            ..Default::default()
        },
    )]);
    let profile = profiles::resolve("mine/model", false, &mine);
    let window = effective_window("mine/model", &profile, None, None);
    assert_eq!(window.tokens, 8_192);
    assert_eq!(window.warnings.len(), 1, "{:?}", window.warnings);
    assert!(
        window.warnings[0].contains("context_window"),
        "{:?}",
        window.warnings
    );
}

#[test]
fn the_local_servers_are_known_by_name() {
    let mut providers = BTreeMap::new();
    assert_eq!(Server::of("ollama", &providers), Some(Server::Ollama));
    assert_eq!(Server::of("lmstudio", &providers), Some(Server::LmStudio));
    assert_eq!(Server::of("llamacpp", &providers), Some(Server::LlamaCpp));
    assert_eq!(Server::of("openrouter", &providers), None);
    // Moved to another machine, Ollama is still Ollama; spoken to in another protocol, it is not
    // asked.
    providers.insert(
        "ollama".to_string(),
        ProviderConfig {
            protocol: Protocol::OpenaiChat,
            base_url: "http://gpu-box:11434/v1".into(),
            api_key_env: None,
        },
    );
    assert_eq!(Server::of("ollama", &providers), Some(Server::Ollama));
    providers.get_mut("ollama").unwrap().protocol = Protocol::AnthropicMessages;
    assert_eq!(Server::of("ollama", &providers), None);
}
