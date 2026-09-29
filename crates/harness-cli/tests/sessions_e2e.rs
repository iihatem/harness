use std::path::PathBuf;
use std::time::{Duration, Instant};

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

fn text_chunk(text: &str) -> Value {
    json!({"choices": [{"index": 0, "delta": {"content": text}, "finish_reason": "stop"}]})
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
            format!("model = \"mock/test-model\"\n[providers.mock]\nprotocol = \"openai-chat\"\nbase_url = \"{server_uri}/v1\"\n"),
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

    /// The session files of this workspace's project.
    fn session_files(&self) -> Vec<PathBuf> {
        let sessions = self.home.path().join("data/sessions");
        let mut out = Vec::new();
        for project in std::fs::read_dir(sessions).into_iter().flatten().flatten() {
            for file in std::fs::read_dir(project.path()).unwrap().flatten() {
                out.push(file.path());
            }
        }
        out.sort();
        out
    }
}

/// Answers `answer` to every request whose body contains `question`.
async fn answers(server: &MockServer, question: &str, answer: &str) {
    Mock::given(method("POST"))
        .and(body_string_contains(question))
        .respond_with(stream(&[text_chunk(answer)]))
        .mount(server)
        .await;
}

/// The messages of each request the server received, as `role: content` lines.
async fn conversations(server: &MockServer) -> Vec<Vec<String>> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| {
            let body: Value = serde_json::from_slice(&r.body).unwrap();
            body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .skip(1)
                .map(|m| {
                    format!(
                        "{}: {}",
                        m["role"].as_str().unwrap(),
                        m["content"].as_str().unwrap_or("")
                    )
                })
                .collect()
        })
        .collect()
}

