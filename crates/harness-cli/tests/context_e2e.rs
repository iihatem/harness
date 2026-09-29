use assert_cmd::Command;
use predicates::str::contains;
use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const BIN: &str = env!("CARGO_BIN_EXE_harness");

fn sse(chunks: &[Value]) -> String {
    let mut body: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
    body.push_str("data: [DONE]\n\n");
    body
}

fn text_chunk(text: &str) -> Value {
    json!({"choices": [{"index": 0, "delta": {"content": text}, "finish_reason": "stop"}]})
}

fn tool_chunk(id: &str, name: &str, arguments: &str) -> Value {
    json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "id": id, "type": "function",
        "function": {"name": name, "arguments": arguments}}]}, "finish_reason": "tool_calls"}]})
}

fn stream(chunks: &[Value]) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(sse(chunks), "text/event-stream")
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
            .env("HARNESS_CREDENTIAL_STORE", "file")
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME")
            .env_remove("XDG_STATE_HOME");
        cmd
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn instruction_files_and_the_environment_reach_the_model() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_string_contains("Answer in French."))
        .and(body_string_contains("Use tabs."))
        .and(body_string_contains("Working directory: "))
        .respond_with(stream(&[text_chunk("d'accord")]))
        .mount(&server)
        .await;
    let env = Env::new(&server.uri());
    std::fs::write(
        env.home.path().join("config/AGENTS.md"),
        "Answer in French.\n",
    )
    .unwrap();
    std::fs::write(env.ws.path().join("AGENTS.md"), "Use tabs.\n").unwrap();
    tokio::task::spawn_blocking(move || {
        env.cmd()
            .args(["ask", "hi"])
            .assert()
            .success()
            .stdout(contains("d'accord"));
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn consecutive_requests_share_a_byte_identical_prefix() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_string_contains("\"role\":\"tool\""))
        .respond_with(stream(&[text_chunk("done")]))
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(stream(&[tool_chunk(
            "c1",
            "read",
            r#"{"path":"AGENTS.md"}"#,
        )]))
        .with_priority(2)
        .mount(&server)
        .await;
    let env = Env::new(&server.uri());
    std::fs::write(env.ws.path().join("AGENTS.md"), "Use tabs.\n").unwrap();
    tokio::task::spawn_blocking(move || {
        env.cmd().args(["ask", "go"]).assert().success();
    })
    .await
    .unwrap();
    let bodies: Vec<Value> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert_eq!(bodies.len(), 2);
    let prefix = |body: &Value| {
        (
            serde_json::to_string(&body["messages"][0]).unwrap(),
            serde_json::to_string(&body["tools"]).unwrap(),
        )
    };
    assert_eq!(prefix(&bodies[0]), prefix(&bodies[1]));
    assert_eq!(bodies[0]["messages"][0]["role"], "system");
}

#[tokio::test(flavor = "multi_thread")]
async fn large_instruction_files_and_bad_imports_are_reported() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(stream(&[text_chunk("ok")]))
        .mount(&server)
        .await;
    let env = Env::new(&server.uri());
    std::fs::write(
        env.ws.path().join("AGENTS.md"),
        format!("@/etc/passwd\n{}\n", "x".repeat(40_000)),
    )
    .unwrap();
    tokio::task::spawn_blocking(move || {
        env.cmd()
            .args(["ask", "hi"])
            .assert()
            .success()
            .stderr(contains("warning: skipped import @/etc/passwd"))
            .stderr(contains(
                "more than a quarter of the 32768-token context window",
            ));
    })
    .await
    .unwrap();
}

// Final review, critical 1 (probe p6): a folder under the home directory that is not a
// repository, such as an extracted archive, whose `AGENTS.md` links to a secret elsewhere in the
// home directory. The link is skipped with a warning and the secret never reaches the model.
#[tokio::test(flavor = "multi_thread")]
async fn outside_a_repository_an_instruction_file_linked_to_a_secret_is_not_sent() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(stream(&[text_chunk("ok")]))
        .mount(&server)
        .await;
    let env = Env::new(&server.uri());
    let base = tempfile::tempdir().unwrap();
    let home = base.path().canonicalize().unwrap().join("home");
    std::fs::create_dir_all(home.join(".aws")).unwrap();
    std::fs::write(home.join(".aws/credentials"), "SECRET_SENTINEL\n").unwrap();
    let evil = home.join("Downloads/evil");
    std::fs::create_dir_all(&evil).unwrap();
    std::os::unix::fs::symlink("../../.aws/credentials", evil.join("AGENTS.md")).unwrap();
    let output = tokio::task::spawn_blocking(move || {
        env.cmd()
            .current_dir(&evil)
            .env("HOME", &home)
            .args(["--mode", "plan", "ask", "hi"])
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("AGENTS.md: it links to") && stderr.contains("outside the project"),
        "{stderr}"
    );
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let body = String::from_utf8_lossy(&requests[0].body);
    assert!(!body.contains("SECRET_SENTINEL"), "{body}");
}
