//! Local models in `harness ask`: the window the server really runs the model with, and a
//! conversation held on local models that is continued on a hosted one.

use assert_cmd::Command;
use predicates::prelude::*;
use predicates::str::contains;
use serde_json::json;
use tempfile::TempDir;
use wiremock::matchers::{method, path};
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
            .env("HARNESS_CREDENTIAL_STORE", "file")
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

// Decision 14: continuing a conversation held on local models with a hosted model sends what it
// holds, tool output included, to that provider: harness says so.
#[tokio::test(flavor = "multi_thread")]
async fn a_local_conversation_continued_on_a_hosted_model_is_flagged() {
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
    let flagged = "ran on local models so far; continuing it on cloud/big sends it";
    tokio::task::spawn_blocking(move || {
        env.cmd()
            .args(["--model", "mock/small", "ask", "one"])
            .assert()
            .success();
        env.cmd()
            .args(["-c", "--model", "mock/small", "ask", "two"])
            .assert()
            .success()
            .stderr(contains(flagged).not());
        env.cmd()
            .args(["-c", "--model", "cloud/big", "ask", "three"])
            .assert()
            .success()
            .stderr(contains(flagged))
            .stderr(contains("tool output included, to cloud"));
        // Once a hosted model has answered in it, the conversation has left the machine already.
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
