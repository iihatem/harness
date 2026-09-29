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

fn text(text: &str) -> ResponseTemplate {
    stream(&[
        json!({"choices": [{"index": 0, "delta": {"content": text}, "finish_reason": "stop"}]}),
    ])
}

fn call(id: &str, name: &str, arguments: Value) -> ResponseTemplate {
    stream(&[
        json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "id": id, "type": "function",
        "function": {"name": name, "arguments": arguments.to_string()}}]}, "finish_reason": "tool_calls"}]}),
    ])
}

/// A model that answers the first request with `first`, the request carrying the result of call
/// `c1` with `second`, and the one carrying the result of `c2` with `done`.
async fn script(server: &MockServer, first: ResponseTemplate, second: ResponseTemplate) {
    Mock::given(method("POST"))
        .and(body_string_contains(r#""tool_call_id":"c2""#))
        .respond_with(text("done"))
        .with_priority(1)
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(body_string_contains(r#""tool_call_id":"c1""#))
        .respond_with(second)
        .with_priority(2)
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .respond_with(first)
        .with_priority(3)
        .mount(server)
        .await;
}

/// A HARNESS_HOME whose config uses provider `mock` at `server_uri`, and a workspace that looks
/// like a git work tree (so the default mode is `auto`).
fn env(server_uri: &str) -> (TempDir, TempDir) {
    let home = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("config")).unwrap();
    std::fs::write(
        home.path().join("config/config.toml"),
        format!("model = \"mock/test-model\"\n[providers.mock]\nprotocol = \"openai-chat\"\nbase_url = \"{server_uri}/v1\"\n"),
    )
    .unwrap();
    std::fs::create_dir(ws.path().join(".git")).unwrap();
    (home, ws)
}

fn cmd(home: &TempDir, ws: &TempDir) -> Command {
    let mut cmd = Command::new(BIN);
    cmd.current_dir(ws.path())
        .env("HARNESS_HOME", home.path())
        .env("HARNESS_CREDENTIAL_STORE", "file")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("XDG_STATE_HOME");
    cmd
}

#[tokio::test(flavor = "multi_thread")]
async fn init_writes_a_new_agents_md() {
    let server = MockServer::start().await;
    script(
        &server,
        call(
            "c1",
            "write",
            json!({"path": "AGENTS.md", "content": "# Project\nRun `cargo test`.\n"}),
        ),
        text("done"),
    )
    .await;
    let (home, ws) = env(&server.uri());
    tokio::task::spawn_blocking(move || {
        cmd(&home, &ws).args(["ask", "/init"]).assert().success();
        assert_eq!(
            std::fs::read_to_string(ws.path().join("AGENTS.md")).unwrap(),
            "# Project\nRun `cargo test`.\n"
        );
    })
    .await
    .unwrap();
    let first = &server.received_requests().await.unwrap()[0];
    assert!(String::from_utf8_lossy(&first.body).contains("write an AGENTS.md"));
}

// Spec: existing AGENTS.md.
#[tokio::test(flavor = "multi_thread")]
async fn init_shows_but_does_not_write_a_replacement_without_confirmation() {
    let server = MockServer::start().await;
    script(
        &server,
        call("c1", "read", json!({"path": "AGENTS.md"})),
        call(
            "c2",
            "write",
            json!({"path": "AGENTS.md", "content": "new rules\n"}),
        ),
    )
    .await;
    let (home, ws) = env(&server.uri());
    std::fs::write(ws.path().join("AGENTS.md"), "old rules\n").unwrap();
    tokio::task::spawn_blocking(move || {
        cmd(&home, &ws)
            .args(["ask", "/init"])
            .assert()
            .code(3)
            .stderr(contains("confirm rule `write:AGENTS.md`"))
            .stderr(contains("proposed content of AGENTS.md:\nnew rules"));
        assert_eq!(
            std::fs::read_to_string(ws.path().join("AGENTS.md")).unwrap(),
            "old rules\n"
        );
    })
    .await
    .unwrap();
}

// Review A: the model may improve an existing AGENTS.md with `edit` rather than `write`; a
// headless run shows that change too.
#[tokio::test(flavor = "multi_thread")]
async fn init_shows_but_does_not_make_an_edit_without_confirmation() {
    let server = MockServer::start().await;
    script(
        &server,
        call("c1", "read", json!({"path": "AGENTS.md"})),
        call(
            "c2",
            "edit",
            json!({"path": "AGENTS.md", "old_string": "old rules", "new_string": "new rules\nmore"}),
        ),
    )
    .await;
    let (home, ws) = env(&server.uri());
    std::fs::write(ws.path().join("AGENTS.md"), "# Rules\nold rules\n").unwrap();
    tokio::task::spawn_blocking(move || {
        cmd(&home, &ws)
            .args(["ask", "/init"])
            .assert()
            .code(3)
            .stderr(contains("confirm rule `write:AGENTS.md`"))
            .stderr(contains(
                "proposed edit of AGENTS.md, replacing:\nold rules\nwith:\nnew rules\nmore",
            ));
        assert_eq!(
            std::fs::read_to_string(ws.path().join("AGENTS.md")).unwrap(),
            "# Rules\nold rules\n"
        );
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn init_runs_shell_commands_read_only() {
    let server = MockServer::start().await;
    script(
        &server,
        call("c1", "bash", json!({"command": "touch made-by-init"})),
        text("done"),
    )
    .await;
    let (home, ws) = env(&server.uri());
    tokio::task::spawn_blocking(move || {
        cmd(&home, &ws).args(["ask", "/init"]).output().unwrap();
        assert!(!ws.path().join("made-by-init").exists());
    })
    .await
    .unwrap();
}
