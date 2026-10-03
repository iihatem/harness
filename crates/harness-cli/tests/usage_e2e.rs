//! M2a: the usage ledger and the outcome log, through the real binary against a mock server.

mod common;
use common::Isolate;

use std::os::unix::fs::PermissionsExt;

use assert_cmd::Command;
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

fn stream(chunks: &[Value]) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(sse(chunks), "text/event-stream")
}

fn text_chunk(text: &str) -> Value {
    json!({"choices": [{"index": 0, "delta": {"content": text}, "finish_reason": "stop"}]})
}

fn tool_chunk(id: &str, name: &str, arguments: &str) -> Value {
    json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "id": id, "type": "function",
        "function": {"name": name, "arguments": arguments}}]}, "finish_reason": "tool_calls"}]})
}

fn usage_chunk(prompt: u64, completion: u64) -> Value {
    json!({"choices": [], "usage": {"prompt_tokens": prompt, "completion_tokens": completion}})
}

struct Env {
    home: TempDir,
    ws: TempDir,
}

impl Env {
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
            .isolate()
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME")
            .env_remove("XDG_STATE_HOME");
        cmd
    }

    fn usage_dir(&self) -> std::path::PathBuf {
        self.home.path().join("data/usage")
    }

    /// The ledger's lines, parsed.
    fn ledger(&self) -> Vec<Value> {
        let mut files: Vec<_> = std::fs::read_dir(self.usage_dir())
            .map(|d| d.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        files.retain(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("ledger-"))
        });
        files.sort();
        files
            .iter()
            .flat_map(|f| {
                std::fs::read_to_string(f)
                    .unwrap()
                    .lines()
                    .map(|l| serde_json::from_str(l).unwrap())
                    .collect::<Vec<Value>>()
            })
            .collect()
    }
}

async fn two_requests(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_string_contains("\"role\":\"tool\""))
        .respond_with(stream(&[text_chunk("All done"), usage_chunk(300, 20)]))
        .with_priority(1)
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(stream(&[
            tool_chunk("c1", "bash", r#"{"command":"cat .env"}"#),
            usage_chunk(200, 10),
        ]))
        .with_priority(2)
        .mount(server)
        .await;
}

// The ledger is written by `harness ask`: a record per request, local and free for a server
// on this machine, private to the user, and without the prompt, paths or commands.
#[tokio::test(flavor = "multi_thread")]
async fn ask_writes_a_record_per_request_without_content() {
    let server = MockServer::start().await;
    two_requests(&server).await;
    let env = Env::new(&server.uri(), "");
    let env = tokio::task::spawn_blocking(move || {
        // Whether the command ran or was held for approval does not matter here.
        let _ = env
            .cmd()
            .args([
                "--model",
                "mock/test-model",
                "ask",
                "what does src/secret.rs do?",
            ])
            .assert();
        env
    })
    .await
    .unwrap();
    let records = env.ledger();
    assert_eq!(records.len(), 2, "{records:?}");
    assert_eq!(records[0]["outcome"], "ok");
    assert_eq!(records[0]["model"], "mock/test-model");
    assert_eq!(records[0]["account"], "local");
    assert_eq!(records[0]["billed_usd"], 0.0);
    assert_eq!(records[0]["input"], 200);
    assert_eq!(records[1]["input"], 300);
    let file = std::fs::read_dir(env.usage_dir())
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .unwrap();
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let text = std::fs::read_to_string(&file).unwrap();
    for forbidden in ["src/secret.rs", ".env", "what does", "cat "] {
        assert!(!text.contains(forbidden), "{forbidden}: {text}");
    }
    let workspace = env.ws.path().canonicalize().unwrap();
    assert!(!text.contains(workspace.to_str().unwrap()), "{text}");
}

fn run_usage(env: &Env, args: &[&str]) -> std::process::Output {
    env.cmd().args(args).output().unwrap()
}

fn stdout(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

async fn ask_twice(env: Env, server: &MockServer) -> Env {
    two_requests(server).await;
    tokio::task::spawn_blocking(move || {
        let _ = env
            .cmd()
            .args(["--model", "mock/test-model", "ask", "go"])
            .assert();
        env
    })
    .await
    .unwrap()
}

// `harness usage --by model` prints one row per model with requests and tokens, offline.
#[tokio::test(flavor = "multi_thread")]
async fn usage_reports_by_model_from_the_ledger() {
    let server = MockServer::start().await;
    let env = ask_twice(Env::new(&server.uri(), ""), &server).await;
    let out = run_usage(&env, &["usage", "--by", "model"]);
    assert!(out.status.success(), "{out:?}");
    let text = stdout(&out);
    assert!(text.starts_with("Usage by model"), "{text}");
    let row = text
        .lines()
        .find(|l| l.starts_with("mock/test-model"))
        .expect(&text);
    // Two requests, 200 + 300 input tokens, on a local server: $0.00, known.
    assert!(row.contains(" 2 ") && row.contains("500"), "{row}");
    assert!(row.contains("$0.00"), "{row}");
    for by in ["provider", "day", "project"] {
        let out = run_usage(&env, &["usage", "--by", by]);
        assert!(out.status.success(), "{by}: {out:?}");
        assert!(stdout(&out).starts_with(&format!("Usage by {by}")), "{by}");
    }
}

// Deleting `index.sqlite` and running the report again shows the same totals as before.
#[tokio::test(flavor = "multi_thread")]
async fn usage_gives_the_same_report_after_the_cache_is_deleted() {
    let server = MockServer::start().await;
    let env = ask_twice(Env::new(&server.uri(), ""), &server).await;
    let before = stdout(&run_usage(&env, &["usage"]));
    let cache = env.usage_dir().join("index.sqlite");
    assert!(cache.exists());
    std::fs::remove_file(&cache).unwrap();
    let after = stdout(&run_usage(&env, &["usage"]));
    assert_eq!(before, after);
    assert!(cache.exists(), "the cache is built again");
}

#[test]
fn usage_with_an_empty_ledger_says_m1_sessions_are_not_included() {
    let env = Env::new("http://127.0.0.1:9", "");
    let out = run_usage(&env, &["usage"]);
    assert!(out.status.success(), "{out:?}");
    let text = stdout(&out);
    assert!(text.contains("No usage recorded yet"), "{text}");
    assert!(text.contains("M1 sessions is not included"), "{text}");
}

#[test]
fn usage_refuses_a_group_or_date_it_does_not_know() {
    let env = Env::new("http://127.0.0.1:9", "");
    for args in [
        vec!["usage", "--by", "colour"],
        vec!["usage", "--since", "2026-02-30"],
        vec!["usage", "--until", "tomorrow"],
    ] {
        let out = run_usage(&env, &args);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {out:?}");
        assert!(!out.stderr.is_empty(), "{args:?}");
    }
}

#[test]
fn usage_is_listed_in_help() {
    let env = Env::new("http://127.0.0.1:9", "");
    let help = stdout(&run_usage(&env, &["--help"]));
    assert!(help.contains("usage"), "{help}");
}
