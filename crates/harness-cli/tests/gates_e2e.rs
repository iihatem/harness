//! Gates in a whole `harness ask` run, against a mock model.

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

fn env(server_uri: &str, extra_config: &str) -> (TempDir, TempDir) {
    let home = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("config")).unwrap();
    std::fs::write(
        home.path().join("config/config.toml"),
        format!("model = \"mock/test-model\"\n[providers.mock]\nprotocol = \"openai-chat\"\nbase_url = \"{server_uri}/v1\"\n{extra_config}"),
    )
    .unwrap();
    (home, ws)
}

fn cmd(home: &TempDir, ws: &TempDir) -> Command {
    let mut cmd = Command::new(BIN);
    cmd.current_dir(ws.path())
        .env("HARNESS_HOME", home.path())
        .isolate()
        .env("HARNESS_SANDBOX", "none")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("XDG_STATE_HOME");
    cmd
}

fn events(stdout: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(stdout)
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn write_result(events: &[Value]) -> String {
    events
        .iter()
        .find(|e| e["type"] == "tool_call_finished")
        .and_then(|e| e["output"].as_str())
        .unwrap()
        .to_string()
}

// Spec "Lint failure after an edit": through the binary, the configured command's output is in
// the edit's result.
#[tokio::test(flavor = "multi_thread")]
async fn a_configured_lint_failure_is_in_the_edit_result() {
    let server = MockServer::start().await;
    write_then_answer(&server).await;
    let (home, ws) = env(
        &server.uri(),
        "[gates]\nafter_edit = \"echo 'hello.txt:1:1: E001 bad'; exit 1\"\n",
    );
    let output = tokio::task::spawn_blocking(move || {
        cmd(&home, &ws)
            .args(["--mode", "full-access", "ask", "--json", "make hello.txt"])
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    let events = events(&output.stdout);
    let result = write_result(&events);
    assert!(result.starts_with("Wrote 3 bytes"), "{result}");
    assert!(result.contains("exit code 1"), "{result}");
    assert!(result.contains("hello.txt:1:1: E001 bad"), "{result}");
    assert!(events.iter().any(|e| e["type"] == "gate_result"
        && e["gate"] == "after_edit"
        && e["status"] == "failed"));
}

// Spec "Headless run" (detection): `harness ask` in a workspace with a Cargo.toml and no [gates]
// shows no proposal and runs no gate.
#[tokio::test(flavor = "multi_thread")]
async fn a_headless_run_neither_proposes_nor_runs_a_detected_gate() {
    let server = MockServer::start().await;
    write_then_answer(&server).await;
    let (home, ws) = env(&server.uri(), "");
    std::fs::write(ws.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
    let output = tokio::task::spawn_blocking(move || {
        let output = cmd(&home, &ws)
            .args(["--mode", "full-access", "ask", "--json", "make hello.txt"])
            .output()
            .unwrap();
        assert!(!home.path().join("data/trust.toml").exists());
        output
    })
    .await
    .unwrap();
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout).to_string()
        + &String::from_utf8_lossy(&output.stderr);
    assert!(!text.contains("cargo test"), "{text}");
    assert!(
        !events(&output.stdout)
            .iter()
            .any(|e| e["type"] == "gate_result")
    );
}

// Spec "Gate commands use the sandbox and the permission rules": a gate command that needs an
// approval nobody can give is blocked, the model is told, and the run exits 3.
#[tokio::test(flavor = "multi_thread")]
async fn a_gate_command_that_needs_approval_blocks_a_headless_run() {
    let server = MockServer::start().await;
    write_then_answer(&server).await;
    // Auto mode without a sandbox: file edits run, and every shell command needs approval.
    let (home, ws) = env(&server.uri(), "[gates]\nafter_edit = \"echo linted\"\n");
    let output = tokio::task::spawn_blocking(move || {
        let output = cmd(&home, &ws)
            .args(["--mode", "auto", "ask", "--json", "make hello.txt"])
            .output()
            .unwrap();
        assert!(ws.path().join("hello.txt").exists());
        output
    })
    .await
    .unwrap();
    assert_eq!(output.status.code(), Some(3));
    let events = events(&output.stdout);
    let result = write_result(&events);
    assert!(result.contains("after_edit gate blocked"), "{result}");
    assert!(events.iter().any(|e| e["type"] == "action_blocked"));
}

// Spec "A headless run ends on a gate failure": the final turn-finished event carries the reason
// `gate_failed`.
#[tokio::test(flavor = "multi_thread")]
async fn a_headless_run_that_ends_on_a_gate_failure_says_gate_failed() {
    let server = MockServer::start().await;
    write_then_answer(&server).await;
    let (home, ws) = env(
        &server.uri(),
        "[gates]\ntest = \"echo '1 test failed'; exit 1\"\n",
    );
    let output = tokio::task::spawn_blocking(move || {
        cmd(&home, &ws)
            .args(["--mode", "full-access", "ask", "--json", "make hello.txt"])
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    let events = events(&output.stdout);
    let last = events.last().unwrap();
    assert_eq!(last["type"], "turn_finished");
    assert_eq!(last["reason"], "gate_failed");
    assert!(events.iter().any(|e| {
        e["type"] == "gate_result"
            && e["gate"] == "test"
            && e["tail"]
                .as_str()
                .is_some_and(|t| t.contains("1 test failed"))
    }));
    assert_eq!(output.status.code(), Some(5));
}

// Spec "No gates configured": a finished turn runs no gate command.
#[tokio::test(flavor = "multi_thread")]
async fn without_gates_a_turn_that_wrote_a_file_runs_nothing_at_its_end() {
    let server = MockServer::start().await;
    write_then_answer(&server).await;
    let (home, ws) = env(&server.uri(), "");
    let output = tokio::task::spawn_blocking(move || {
        cmd(&home, &ws)
            .args(["--mode", "full-access", "ask", "--json", "make hello.txt"])
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(output.status.success());
    assert!(
        !events(&output.stdout)
            .iter()
            .any(|e| e["type"] == "gate_result")
    );
}
