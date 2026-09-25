use std::process::Stdio;
use std::time::{Duration, Instant};

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

struct Env {
    home: TempDir,
    ws: TempDir,
}

impl Env {
    /// A HARNESS_HOME whose config defines provider `mock` at `server_uri`, and a workspace that looks
    /// like a git work tree (so the default mode is `auto`).
    fn new(server_uri: &str, extra_config: &str) -> Env {
        let home = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("config")).unwrap();
        std::fs::write(
            home.path().join("config/config.toml"),
            format!("{extra_config}\n[providers.mock]\nprotocol = \"openai-chat\"\nbase_url = \"{server_uri}/v1\"\n"),
        )
        .unwrap();
        std::fs::create_dir(ws.path().join(".git")).unwrap();
        Env { home, ws }
    }

    fn cmd(&self) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.current_dir(self.ws.path())
            .env("HARNESS_HOME", self.home.path())
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME")
            .env_remove("XDG_STATE_HOME");
        cmd
    }
}

async fn write_then_answer(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_string_contains("\"role\":\"tool\""))
        .respond_with(stream(&[text_chunk("Created hello.txt")]))
        .with_priority(1)
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(stream(&[tool_chunk(
            "c1",
            "write",
            r#"{"path":"hello.txt","content":"hi\n"}"#,
        )]))
        .with_priority(2)
        .mount(server)
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ask_runs_a_multi_step_task_and_prints_the_final_answer() {
    let server = MockServer::start().await;
    write_then_answer(&server).await;
    let env = Env::new(&server.uri(), "");
    let env = tokio::task::spawn_blocking(move || {
        env.cmd()
            .args(["--model", "mock/test-model", "ask", "make", "hello.txt"])
            .assert()
            .success()
            .stdout(contains("Created hello.txt"));
        env
    })
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(env.ws.path().join("hello.txt")).unwrap(),
        "hi\n"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn json_output_is_one_event_per_line_ending_with_turn_finished() {
    let server = MockServer::start().await;
    write_then_answer(&server).await;
    let env = Env::new(&server.uri(), "model = \"mock/test-model\"");
    let output = tokio::task::spawn_blocking(move || {
        env.cmd().args(["ask", "--json", "go"]).output().unwrap()
    })
    .await
    .unwrap();
    assert!(output.status.success());
    let events: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).expect("every stdout line is JSON"))
        .collect();
    assert_eq!(events.first().unwrap()["type"], "turn_started");
    assert_eq!(events.last().unwrap()["type"], "turn_finished");
    assert_eq!(events.last().unwrap()["reason"], "completed");
}

#[tokio::test(flavor = "multi_thread")]
async fn ask_mode_blocks_writes_and_exits_3() {
    let server = MockServer::start().await;
    write_then_answer(&server).await;
    let env = Env::new(&server.uri(), "model = \"mock/test-model\"");
    let env = tokio::task::spawn_blocking(move || {
        env.cmd()
            .args(["--mode", "ask", "ask", "go"])
            .assert()
            .code(3)
            .stderr(contains("blocked"));
        env
    })
    .await
    .unwrap();
    assert!(!env.ws.path().join("hello.txt").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn piped_stdin_is_appended_to_the_prompt() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_string_contains("review this"))
        .and(body_string_contains("diff --git a/x b/x"))
        .respond_with(stream(&[text_chunk("looks fine")]))
        .mount(&server)
        .await;
    let env = Env::new(&server.uri(), "model = \"mock/test-model\"");
    tokio::task::spawn_blocking(move || {
        env.cmd()
            .args(["ask", "review this"])
            .write_stdin("diff --git a/x b/x\n")
            .assert()
            .success()
            .stdout(contains("looks fine"));
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn provider_errors_exit_1() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401).set_body_string("bad key"))
        .mount(&server)
        .await;
    let env = Env::new(&server.uri(), "model = \"mock/test-model\"");
    tokio::task::spawn_blocking(move || {
        env.cmd()
            .args(["ask", "hi"])
            .assert()
            .code(1)
            .stderr(contains("HTTP 401"));
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_model_and_unknown_provider_exit_2() {
    let server = MockServer::start().await;
    let env = Env::new(&server.uri(), "");
    tokio::task::spawn_blocking(move || {
        env.cmd()
            .args(["ask", "hi"])
            .assert()
            .code(2)
            .stderr(contains("no model configured"));
        env.cmd()
            .args(["--model", "nope/x", "ask", "hi"])
            .assert()
            .code(2)
            .stderr(contains("unknown provider"));
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_config_exits_2_with_the_file_and_line() {
    let server = MockServer::start().await;
    let env = Env::new(&server.uri(), "mdoe = \"auto\"");
    tokio::task::spawn_blocking(move || {
        env.cmd()
            .args(["ask", "hi"])
            .assert()
            .code(2)
            .stderr(contains("config.toml"))
            .stderr(contains("mdoe"));
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn models_lists_configured_provider_models() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [{"id": "m1"}]})))
        .mount(&server)
        .await;
    let env = Env::new(&server.uri(), "");
    tokio::task::spawn_blocking(move || {
        env.cmd()
            .arg("models")
            .assert()
            .success()
            .stdout(contains("mock/m1"));
    })
    .await
    .unwrap();
}

// Review Focus: Ctrl+C during `harness ask`.
//
// The child's SIGINT handler is installed as the very first thing `ask::run` does, before it sends
// the chat request. So instead of racing a fixed sleep against process/runtime start-up (flaky under
// load), we wait for wiremock to confirm the request actually arrived: that proves the handler is
// already live, making the SIGINT below deterministic.
#[tokio::test(flavor = "multi_thread")]
async fn ctrl_c_interrupts_the_run_and_exits_130() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(stream(&[text_chunk("too late")]).set_delay(Duration::from_secs(30)))
        .mount(&server)
        .await;
    let env = Env::new(&server.uri(), "model = \"mock/test-model\"");
    let mut child = std::process::Command::new(BIN)
        .args(["ask", "hi"])
        .current_dir(env.ws.path())
        .env("HARNESS_HOME", env.home.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let received = server.received_requests().await.unwrap_or_default();
        if !received.is_empty() {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("the mock server never received a chat request within 10s");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let started = Instant::now();
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(child.id() as i32),
        nix::sys::signal::Signal::SIGINT,
    )
    .unwrap();
    let status = tokio::task::spawn_blocking(move || child.wait().unwrap())
        .await
        .unwrap();
    let elapsed = started.elapsed();

    assert_eq!(status.code(), Some(130));
    assert!(elapsed < Duration::from_secs(5));
}

#[test]
fn no_subcommand_explains_that_interactive_mode_is_not_ready() {
    Command::new(BIN)
        .assert()
        .code(2)
        .stderr(contains("harness ask"));
}
