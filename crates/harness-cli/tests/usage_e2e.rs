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

// `harness pricing update` fetches the table once, validates it and stores it; a failed update
// leaves the old table; and a session makes no connection to the pricing host.
const MODELS_DEV: &str = r#"{"openai":{"models":{"gpt-5":{"cost":{"input":1.5,"output":9}},"o3":{"cost":{"input":2,"output":8}}}}}"#;

async fn pricing_server(status: u16, body: &str) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api.json"))
        .respond_with(ResponseTemplate::new(status).set_body_string(body))
        .mount(&server)
        .await;
    server
}

#[tokio::test(flavor = "multi_thread")]
async fn pricing_update_stores_the_table_and_prints_its_date_and_size() {
    let models_dev = pricing_server(200, MODELS_DEV).await;
    let env = Env::new("http://127.0.0.1:9", "");
    let url = format!("{}/api.json", models_dev.uri());
    let env = tokio::task::spawn_blocking(move || {
        let out = env
            .cmd()
            .env("HARNESS_PRICING_URL", &url)
            .args(["pricing", "update"])
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(text.contains("2 models"), "{text}");
        assert!(text.contains("dated 20"), "{text}");
        let file = env.home.path().join("data/pricing.json");
        assert!(
            std::fs::read_to_string(&file)
                .unwrap()
                .contains("\"gpt-5\"")
        );
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        env
    })
    .await
    .unwrap();
    assert_eq!(models_dev.received_requests().await.unwrap().len(), 1);
    drop(env);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_pricing_update_exits_non_zero_and_keeps_the_old_table() {
    let models_dev = pricing_server(200, "this is not pricing data").await;
    let env = Env::new("http://127.0.0.1:9", "");
    let file = env.home.path().join("data/pricing.json");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, "old table").unwrap();
    let url = format!("{}/api.json", models_dev.uri());
    tokio::task::spawn_blocking(move || {
        let out = env
            .cmd()
            .env("HARNESS_PRICING_URL", &url)
            .args(["pricing", "update"])
            .output()
            .unwrap();
        assert!(!out.status.success(), "{out:?}");
        assert!(!out.stderr.is_empty());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "old table");
    })
    .await
    .unwrap();
}

// A session runs without the user running `harness pricing update`: no connection to models.dev.
#[tokio::test(flavor = "multi_thread")]
async fn a_session_never_fetches_prices() {
    let models_dev = pricing_server(200, MODELS_DEV).await;
    let server = MockServer::start().await;
    two_requests(&server).await;
    let env = Env::new(&server.uri(), "");
    let url = format!("{}/api.json", models_dev.uri());
    let env = tokio::task::spawn_blocking(move || {
        let _ = env
            .cmd()
            .env("HARNESS_PRICING_URL", &url)
            .args(["--model", "mock/test-model", "ask", "go"])
            .assert();
        let _ = env
            .cmd()
            .env("HARNESS_PRICING_URL", &url)
            .arg("usage")
            .output();
        env
    })
    .await
    .unwrap();
    assert!(models_dev.received_requests().await.unwrap().is_empty());
    drop(env);
}

#[test]
fn pricing_is_listed_in_help() {
    let env = Env::new("http://127.0.0.1:9", "");
    let help = stdout(&run_usage(&env, &["--help"]));
    assert!(help.contains("pricing"), "{help}");
    let help = stdout(&run_usage(&env, &["pricing", "--help"]));
    assert!(help.contains("update"), "{help}");
}

