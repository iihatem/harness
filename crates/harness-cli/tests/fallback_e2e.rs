//! A configured fallback chain through the real binary: a subscription-style limit on the first
//! server is answered by the second, announced in `--json`, and without a chain the run stops.

mod common;
use common::Isolate;

use assert_cmd::Command;
use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

const BIN: &str = env!("CARGO_BIN_EXE_harness");

fn answer(text: &str) -> ResponseTemplate {
    let chunk =
        json!({"choices": [{"index": 0, "delta": {"content": text}, "finish_reason": "stop"}]});
    ResponseTemplate::new(200).set_body_raw(
        format!("data: {chunk}\n\ndata: [DONE]\n\n"),
        "text/event-stream",
    )
}

async fn limited() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429).set_body_json(
            json!({"error": {"type": "usage_limit_reached", "resets_at": 1_893_456_000u64}}),
        ))
        .mount(&server)
        .await;
    server
}

async fn healthy(text: &str) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(answer(text))
        .mount(&server)
        .await;
    server
}

fn env(first: &MockServer, second: &MockServer, chain: &str) -> (TempDir, TempDir) {
    let home = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("config")).unwrap();
    std::fs::write(
        home.path().join("config/config.toml"),
        format!(
            "model = \"first/m\"\n[providers.first]\nprotocol = \"openai-chat\"\nbase_url = \"{}/v1\"\n[providers.second]\nprotocol = \"openai-chat\"\nbase_url = \"{}/v1\"\n[profiles.\"*\"]\ncontext_window = 32768\n{chain}",
            first.uri(),
            second.uri()
        ),
    )
    .unwrap();
    std::fs::create_dir(ws.path().join(".git")).unwrap();
    (home, ws)
}

async fn run_json(home: TempDir, ws: TempDir) -> (Option<i32>, Vec<Value>) {
    let output = tokio::task::spawn_blocking(move || {
        Command::new(BIN)
            .current_dir(ws.path())
            .env("HARNESS_HOME", home.path())
            .isolate()
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME")
            .env_remove("XDG_STATE_HOME")
            .args(["ask", "--json", "go"])
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    let events = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    (output.status.code(), events)
}

// Spec "Subscription limit with a chain", "Headless output": the turn goes on on the second
// model, and `ask --json` prints the `ModelSwitched` event.
#[tokio::test(flavor = "multi_thread")]
async fn a_limit_falls_back_to_the_next_model_and_json_says_so() {
    let (first, second) = (limited().await, healthy("from the second").await);
    let (home, ws) = env(
        &first,
        &second,
        "[fallback]\n\"first/*\" = [\"second/x\"]\n",
    );
    let (code, events) = run_json(home, ws).await;
    assert_eq!(code, Some(0), "{events:?}");
    let switched: Vec<&Value> = events
        .iter()
        .filter(|e| e["type"] == "model_switched")
        .collect();
    assert_eq!(switched.len(), 1, "{events:?}");
    assert_eq!(switched[0]["from"], "first/m");
    assert_eq!(switched[0]["to"], "second/x");
    assert_eq!(switched[0]["reason"], "fallback");
    assert!(
        switched[0]["detail"]
            .as_str()
            .unwrap()
            .contains("not billed")
    );
    assert!(!events.iter().any(|e| e["type"] == "limit_reached"));
    assert!(!events.iter().any(|e| e["type"] == "error"));
    let reply = events
        .iter()
        .find(|e| e["type"] == "assistant_message")
        .unwrap();
    assert_eq!(reply["model"], "second/x");
    assert_eq!(reply["switch_reason"], "fallback");
}

// Spec "No chain": the run stops as before, with the limit's reset time for a frontend.
#[tokio::test(flavor = "multi_thread")]
async fn without_a_chain_the_run_stops_with_the_limit_reported() {
    let (first, second) = (limited().await, healthy("never").await);
    let (home, ws) = env(&first, &second, "");
    let (code, events) = run_json(home, ws).await;
    assert_eq!(code, Some(1));
    assert!(events.iter().any(|e| e["type"] == "limit_reached"));
    assert!(!events.iter().any(|e| e["type"] == "model_switched"));
    assert!(second.received_requests().await.unwrap().is_empty());
}
