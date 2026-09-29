//! Model profiles in `harness ask`: the context window and request settings come from the
//! profile of the model in use.

use assert_cmd::Command;
use predicates::prelude::*;
use predicates::str::contains;
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

struct Env {
    home: TempDir,
    ws: TempDir,
}

impl Env {
    /// Provider `mock` at `server_uri`, and `extra` config (profiles).
    fn new(server_uri: &str, extra: &str) -> Env {
        let home = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("config")).unwrap();
        std::fs::write(
            home.path().join("config/config.toml"),
            format!(
                "model = \"mock/coder\"\n[providers.mock]\nprotocol = \"openai-chat\"\nbase_url = \"{server_uri}/v1\"\n{extra}"
            ),
        )
        .unwrap();
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

async fn body_of_last_request(server: &MockServer) -> Value {
    let requests = server.received_requests().await.unwrap();
    requests.last().unwrap().body_json().unwrap()
}

// Spec: "Unknown context window".
#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_window_is_assumed_small_with_one_warning() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(answer("ok"))
        .mount(&server)
        .await;
    let env = Env::new(&server.uri(), "");
    let output =
        tokio::task::spawn_blocking(move || env.cmd().args(["ask", "hi"]).output().unwrap())
            .await
            .unwrap();
    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    let warning = "the context window of mock/coder is unknown; assuming 8192 tokens";
    assert_eq!(stderr.matches(warning).count(), 1, "{stderr}");
    assert!(stderr.contains("[profiles.\"mock/coder\"]"), "{stderr}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_profile_sets_the_window_and_the_request() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(answer("ok"))
        .mount(&server)
        .await;
    let profile = "[profiles.\"mock/*\"]\ncontext_window = 16384\ntemperature = 0.2\nmax_output_tokens = 2048\n";
    let env = Env::new(&server.uri(), profile);
    // About 5,000 tokens of instructions: more than a quarter of 16,384.
    std::fs::write(env.ws.path().join("AGENTS.md"), "x".repeat(20_000)).unwrap();
    tokio::task::spawn_blocking(move || {
        env.cmd()
            .args(["ask", "hi"])
            .assert()
            .success()
            .stderr(contains("the 16384-token context window"))
            .stderr(contains("is unknown").not());
    })
    .await
    .unwrap();
    let body = body_of_last_request(&server).await;
    assert_eq!(body["temperature"], 0.2);
    assert_eq!(body["max_tokens"], 2048);
}