// With a baseline named, `harness usage` shows what the tokens that ran on a local model would
// have cost there; without one, no avoided figure.
#[tokio::test(flavor = "multi_thread")]
async fn usage_shows_the_avoided_figure_only_with_a_baseline() {
    let server = MockServer::start().await;
    let config = "[usage]\nbaseline = \"openai/gpt-5\"\n[pricing.\"openai/gpt-5\"]\ninput = 2.0\noutput = 2.0\n";
    let env = ask_twice(Env::new(&server.uri(), config), &server).await;
    let with = stdout(&run_usage(&env, &["usage"]));
    // 500 input and 30 output tokens, at $2 per million, are $0.00106.
    assert!(with.contains("Avoided vs openai/gpt-5: $0.0011"), "{with}");
    // The local server's own figures stay $0.00 billed and estimated.
    let row = with
        .lines()
        .find(|l| l.starts_with("mock/test-model"))
        .unwrap();
    assert_eq!(row.matches("$0.00").count(), 2, "{row}");
    std::fs::write(
        env.home.path().join("config/config.toml"),
        "[providers.mock]\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1:9/v1\"\n",
    )
    .unwrap();
    let without = stdout(&run_usage(&env, &["usage"]));
    assert!(!without.contains("Avoided"), "{without}");
}

// A budget stops a headless run before the request that would cross it, with exit code 4, and the
// `--json` output ends with the budget event and a turn-finished event with reason `budget`.
#[tokio::test(flavor = "multi_thread")]
async fn a_headless_budget_stop_exits_4() {
    let server = MockServer::start().await;
    two_requests(&server).await;
    // The mock server is on this machine, so a profile says it is hosted; its tokens cost $1 each.
    let config = "[budgets]\nsession_usd = 0.10\n[profiles.\"mock/*\"]\nlocal = false\n[pricing.\"mock/*\"]\ninput = 1000000\noutput = 1000000\n";
    let env = Env::new(&server.uri(), config);
    let (code, stdout) = tokio::task::spawn_blocking(move || {
        let out = env
            .cmd()
            .args(["--model", "mock/test-model", "ask", "--json", "go"])
            .output()
            .unwrap();
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    })
    .await
    .unwrap();
    assert_eq!(code, Some(4), "{stdout}");
    let events: Vec<Value> = stdout
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let n = events.len();
    assert_eq!(events[n - 1]["type"], "turn_finished");
    assert_eq!(events[n - 1]["reason"], "budget");
    let reached = events[n - 4..n - 1]
        .iter()
        .find(|e| e["type"] == "budget_reached")
        .expect("the budget event comes just before the turn's end");
    assert_eq!(reached["notice"]["budget"], "session");
    // Only the first request was sent.
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

// With the daily budget reached by billed requests, a headless run on a local model is not
// affected: both its requests go, and it exits 0. The same run on the billed account still
// stops with reason `budget` and exit 4.
#[tokio::test(flavor = "multi_thread")]
async fn a_reached_budget_does_not_stop_a_headless_run_on_a_local_model() {
    let server = MockServer::start().await;
    two_requests(&server).await;
    let billed = "[budgets]\ndaily_usd = 0.10\n[profiles.\"mock/*\"]\nlocal = false\n[pricing.\"mock/*\"]\ninput = 1000000\noutput = 1000000\n";
    let env = Env::new(&server.uri(), billed);
    let env = std::sync::Arc::new(env);
    let run = |env: std::sync::Arc<Env>| {
        tokio::task::spawn_blocking(move || {
            env.cmd()
                .args(["--model", "mock/test-model", "ask", "go"])
                .output()
                .unwrap()
                .status
                .code()
        })
    };
    assert_eq!(run(env.clone()).await.unwrap(), Some(4), "billed, first");
    // The same day, the same server, but now it is a server of the user's own.
    let local = billed.replace("[profiles.\"mock/*\"]\nlocal = false\n", "");
    std::fs::write(
        env.home.path().join("config/config.toml"),
        format!(
            "{local}\n[providers.mock]\nprotocol = \"openai-chat\"\nbase_url = \"{}/v1\"\n",
            server.uri()
        ),
    )
    .unwrap();
    assert_eq!(run(env.clone()).await.unwrap(), Some(0), "local");
    let ledger = env.ledger();
    assert!(
        ledger.iter().filter(|r| r["account"] == "local").count() >= 2,
        "{ledger:?}"
    );
}

// Headless runs never auto-resume: `harness ask` that hits a subscription limit exits with its M1
// error behaviour at once and does not wait for the reset.
#[tokio::test(flavor = "multi_thread")]
async fn a_headless_run_that_hits_a_limit_does_not_wait() {
    let server = MockServer::start().await;
    let reset = harness_core_now() + 3_600;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(429).set_body_json(
                json!({"error": {"type": "usage_limit_reached", "resets_at": reset}}),
            ),
        )
        .mount(&server)
        .await;
    let env = Env::new(&server.uri(), "");
    let started = std::time::Instant::now();
    let (code, stdout) = tokio::task::spawn_blocking(move || {
        let out = env
            .cmd()
            .args(["--model", "mock/test-model", "ask", "--json", "go"])
            .output()
            .unwrap();
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    })
    .await
    .unwrap();
    assert_eq!(code, Some(1), "{stdout}");
    assert!(started.elapsed() < std::time::Duration::from_secs(20));
    // The event is printed for scripts; nothing waits on it.
    assert!(stdout.contains("\"type\":\"limit_reached\""), "{stdout}");
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

fn harness_core_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn outcome_lines(env: &Env) -> Vec<Value> {
    let dir = env.home.path().join("data/outcomes");
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .map(|d| d.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
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

// `harness ask` writes an outcome line for its turn, with counts and no content.
#[tokio::test(flavor = "multi_thread")]
async fn ask_writes_an_outcome_line_without_content() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_string_contains("\"role\":\"tool\""))
        .respond_with(stream(&[text_chunk("All done"), usage_chunk(300, 20)]))
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(stream(&[
            tool_chunk("c1", "bash", r#"{"command":"cargo test -p billing"}"#),
            usage_chunk(200, 10),
        ]))
        .with_priority(2)
        .mount(&server)
        .await;
    let env = Env::new(&server.uri(), "");
    let env = tokio::task::spawn_blocking(move || {
        let _ = env
            .cmd()
            .args([
                "--model",
                "mock/test-model",
                "ask",
                "fix src/billing/invoice.rs",
            ])
            .assert();
        env
    })
    .await
    .unwrap();
    let lines = outcome_lines(&env);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0]["model"], "mock/test-model");
    assert_eq!(lines[0]["role"], "main");
    assert_eq!(lines[0]["selected_by"], "config");
    assert_eq!(lines[0]["tool_calls"], 1);
    assert_eq!(lines[0]["input_tokens"], 500);
    assert_eq!(lines[0]["output_tokens"], 30);
    assert_eq!(lines[0]["user_signal"], "continued");
    let file = std::fs::read_dir(env.home.path().join("data/outcomes"))
        .unwrap()
        .flatten()
        .next()
        .unwrap()
        .path();
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let text = std::fs::read_to_string(&file).unwrap();
    for forbidden in ["invoice.rs", "billing", "cargo test", "fix src"] {
        assert!(!text.contains(forbidden), "{forbidden}: {text}");
    }
}

