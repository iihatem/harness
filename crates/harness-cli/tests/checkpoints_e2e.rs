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

fn stream(chunks: &[Value]) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(sse(chunks), "text/event-stream")
}

/// A model that writes `hello.txt` and then says `done`.
async fn write_then_answer(server: &MockServer) {
    Mock::given(method("POST"))
        .and(body_string_contains("\"role\":\"tool\""))
        .respond_with(stream(&[json!({"choices": [{"index": 0, "delta": {"content": "done"}, "finish_reason": "stop"}]})]))
        .with_priority(1)
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .respond_with(stream(&[json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "id": "c1", "type": "function",
            "function": {"name": "write", "arguments": "{\"path\":\"hello.txt\",\"content\":\"hi\\n\"}"}}]}, "finish_reason": "tool_calls"}]})]))
        .with_priority(2)
        .mount(server)
        .await;
}

fn env(server_uri: &str) -> (TempDir, TempDir) {
    let home = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("config")).unwrap();
    std::fs::write(
        home.path().join("config/config.toml"),
        format!("model = \"mock/test-model\"\n[providers.mock]\nprotocol = \"openai-chat\"\nbase_url = \"{server_uri}/v1\"\n"),
    )
    .unwrap();
    (home, ws)
}

fn cmd(home: &TempDir, ws: &TempDir) -> Command {
    let mut cmd = Command::new(BIN);
    cmd.current_dir(ws.path())
        .env("HARNESS_HOME", home.path())
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("XDG_STATE_HOME");
    cmd
}

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_that_writes_takes_a_checkpoint() {
    let server = MockServer::start().await;
    write_then_answer(&server).await;
    let (home, ws) = env(&server.uri());
    // Not a git repository: checkpoints work anyway (ask mode would block the write).
    let output = tokio::task::spawn_blocking(move || {
        let output = cmd(&home, &ws)
            .args(["--mode", "auto", "ask", "--json", "make hello.txt"])
            .output()
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(ws.path().join("hello.txt")).unwrap(),
            "hi\n"
        );
        assert!(home.path().join("data/checkpoints").is_dir());
        output
    })
    .await
    .unwrap();
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let events: Vec<Value> = stdout
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let checkpoint = events
        .iter()
        .position(|e| e["type"] == "checkpoint_created")
        .expect("a checkpoint");
    let write = events
        .iter()
        .position(|e| e["type"] == "tool_call_finished")
        .unwrap();
    assert!(checkpoint < write);
}

// Spec: git missing.
#[tokio::test(flavor = "multi_thread")]
async fn without_git_checkpoints_are_disabled_and_turns_proceed() {
    let server = MockServer::start().await;
    write_then_answer(&server).await;
    let (home, ws) = env(&server.uri());
    tokio::task::spawn_blocking(move || {
        cmd(&home, &ws)
            .env("PATH", "/nonexistent")
            .args(["--mode", "auto", "ask", "make hello.txt"])
            .assert()
            .success()
            .stderr(contains(
                "warning: checkpoints are disabled: git was not found on PATH",
            ))
            .stdout(contains("done"));
        assert_eq!(
            std::fs::read_to_string(ws.path().join("hello.txt")).unwrap(),
            "hi\n"
        );
    })
    .await
    .unwrap();
}
