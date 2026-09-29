//! Local models in `harness ask`: the window the server really runs the model with, and a
//! conversation held on local models that is continued on a hosted one (which says nothing).

mod common;
use common::Isolate;

use assert_cmd::Command;
use predicates::prelude::*;
use predicates::str::contains;
use serde_json::json;
use tempfile::TempDir;
use wiremock::matchers::{body_string_contains, method, path};
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

struct Env {
    home: TempDir,
    ws: TempDir,
}

impl Env {
    fn new(config: &str) -> Env {
        let home = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("config")).unwrap();
        std::fs::write(home.path().join("config/config.toml"), config).unwrap();
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

// Spec: "Ollama running with a small context".
#[tokio::test(flavor = "multi_thread")]
async fn ollamas_small_running_context_is_used_and_explained() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/ps"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models": [
            {"name": "qwen3-coder:30b", "model": "qwen3-coder:30b", "context_length": 4096}
        ]})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(answer("ok"))
        .mount(&server)
        .await;
    // Ollama moved to the mock server: still Ollama, so it is asked.
    let env = Env::new(&format!(
        "model = \"ollama/qwen3-coder:30b\"\n[providers.ollama]\nprotocol = \"openai-chat\"\nbase_url = \"{}/v1\"\n",
        server.uri()
    ));
    // About 1,250 tokens: more than a quarter of 4,096, but not of the model's 262,144.
    std::fs::write(env.ws.path().join("AGENTS.md"), "x".repeat(5_000)).unwrap();
    tokio::task::spawn_blocking(move || {
        env.cmd()
            .args(["ask", "hi"])
            .assert()
            .success()
            .stderr(contains("runs with a 4096-token context window"))
            .stderr(contains("OLLAMA_CONTEXT_LENGTH=32768"))
            .stderr(contains("the 4096-token context window"));
    })
    .await
    .unwrap();
}

// Review D M2: a model Ollama does not have gets one clear message, Ollama's own, rather than a
// warning that its window is unknown first.
#[tokio::test(flavor = "multi_thread")]
async fn a_model_ollama_does_not_have_gets_one_message() {
    let server = MockServer::start().await;
    let refusal = json!({"error": "model \"nope\" not found, try pulling it first"});
    Mock::given(method("GET"))
        .and(path("/api/ps"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models": []})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/generate"))
        .respond_with(ResponseTemplate::new(404).set_body_json(refusal.clone()))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({"error": {
            "message": "model \"nope\" not found, try pulling it first",
            "type": "api_error", "param": null, "code": null}})))
        .mount(&server)
        .await;
    let env = Env::new(&format!(
        "model = \"ollama/nope\"\n[providers.ollama]\nprotocol = \"openai-chat\"\nbase_url = \"{}/v1\"\n",
        server.uri()
    ));
    tokio::task::spawn_blocking(move || {
        env.cmd()
            .args(["ask", "hi"])
            .assert()
            .failure()
            .stderr(contains("not found, try pulling it first"))
            .stderr(contains("context window").not());
    })
    .await
    .unwrap();
}

// Decision 14: continuing a conversation held on local models with a hosted model says nothing
// about it (the behaviour before P4); harness does not flag it.
#[tokio::test(flavor = "multi_thread")]
async fn a_local_conversation_continued_on_a_hosted_model_prints_no_warning() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(answer("ok"))
        .mount(&server)
        .await;
    // `mock` is on the loopback interface, so local; `cloud` is too, but its profile says it is
    // not.
    let env = Env::new(&format!(
        "[providers.mock]\nprotocol = \"openai-chat\"\nbase_url = \"{uri}/v1\"\n[providers.cloud]\nprotocol = \"openai-chat\"\nbase_url = \"{uri}/v1\"\n[profiles.\"mock/*\"]\ncontext_window = 32768\n[profiles.\"cloud/*\"]\ncontext_window = 200000\nlocal = false\n",
        uri = server.uri()
    ));
    let flagged = "ran on local models so far; continuing it on";
    tokio::task::spawn_blocking(move || {
        env.cmd()
            .args(["--model", "mock/small", "ask", "one"])
            .assert()
            .success()
            .stderr(contains(flagged).not());
        env.cmd()
            .args(["-c", "--model", "mock/small", "ask", "two"])
            .assert()
            .success()
            .stderr(contains(flagged).not());
        env.cmd()
            .args(["-c", "--model", "cloud/big", "ask", "three"])
            .assert()
            .success()
            .stderr(contains(flagged).not());
        env.cmd()
            .args(["-c", "--model", "cloud/big", "ask", "four"])
            .assert()
            .success()
            .stderr(contains(flagged).not());
        // A new conversation holds nothing yet.
        env.cmd()
            .args(["--model", "cloud/big", "ask", "five"])
            .assert()
            .success()
            .stderr(contains(flagged).not());
    })
    .await
    .unwrap();
}

// Spec: "Local model emits a tagged tool call as text": a model on a local server gets text tool
// calls by default.
#[tokio::test(flavor = "multi_thread")]
async fn a_local_models_tagged_tool_call_runs() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_string_contains("\"role\":\"tool\""))
        .and(body_string_contains("pub fn add"))
        .respond_with(answer("It defines add."))
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(answer(
            r#"<tool_call>{"name": "read", "arguments": {"path": "src/lib.rs"}}</tool_call>"#,
        ))
        .with_priority(2)
        .mount(&server)
        .await;
    let env = Env::new(&format!(
        "model = \"mock/qwen\"\n[providers.mock]\nprotocol = \"openai-chat\"\nbase_url = \"{}/v1\"\n[profiles.\"mock/*\"]\ncontext_window = 32768\n",
        server.uri()
    ));
    std::fs::create_dir(env.ws.path().join("src")).unwrap();
    std::fs::write(env.ws.path().join("src/lib.rs"), "pub fn add() {}\n").unwrap();
    tokio::task::spawn_blocking(move || {
        env.cmd()
            .args(["ask", "what is in lib.rs?"])
            .assert()
            .success()
            .stdout(contains("It defines add."));
    })
    .await
    .unwrap();
}