// `[outcomes] enabled = false`: no outcome file, and `harness usage` still reports the requests.
#[tokio::test(flavor = "multi_thread")]
async fn with_outcomes_off_the_ledger_and_the_report_still_work() {
    let server = MockServer::start().await;
    let env = ask_twice(
        Env::new(&server.uri(), "[outcomes]\nenabled = false\n"),
        &server,
    )
    .await;
    assert!(!env.home.path().join("data/outcomes").exists());
    let report = stdout(&run_usage(&env, &["usage"]));
    assert!(report.contains("mock/test-model"), "{report}");
}

#[tokio::test(flavor = "multi_thread")]
async fn usage_export_writes_csv_and_jsonl_to_stdout() {
    let server = MockServer::start().await;
    let env = ask_twice(Env::new(&server.uri(), ""), &server).await;
    let csv = stdout(&run_usage(
        &env,
        &[
            "usage",
            "export",
            "--format",
            "csv",
            "--since",
            "2020-01-01",
        ],
    ));
    let lines: Vec<&str> = csv.lines().collect();
    assert_eq!(lines.len(), 3, "{csv}");
    assert!(lines[0].starts_with("t,time,session,"), "{csv}");
    let jsonl = stdout(&run_usage(&env, &["usage", "export"]));
    assert_eq!(jsonl.lines().count(), 2);
    let first: Value = serde_json::from_str(jsonl.lines().next().unwrap()).unwrap();
    assert_eq!(first["model"], "mock/test-model");
    let none = stdout(&run_usage(
        &env,
        &["usage", "export", "--since", "2999-01-01"],
    ));
    assert!(none.is_empty(), "{none}");
    let bad = run_usage(&env, &["usage", "export", "--format", "xml"]);
    assert!(!bad.status.success());
}

