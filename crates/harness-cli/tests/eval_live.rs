//! Live mode, against a mock model: `harness ask` runs on each task, the task's test decides, and
//! the events give the metrics. The same code runs against a real model with `cargo xtask eval
//! run`.

use std::{path::Path, time::Duration};

use harness_core::edit_format::EditFormat;
use serde_json::{Value, json};
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};
use xtask::{live, task};

const BIN: &str = env!("CARGO_BIN_EXE_harness");

fn sse(chunks: &[Value]) -> ResponseTemplate {
    let mut body: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
    body.push_str("data: [DONE]\n\n");
    ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
}

fn call(id: &str, name: &str, arguments: Value) -> ResponseTemplate {
    sse(&[
        json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "id": id, "type": "function",
            "function": {"name": name, "arguments": arguments.to_string()}}]}, "finish_reason": "tool_calls"}]}),
        json!({"choices": [], "usage": {"prompt_tokens": 10, "completion_tokens": 7}}),
    ])
}

fn text(said: &str) -> ResponseTemplate {
    sse(&[
        json!({"choices": [{"index": 0, "delta": {"content": said}, "finish_reason": "stop"}]}),
        json!({"choices": [], "usage": {"prompt_tokens": 10, "completion_tokens": 3}}),
    ])
}

/// The model's replies, one request each, in order; then "done" for any more.
async fn replies(server: &MockServer, replies: Vec<ResponseTemplate>) {
    let count = replies.len() as u32;
    for (i, reply) in replies.into_iter().enumerate() {
        Mock::given(method("POST"))
            .respond_with(reply)
            .up_to_n_times(1)
            .with_priority(1 + i as u8)
            .mount(server)
            .await;
    }
    Mock::given(method("POST"))
        .respond_with(text("done"))
        .with_priority(1 + count as u8)
        .mount(server)
        .await;
}

fn config(server: &MockServer, dir: &Path) -> std::path::PathBuf {
    let path = dir.join("user-config.toml");
    std::fs::write(
        &path,
        format!(
            "[providers.mock]\nprotocol = \"openai-chat\"\nbase_url = \"{}/v1\"\n",
            server.uri()
        ),
    )
    .unwrap();
    path
}

fn tasks(ids: &[&str]) -> Vec<task::Task> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../eval/tasks");
    task::load_all(&root)
        .unwrap()
        .into_iter()
        .filter(|t| ids.contains(&t.id.as_str()))
        .collect()
}

fn options(server: &MockServer, dir: &Path, format: EditFormat, n: u32) -> live::Options {
    live::Options {
        model: "mock/test-model".into(),
        format,
        runs: n,
        harness: BIN.into(),
        user_config: Some(config(server, dir)),
        timeout: Duration::from_secs(120),
        // What keeps the run away from the developer's keychain and provider keys.
        env: vec![("HARNESS_TEST_NO_KEYCHAIN".into(), "1".into())],
    }
}

const GUARD_EDIT: &str = "    return sum(xs) / len(xs)";

fn fix() -> Value {
    json!({"path": "app.py", "old_string": GUARD_EDIT,
        "new_string": "    if not xs:\n        return 0.0\n    return sum(xs) / len(xs)"})
}

