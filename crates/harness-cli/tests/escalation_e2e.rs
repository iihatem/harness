//! Escalation through `harness ask`: a suggestion in `--json`, never a switch.

mod common;
use common::Isolate;

use assert_cmd::Command;
use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

const BIN: &str = env!("CARGO_BIN_EXE_harness");

fn sse(chunk: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(
        format!("data: {chunk}\n\ndata: [DONE]\n\n"),
        "text/event-stream",
    )
}

/// A server that makes `n` calls of a tool that does not exist, then answers.
async fn flailing(n: usize) -> MockServer {
    let server = MockServer::start().await;
    for i in 0..n {
        Mock::given(method("POST"))
            .respond_with(sse(json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "id": format!("c{i}"), "type": "function",
                "function": {"name": "nope", "arguments": "{}"}}]}, "finish_reason": "tool_calls"}]})))
            .up_to_n_times(1)
            .with_priority(1 + i as u8)
            .mount(&server)
            .await;
    }
    Mock::given(method("POST"))
        .respond_with(sse(
            json!({"choices": [{"index": 0, "delta": {"content": "gave up"}, "finish_reason": "stop"}]}),
        ))
        .with_priority(200)
        .mount(&server)
        .await;
    server
}

fn env(server: &MockServer, escalation: &str) -> (TempDir, TempDir) {
    let home = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("config")).unwrap();
    std::fs::write(
        home.path().join("config/config.toml"),
        format!(
            "model = \"mock/m\"\n[providers.mock]\nprotocol = \"openai-chat\"\nbase_url = \"{}/v1\"\n[profiles.\"*\"]\ncontext_window = 32768\n{escalation}",
            server.uri()
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

// Spec "Headless": the model is unchanged and the `--json` output contains the event.
#[tokio::test(flavor = "multi_thread")]
async fn a_headless_run_suggests_in_json_and_never_switches() {
    let server = flailing(3).await;
    let (home, ws) = env(&server, "[escalation]\nto = \"chatgpt/gpt-5\"\n");
    let (code, events) = run_json(home, ws).await;
    assert_eq!(code, Some(0), "{events:?}");
    let found: Vec<&Value> = events
        .iter()
        .filter(|e| e["type"] == "escalation_suggested")
        .collect();
    assert_eq!(found.len(), 1, "{events:?}");
    assert_eq!(found[0]["trigger"], "invalid_tool_calls");
    assert_eq!(found[0]["count"], 3);
    assert_eq!(found[0]["to"], "chatgpt/gpt-5");
    assert!(!events.iter().any(|e| e["type"] == "model_switched"));
    assert_eq!(server.received_requests().await.unwrap().len(), 4);
}

#[tokio::test(flavor = "multi_thread")]
async fn without_escalation_to_nothing_is_suggested() {
    let server = flailing(3).await;
    let (home, ws) = env(&server, "");
    let (code, events) = run_json(home, ws).await;
    assert_eq!(code, Some(0));
    assert!(!events.iter().any(|e| e["type"] == "escalation_suggested"));
}