#[tokio::test(flavor = "multi_thread")]
async fn usage_forget_needs_a_range_and_forget_all_empties_the_report() {
    let server = MockServer::start().await;
    let env = ask_twice(Env::new(&server.uri(), ""), &server).await;
    // With neither flag: nothing is deleted, and it exits non-zero, saying so.
    let refused = run_usage(&env, &["usage", "forget"]);
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("--before"),
        "{refused:?}"
    );
    assert_eq!(env.ledger().len(), 2);
    assert_eq!(outcome_lines(&env).len(), 1);
    let both = run_usage(
        &env,
        &["usage", "forget", "--all", "--before", "2026-01-01"],
    );
    assert!(!both.status.success());
    let done = run_usage(&env, &["usage", "forget", "--all"]);
    assert!(done.status.success(), "{done:?}");
    assert!(env.ledger().is_empty());
    assert!(outcome_lines(&env).is_empty());
    let report = stdout(&run_usage(&env, &["usage"]));
    assert!(report.contains("No usage recorded yet"), "{report}");
    let help = stdout(&run_usage(&env, &["usage", "--help"]));
    assert!(help.contains("export") && help.contains("forget"), "{help}");
}

/// The README's text, which documents the commands and the settings the tests below run.
const README: &str = include_str!("../../../README.md");

/// The lines of the fenced block that follows `marker` in the README.
fn block_after(marker: &str) -> Vec<String> {
    let after = README
        .split(marker)
        .nth(1)
        .unwrap_or_else(|| panic!("the README has no {marker}"));
    let mut lines = after.lines().skip_while(|l| !l.starts_with("```"));
    lines.next();
    lines
        .take_while(|l| !l.starts_with("```"))
        .map(String::from)
        .collect()
}

// The commands the README documents run as written: against a ledger with records, a pricing
// server on this machine, and the user's own data directory.
#[tokio::test(flavor = "multi_thread")]
async fn the_documented_usage_commands_run_as_written() {
    let models_dev = pricing_server(200, MODELS_DEV).await;
    let server = MockServer::start().await;
    let env = ask_twice(Env::new(&server.uri(), ""), &server).await;
    let url = format!("{}/api.json", models_dev.uri());
    let commands = block_after("<!-- documented-commands -->");
    assert!(commands.len() >= 6, "{commands:?}");
    tokio::task::spawn_blocking(move || {
        for line in commands {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            // `harness usage export … > usage.csv`: the redirect is the shell's, not an argument.
            let command = line
                .split(" > ")
                .next()
                .unwrap()
                .split(" #")
                .next()
                .unwrap();
            let words: Vec<&str> = command.split_whitespace().collect();
            assert_eq!(words[0], "harness", "{line}");
            let out = env
                .cmd()
                .env("HARNESS_PRICING_URL", &url)
                .args(&words[1..])
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "`{line}` failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    })
    .await
    .unwrap();
}

// The configuration the README shows is accepted as written.
#[test]
fn the_documented_configuration_parses() {
    let config = block_after("<!-- documented-config -->").join("\n");
    assert!(
        config.contains("[budgets]")
            && config.contains("[pricing.")
            && config.contains("[outcomes]")
    );
    let env = Env::new("http://127.0.0.1:9", "");
    std::fs::write(env.home.path().join("config/config.toml"), config).unwrap();
    let out = run_usage(&env, &["usage"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