// Spec: continue latest.
#[tokio::test(flavor = "multi_thread")]
async fn continue_resumes_the_latest_session() {
    let server = MockServer::start().await;
    answers(&server, "second question", "second answer").await;
    answers(&server, "first question", "first answer").await;
    let env = Env::new(&server.uri());
    tokio::task::spawn_blocking(move || {
        env.cmd().args(["ask", "first question"]).assert().success();
        env.cmd()
            .args(["-c", "ask", "second question"])
            .assert()
            .success()
            .stdout(contains("second answer"));
        assert_eq!(env.session_files().len(), 1);
    })
    .await
    .unwrap();
    assert_eq!(
        conversations(&server).await[1],
        [
            "user: first question",
            "assistant: first answer",
            "user: second question"
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_are_listed_and_resumed_by_id() {
    let server = MockServer::start().await;
    answers(&server, "question", "answer").await;
    let env = Env::new(&server.uri());
    let env = tokio::task::spawn_blocking(move || {
        env.cmd().args(["ask", "alpha question"]).assert().success();
        std::thread::sleep(Duration::from_millis(20));
        env.cmd().args(["ask", "beta question"]).assert().success();
        env
    })
    .await
    .unwrap();
    let files = env.session_files();
    assert_eq!(files.len(), 2);
    let listing = env.cmd().arg("--resume").output().unwrap();
    assert!(listing.status.success());
    let listing = String::from_utf8(listing.stdout).unwrap();
    let alpha = listing.find("alpha question").expect("listed");
    let beta = listing.find("beta question").expect("listed");
    assert!(beta < alpha, "most recent first:\n{listing}");
    let alpha_line = listing.lines().find(|l| l.contains("alpha")).unwrap();
    let id = alpha_line.split_whitespace().next().unwrap().to_string();
    let env = tokio::task::spawn_blocking(move || {
        env.cmd()
            .args(["--resume", &id, "ask", "gamma question"])
            .assert()
            .success();
        env.cmd()
            .args(["--resume", "../../x", "ask", "hi"])
            .assert()
            .code(2)
            .stderr(contains("there is no session ../../x"));
        env
    })
    .await
    .unwrap();
    assert_eq!(
        conversations(&server).await[2],
        [
            "user: alpha question",
            "assistant: answer",
            "user: gamma question"
        ]
    );
    drop(env);
}

#[tokio::test(flavor = "multi_thread")]
async fn continue_without_a_session_is_an_error() {
    let server = MockServer::start().await;
    let env = Env::new(&server.uri());
    tokio::task::spawn_blocking(move || {
        env.cmd()
            .args(["-c", "ask", "hi"])
            .assert()
            .code(2)
            .stderr(contains("no earlier session"));
    })
    .await
    .unwrap();
}

// Spec: entries survive a crash; truncated session files are tolerated.
#[tokio::test(flavor = "multi_thread")]
async fn a_killed_run_keeps_its_completed_turns() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_string_contains("three"))
        .respond_with(stream(&[text_chunk("never sent")]).set_delay(Duration::from_secs(60)))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    answers(&server, "one", "answer").await;
    let env = Env::new(&server.uri());
    let env = tokio::task::spawn_blocking(move || {
        env.cmd().args(["ask", "one"]).assert().success();
        env.cmd().args(["-c", "ask", "two"]).assert().success();
        env
    })
    .await
    .unwrap();
    let mut child = std::process::Command::new(BIN)
        .current_dir(env.ws.path())
        .env("HARNESS_HOME", env.home.path())
        .env("HARNESS_CREDENTIAL_STORE", "file")
        .args(["-c", "ask", "three"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let start = Instant::now();
    while server.received_requests().await.unwrap().len() < 3 {
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "the third request never came"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // A second run cannot use the session while the first holds it.
    let busy = env.cmd().args(["-c", "ask", "four"]).output().unwrap();
    assert_eq!(busy.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&busy.stderr).contains("open in another harness process"));
    child.kill().unwrap();
    child.wait().unwrap();

    let file = &env.session_files()[0];
    let saved = std::fs::read_to_string(file).unwrap();
    for text in ["\"one\"", "\"two\"", "\"three\""] {
        assert!(saved.contains(text), "{text} missing from {saved}");
    }
    // Simulate a write cut short, then continue: the complete entries load, with a warning.
    std::fs::write(file, format!("{saved}{{\"id\":\"x\",\"par")).unwrap();
    let env = tokio::task::spawn_blocking(move || {
        env.cmd()
            .args(["-c", "ask", "one more"])
            .assert()
            .success()
            .stderr(contains("warning: the last line"));
        env
    })
    .await
    .unwrap();
    // The killed turn's prompt and the next one are consecutive user messages, sent as one.
    let last = conversations(&server).await.pop().unwrap();
    assert_eq!(
        last,
        [
            "user: one",
            "assistant: answer",
            "user: two",
            "assistant: answer",
            "user: three\n\none more"
        ]
    );
    drop(env);
}

// Review D M5: the listing shows the id `--resume` looks up (the file name), and nothing from
// inside the file reaches the terminal unescaped.
#[tokio::test(flavor = "multi_thread")]
async fn the_session_list_shows_file_names_and_escapes_what_the_files_say() {
    let server = MockServer::start().await;
    answers(&server, "question", "answer").await;
    let env = Env::new(&server.uri());
    let env = tokio::task::spawn_blocking(move || {
        env.cmd().args(["ask", "a question"]).assert().success();
        env
    })
    .await
    .unwrap();
    let file = env.session_files()[0].clone();
    let id = file.file_stem().unwrap().to_str().unwrap().to_string();
    let text = std::fs::read_to_string(&file).unwrap();
    let mut lines: Vec<String> = text.lines().map(String::from).collect();
    let mut header: Value = serde_json::from_str(&lines[0]).unwrap();
    header["started_at"] = json!("\u{1b}]0;owned\u{7}2026");
    lines[0] = header.to_string();
    std::fs::write(&file, lines.join("\n") + "\n").unwrap();
    let listing = env.cmd().arg("--resume").output().unwrap();
    assert!(listing.status.success());
    let listing = String::from_utf8(listing.stdout).unwrap();
    assert!(
        !listing.contains('\u{1b}') && !listing.contains('\u{7}'),
        "{listing:?}"
    );
    assert!(listing.starts_with(&format!("{id}  ")), "{listing}");
}

// Review D I1, end to end: harness killed while a tool runs, then continued. Strict providers
// reject a tool call without a result, so the next request must carry one for every call.
#[tokio::test(flavor = "multi_thread")]
async fn a_run_killed_during_a_tool_call_can_be_continued() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_string_contains("start the long job"))
        .respond_with(stream(&[json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "id": "call_1", "type": "function",
            "function": {"name": "bash", "arguments": "{\"command\":\"sleep 20\"}"}}]}, "finish_reason": "tool_calls"}]})]))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    answers(&server, "what happened", "it was stopped").await;
    let env = Env::new(&server.uri());
    // Full access, so the command runs without approval or sandbox on every platform.
    let mut child = std::process::Command::new(BIN)
        .current_dir(env.ws.path())
        .env("HARNESS_HOME", env.home.path())
        .env("HARNESS_CREDENTIAL_STORE", "file")
        .args(["--mode", "full-access", "ask", "start the long job"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let start = Instant::now();
    loop {
        let saved = env
            .session_files()
            .first()
            .map(|f| std::fs::read_to_string(f).unwrap_or_default())
            .unwrap_or_default();
        if saved.contains("call_1") {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "the tool call was never saved"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    child.kill().unwrap();
    child.wait().unwrap();

    let env = tokio::task::spawn_blocking(move || {
        env.cmd()
            .args(["-c", "ask", "what happened?"])
            .assert()
            .success()
            .stdout(contains("it was stopped"));
        env
    })
    .await
    .unwrap();
    let requests = server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&requests.last().unwrap().body).unwrap();
    let messages = body["messages"].as_array().unwrap();
    let result = messages
        .iter()
        .position(|m| m["role"] == "tool" && m["tool_call_id"] == "call_1")
        .expect("a result for the call");
    assert_eq!(messages[result - 1]["tool_calls"][0]["id"], "call_1");
    assert!(
        messages[result]["content"]
            .as_str()
            .unwrap()
            .contains("harness stopped before this tool call finished"),
        "{messages:?}"
    );
    assert_eq!(messages[result + 1]["role"], "user");
    drop(env);
}