// Spec "Live run": each task runs, and the report shows the five metrics for the model and format.
#[tokio::test(flavor = "multi_thread")]
async fn a_model_that_fixes_the_task_passes_with_clean_metrics() {
    let server = MockServer::start().await;
    replies(
        &server,
        vec![
            call("c1", "read", json!({"path": "app.py"})),
            call("c2", "edit", fix()),
            text("fixed"),
        ],
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let options = options(&server, dir.path(), EditFormat::StrReplace, 1);
    let tasks = tasks(&["p01-average-empty"]);
    let report = tokio::task::spawn_blocking(move || live::run(&tasks, &options))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(report.runs.len(), 1);
    let s = &report.summary;
    assert_eq!(
        (
            s.runs,
            s.pass_rate,
            s.first_try_apply_rate,
            s.format_error_rate
        ),
        (1, 1.0, 1.0, 0.0)
    );
    assert_eq!(s.retries_per_run, 0.0);
    // 7 + 7 + 3 tokens in the three replies.
    assert_eq!(s.output_tokens_per_run, 17.0);
    assert_eq!(
        (report.model.as_str(), report.format),
        ("mock/test-model", EditFormat::StrReplace)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_first_edit_that_fails_counts_as_a_format_error_and_a_retry() {
    let server = MockServer::start().await;
    let wrong = json!({"path": "app.py", "old_string": "no such text", "new_string": "x"});
    replies(
        &server,
        vec![
            call("c1", "read", json!({"path": "app.py"})),
            call("c2", "edit", wrong),
            call("c3", "edit", fix()),
            text("fixed"),
        ],
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let options = options(&server, dir.path(), EditFormat::StrReplace, 1);
    let tasks = tasks(&["p01-average-empty"]);
    let report = tokio::task::spawn_blocking(move || live::run(&tasks, &options))
        .await
        .unwrap()
        .unwrap();
    let s = &report.summary;
    assert_eq!(s.pass_rate, 1.0);
    assert_eq!(s.first_try_apply_rate, 0.0);
    assert_eq!(s.format_error_rate, 0.5);
    assert_eq!(s.retries_per_run, 1.0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_model_that_changes_nothing_fails_the_task() {
    let server = MockServer::start().await;
    replies(&server, vec![text("I looked and it is fine")]).await;
    let dir = tempfile::tempdir().unwrap();
    let options = options(&server, dir.path(), EditFormat::StrReplace, 1);
    let tasks = tasks(&["p01-average-empty"]);
    let report = tokio::task::spawn_blocking(move || live::run(&tasks, &options))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(report.summary.pass_rate, 0.0);
    assert_eq!(report.runs[0].events.first_edit_applied, None);
}

// `-n`: each task runs that many times, each in a fresh copy.
#[tokio::test(flavor = "multi_thread")]
async fn each_task_runs_n_times() {
    let server = MockServer::start().await;
    replies(&server, vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let options = options(&server, dir.path(), EditFormat::StrReplace, 3);
    let tasks = tasks(&["p01-average-empty", "p04-fizzbuzz-range"]);
    let report = tokio::task::spawn_blocking(move || live::run(&tasks, &options))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(report.runs.len(), 6);
    assert_eq!(
        report
            .runs
            .iter()
            .filter(|r| r.task == "p04-fizzbuzz-range")
            .count(),
        3
    );
    assert_eq!(report.runs.iter().map(|r| r.run).max(), Some(3));
}

// The format reaches the model: its profile in the run's configuration sets the edit tool.
#[tokio::test(flavor = "multi_thread")]
async fn the_format_chosen_is_the_one_the_model_is_offered() {
    let server = MockServer::start().await;
    replies(&server, vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let options = options(&server, dir.path(), EditFormat::ApplyPatch, 1);
    let tasks = tasks(&["p01-average-empty"]);
    tokio::task::spawn_blocking(move || live::run(&tasks, &options))
        .await
        .unwrap()
        .unwrap();
    let requests = server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    let names: Vec<&str> = body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"apply_patch") && !names.contains(&"edit"),
        "{names:?}"
    );
}

// A run that never ends is stopped, and counts as a failure.
#[tokio::test(flavor = "multi_thread")]
async fn a_run_that_takes_too_long_is_stopped_and_fails() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(text("late").set_delay(Duration::from_secs(30)))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let mut options = options(&server, dir.path(), EditFormat::StrReplace, 1);
    options.timeout = Duration::from_secs(2);
    let tasks = tasks(&["p01-average-empty"]);
    let started = std::time::Instant::now();
    let report = tokio::task::spawn_blocking(move || live::run(&tasks, &options))
        .await
        .unwrap()
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(20));
    assert_eq!(report.summary.pass_rate, 0.0);
    assert!(
        report.runs[0]
            .error
            .as_deref()
            .is_some_and(|e| e.contains("timed out")),
        "{:?}",
        report.runs[0]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_report_is_saved_as_json_under_a_name_with_the_date_model_and_format() {
    let server = MockServer::start().await;
    replies(&server, vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let options = options(&server, dir.path(), EditFormat::Hashline, 1);
    let tasks = tasks(&["p01-average-empty"]);
    let report = tokio::task::spawn_blocking(move || live::run(&tasks, &options))
        .await
        .unwrap()
        .unwrap();
    let results = dir.path().join("results");
    let path = live::save(&report, &results).unwrap();
    let name = path.file_name().unwrap().to_string_lossy().into_owned();
    assert!(name.ends_with("-mock-test-model-hashline.json"), "{name}");
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(saved["model"], "mock/test-model");
    assert_eq!(saved["format"], "hashline");
    for metric in [
        "pass_rate",
        "first_try_apply_rate",
        "format_error_rate",
        "retries_per_run",
        "output_tokens_per_run",
    ] {
        assert!(saved["summary"][metric].is_number(), "{metric}");
    }
    assert!(saved["runs"].is_array() && saved["date"].is_string());
}
