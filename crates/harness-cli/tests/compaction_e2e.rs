mod common;
use common::Isolate;

use assert_cmd::Command;
use predicates::str::contains;
use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::{body_string_contains, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

const BIN: &str = env!("CARGO_BIN_EXE_harness");

fn sse(chunks: &[Value]) -> String {
    let mut body: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
    body.push_str("data: [DONE]\n\n");
    body
}

fn text(text: &str) -> ResponseTemplate {
    let chunk =
        json!({"choices": [{"index": 0, "delta": {"content": text}, "finish_reason": "stop"}]});
    ResponseTemplate::new(200).set_body_raw(sse(&[chunk]), "text/event-stream")
}

/// A HARNESS_HOME whose config uses provider `mock` at `server_uri`, and a workspace that looks
/// like a git work tree.
struct Env {
    home: TempDir,
    ws: TempDir,
}

impl Env {
    fn new(server_uri: &str) -> Env {
        let home = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("config")).unwrap();
        std::fs::write(
            home.path().join("config/config.toml"),
            // These tests measure against a 32,768-token window.
            format!("model = \"mock/test-model\"\n[providers.mock]\nprotocol = \"openai-chat\"\nbase_url = \"{server_uri}/v1\"\n[profiles.\"mock/*\"]\ncontext_window = 32768\n"),
        )
        .unwrap();
        std::fs::create_dir(ws.path().join(".git")).unwrap();
        Env { home, ws }
    }

    fn cmd(&self) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.current_dir(self.ws.path())
            .env("HARNESS_HOME", self.home.path())
            .isolate()
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME")
            .env_remove("XDG_STATE_HOME");
        cmd
    }
}

/// Summary requests (recognised by their system prompt) get `summary`.
async fn summarizes(server: &MockServer, summary: &str) {
    Mock::given(method("POST"))
        .and(body_string_contains("You summarize a conversation"))
        .respond_with(text(summary))
        .with_priority(1)
        .mount(server)
        .await;
}

/// The messages of the last request the server received, as `role: first 40 characters`.
async fn last_request(server: &MockServer) -> Vec<String> {
    let requests = server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&requests.last().unwrap().body).unwrap();
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .skip(1)
        .map(|m| {
            let content: String = m["content"]
                .as_str()
                .unwrap_or("")
                .chars()
                .take(40)
                .collect();
            format!("{}: {content}", m["role"].as_str().unwrap())
        })
        .collect()
}

/// The content of the last message of the last request.
async fn last_message(server: &MockServer) -> String {
    let requests = server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&requests.last().unwrap().body).unwrap();
    body["messages"].as_array().unwrap().last().unwrap()["content"]
        .as_str()
        .unwrap()
        .to_string()
}

// Spec: provider reports context overflow.
#[tokio::test(flavor = "multi_thread")]
async fn a_context_overflow_is_compacted_and_retried_once() {
    let server = MockServer::start().await;
    summarizes(&server, "They asked about the parser.").await;
    Mock::given(method("POST"))
        .and(body_string_contains("second question"))
        .respond_with(ResponseTemplate::new(400).set_body_string(
            r#"{"error":{"message":"This model's maximum context length is 8192 tokens","code":"context_length_exceeded"}}"#,
        ))
        .up_to_n_times(1)
        .with_priority(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(text("an answer"))
        .with_priority(3)
        .mount(&server)
        .await;
    let env = Env::new(&server.uri());
    tokio::task::spawn_blocking(move || {
        env.cmd().args(["ask", "first question"]).assert().success();
        env.cmd()
            .args(["-c", "ask", "second question"])
            .assert()
            .success()
            .stdout(contains("an answer"))
            .stderr(contains("compacted the conversation"))
            .stderr(contains("They asked about the parser."));
    })
    .await
    .unwrap();
    // The summary and the next prompt are consecutive user messages, sent as one.
    let last = last_request(&server).await;
    assert_eq!(last.len(), 1, "{last:?}");
    assert!(last[0].starts_with("user: [Summary of the earlier conversation]"));
    assert!(last_message(&server).await.ends_with("\n\nsecond question"));
}

// Spec: automatic compaction.
#[tokio::test(flavor = "multi_thread")]
async fn a_conversation_near_the_window_is_compacted_before_the_next_request() {
    let server = MockServer::start().await;
    summarizes(&server, "A long paste about apples.").await;
    Mock::given(method("POST"))
        .respond_with(text("ok"))
        .with_priority(2)
        .mount(&server)
        .await;
    let env = Env::new(&server.uri());
    // About 15,000 tokens, then about 12,500: together past 80% of the 32,768-token window.
    let first = format!("apples {}", "a".repeat(60_000));
    let second = format!("pears {}", "p".repeat(50_000));
    tokio::task::spawn_blocking(move || {
        env.cmd().args(["ask", &first]).assert().success();
        env.cmd()
            .args(["-c", "ask", "--json", &second])
            .assert()
            .success()
            .stdout(contains(r#""type":"compacted""#));
    })
    .await
    .unwrap();
    let last = last_request(&server).await;
    assert_eq!(last.len(), 1, "{last:?}");
    assert!(last[0].starts_with("user: [Summary of the earlier conversation]"));
    assert!(last_message(&server).await.contains("\n\npears "));
}
