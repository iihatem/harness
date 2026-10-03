//! The edit format a model's profile chooses, through the binary against a mock model.

mod common;
use common::Isolate;

use assert_cmd::Command;
use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::{body_string_contains, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

const BIN: &str = env!("CARGO_BIN_EXE_harness");

fn stream(chunks: &[Value]) -> ResponseTemplate {
    let mut body: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
    body.push_str("data: [DONE]\n\n");
    ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
}

/// A model that calls `tool` with `arguments`, and then says `done`.
async fn call_then_answer(server: &MockServer, tool: &str, arguments: &str) {
    Mock::given(method("POST"))
        .and(body_string_contains("\"role\":\"tool\""))
        .respond_with(stream(&[json!({"choices": [{"index": 0, "delta": {"content": "done"}, "finish_reason": "stop"}]})]))
        .with_priority(1)
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .respond_with(stream(&[json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "id": "c1", "type": "function",
            "function": {"name": tool, "arguments": arguments}}]}, "finish_reason": "tool_calls"}]})]))
        .with_priority(2)
        .mount(server)
        .await;
}

fn env(server_uri: &str, profiles: &str) -> (TempDir, TempDir) {
    let home = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("config")).unwrap();
    std::fs::write(
        home.path().join("config/config.toml"),
        format!("model = \"mock/test-model\"\n[providers.mock]\nprotocol = \"openai-chat\"\nbase_url = \"{server_uri}/v1\"\n{profiles}"),
    )
    .unwrap();
    (home, ws)
}

fn run(home: &TempDir, ws: &TempDir) -> String {
    let mut cmd = Command::new(BIN);
    let out = cmd
        .current_dir(ws.path())
        .env("HARNESS_HOME", home.path())
        .isolate()
        .env("HARNESS_SANDBOX", "none")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("XDG_STATE_HOME")
        .args(["--mode", "auto", "ask", "--json", "make x.txt"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    String::from_utf8_lossy(&out).into_owned()
}

async fn first_request(server: &MockServer) -> Value {
    let requests = server.received_requests().await.unwrap();
    serde_json::from_slice(&requests[0].body).unwrap()
}

fn tool_names(body: &Value) -> Vec<String> {
    body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap().to_string())
        .collect()
}

fn system(body: &Value) -> String {
    body["messages"][0]["content"].as_str().unwrap().to_string()
}

// Spec "Profile selects a format": the model is offered `apply_patch` and not `edit`.
#[tokio::test(flavor = "multi_thread")]
async fn a_profile_with_apply_patch_offers_it_and_not_edit() {
    let server = MockServer::start().await;
    let patch = "*** Begin Patch\\n*** Add File: x.txt\\n+hello\\n*** End Patch\\n";
    call_then_answer(
        &server,
        "apply_patch",
        &format!("{{\"input\":\"{patch}\"}}"),
    )
    .await;
    let (home, ws) = env(
        &server.uri(),
        "[profiles.\"mock/*\"]\nedit_format = \"apply_patch\"\n",
    );
    let (out, file) = tokio::task::spawn_blocking(move || {
        let out = run(&home, &ws);
        (out, std::fs::read_to_string(ws.path().join("x.txt")))
    })
    .await
    .unwrap();
    let body = first_request(&server).await;
    let names = tool_names(&body);
    assert!(
        names.contains(&"apply_patch".to_string()) && !names.contains(&"edit".to_string()),
        "{names:?}"
    );
    assert!(!names.contains(&"write".to_string()), "{names:?}");
    assert!(system(&body).contains("apply_patch"), "{}", system(&body));
    // And the patch the model sent was applied.
    assert!(out.contains("Applied the patch"), "{out}");
    assert_eq!(file.unwrap(), "hello\n");
}

// Spec "Default format" / "Edit format unset": `edit`, with old_string and new_string.
#[tokio::test(flavor = "multi_thread")]
async fn without_a_setting_the_model_is_offered_edit() {
    let server = MockServer::start().await;
    call_then_answer(
        &server,
        "write",
        "{\"path\":\"x.txt\",\"content\":\"hi\\n\"}",
    )
    .await;
    let (home, ws) = env(&server.uri(), "");
    tokio::task::spawn_blocking(move || run(&home, &ws))
        .await
        .unwrap();
    let body = first_request(&server).await;
    let names = tool_names(&body);
    assert!(
        names.contains(&"edit".to_string()) && !names.contains(&"apply_patch".to_string()),
        "{names:?}"
    );
    let edit = body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["function"]["name"] == "edit")
        .unwrap();
    assert!(edit["function"]["parameters"]["properties"]["old_string"].is_object());
    assert!(system(&body).contains("`edit`"), "{}", system(&body));
}
