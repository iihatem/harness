mod common;
use common::Isolate;

use std::os::unix::process::ExitStatusExt;
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
            .isolate()
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

// Review Focus: the model's final answer is model-controlled text, so a prompt-injected ANSI/OSC
// escape in it must never reach the terminal raw — but an ordinary multi-line answer must still
// print as multiple lines, not one line full of literal `\n`s.
#[tokio::test(flavor = "multi_thread")]
async fn the_final_answer_escapes_ansi_but_keeps_newlines() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(stream(&[text_chunk("\u{1b}[31mred\nline two")]))
        .mount(&server)
        .await;
    let env = Env::new(&server.uri(), "model = \"mock/test-model\"");
    let output =
        tokio::task::spawn_blocking(move || env.cmd().args(["ask", "hi"]).output().unwrap())
            .await
            .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(
        !stdout.contains('\u{1b}'),
        "raw ESC byte in stdout: {stdout:?}"
    );
    assert!(stdout.contains("\\u{1b}[31m"), "{stdout:?}");
    assert!(stdout.contains("red\nline two"), "{stdout:?}");
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

// Final review, M-3: a refused key is named by where it came from, with what fixes it.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_key_says_which_key_it_was() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401).set_body_string("Incorrect API key provided"))
        .mount(&server)
        .await;
    let keyed = format!(
        "model = \"keyed/m\"\n[providers.keyed]\nprotocol = \"openai-chat\"\nbase_url = \"{}/v1\"\napi_key_env = \"KEYED_API_KEY\"\n",
        server.uri()
    );
    let env = Env::new(&server.uri(), &keyed);
    tokio::task::spawn_blocking(move || {
        env.cmd()
            .env("KEYED_API_KEY", "sk-keyed-0123456789")
            .args(["ask", "hi"])
            .assert()
            .code(1)
            .stderr(contains("HTTP 401: Incorrect API key provided"))
            .stderr(contains("$KEYED_API_KEY"))
            .stderr(contains("unset KEYED_API_KEY"))
            .stderr(contains("`harness auth add keyed`"))
            .stderr(predicates::prelude::PredicateBooleanExt::not(contains(
                "sk-keyed",
            )));
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
        .isolate()
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

// Review Focus: a closed stdout pipe (e.g. `harness ask --json | head -c1`) must not panic the
// renderer and silently lose the exit code.
#[tokio::test(flavor = "multi_thread")]
async fn a_closed_stdout_pipe_does_not_panic_and_still_exits() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(stream(&[text_chunk("hello there")]).set_delay(Duration::from_millis(500)))
        .mount(&server)
        .await;
    let env = Env::new(&server.uri(), "model = \"mock/test-model\"");
    let mut child = std::process::Command::new(BIN)
        .args(["ask", "--json", "hi"])
        .current_dir(env.ws.path())
        .env("HARNESS_HOME", env.home.path())
        .isolate()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();

    // Read exactly one byte, then drop the read end: once every reader is gone, the next write
    // the child makes to stdout fails with a broken pipe.
    tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let mut byte = [0u8; 1];
        let _ = stdout.read_exact(&mut byte);
        drop(stdout);
    })
    .await
    .unwrap();

    let wait = tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let status = child.wait().unwrap();
        let mut stderr_text = String::new();
        let _ = stderr.read_to_string(&mut stderr_text);
        (status, stderr_text)
    });
    let (_status, stderr_text) = match tokio::time::timeout(Duration::from_secs(10), wait).await {
        Ok(joined) => joined.unwrap(),
        Err(_) => panic!("harness ask did not exit within 10s after its stdout pipe closed"),
    };

    assert!(!stderr_text.contains("panicked"), "{stderr_text}");
}

// Review Focus: full-access mode runs commands with no approval and no sandbox; the user should
// be warned.
#[tokio::test(flavor = "multi_thread")]
async fn full_access_mode_warns_once_on_stderr() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(stream(&[text_chunk("done")]))
        .mount(&server)
        .await;
    let env = Env::new(&server.uri(), "model = \"mock/test-model\"");
    tokio::task::spawn_blocking(move || {
        env.cmd()
            .args(["--mode", "full-access", "ask", "go"])
            .assert()
            .success()
            .stderr(contains(
                "warning: full-access mode: commands run without approval or sandbox",
            ));
    })
    .await
    .unwrap();
}

// Review Focus: a missing model must be reported promptly, without first blocking on stdin.
#[tokio::test(flavor = "multi_thread")]
async fn missing_model_exits_2_promptly_even_with_an_open_stdin_pipe() {
    let server = MockServer::start().await;
    let env = Env::new(&server.uri(), ""); // no model configured
    let mut child = std::process::Command::new(BIN)
        .args(["ask", "hi"])
        .current_dir(env.ws.path())
        .env("HARNESS_HOME", env.home.path())
        .isolate()
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Hold the write end open (never send EOF): if stdin were read before the model check, this
    // would hang the child instead of letting it exit promptly.
    let _stdin_writer = child.stdin.take().unwrap();

    let wait = tokio::task::spawn_blocking(move || child.wait().unwrap());
    let status = match tokio::time::timeout(Duration::from_secs(3), wait).await {
        Ok(joined) => joined.unwrap(),
        Err(_) => {
            panic!("harness ask hung waiting on stdin instead of exiting for a missing model")
        }
    };
    assert_eq!(status.code(), Some(2));
}

// Review Focus: an idle stdin pipe (open, but the writer never sends data or closes it) must not
// hang `harness ask` forever. HARNESS_STDIN_WAIT_MS shortens the first-data wait for the test.
#[tokio::test(flavor = "multi_thread")]
async fn idle_stdin_pipe_does_not_hang() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(stream(&[text_chunk("ok")]))
        .mount(&server)
        .await;
    let env = Env::new(&server.uri(), "model = \"mock/test-model\"");
    let mut child = std::process::Command::new(BIN)
        .args(["ask", "hi"])
        .current_dir(env.ws.path())
        .env("HARNESS_HOME", env.home.path())
        .isolate()
        .env("HARNESS_STDIN_WAIT_MS", "300")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Hold the write end open (never write, never close): a real "idle pipe" from a parent process.
    let _stdin_writer = child.stdin.take().unwrap();
    let pid = nix::unistd::Pid::from_raw(child.id() as i32);

    let wait = tokio::task::spawn_blocking(move || child.wait_with_output().unwrap());
    let output = match tokio::time::timeout(Duration::from_secs(10), wait).await {
        Ok(joined) => joined.unwrap(),
        Err(_) => {
            // Bound the failure instead of hanging the test suite forever: kill the leaked child.
            let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL);
            panic!("harness ask hung on an idle stdin pipe instead of timing out (child killed)")
        }
    };
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("ok"));
    assert!(String::from_utf8_lossy(&output.stderr).contains("no stdin data received"));
}

// Review Focus: a slow producer piping into stdin (data arrives after the first-data wait starts,
// then the pipe closes) must still have its data included in full, with no further timeout.
#[tokio::test(flavor = "multi_thread")]
async fn slowly_piped_stdin_is_still_included() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_string_contains("late data line"))
        .respond_with(stream(&[text_chunk("saw the late data")]))
        .mount(&server)
        .await;
    let env = Env::new(&server.uri(), "model = \"mock/test-model\"");
    let mut child = std::process::Command::new(BIN)
        .args(["ask", "hi"])
        .current_dir(env.ws.path())
        .env("HARNESS_HOME", env.home.path())
        .isolate()
        .env("HARNESS_STDIN_WAIT_MS", "2000")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin_writer = child.stdin.take().unwrap();
    let pid = nix::unistd::Pid::from_raw(child.id() as i32);
    let _writer = tokio::task::spawn_blocking(move || {
        use std::io::Write as _;
        std::thread::sleep(Duration::from_millis(500));
        let _ = stdin_writer.write_all(b"late data line\n");
        drop(stdin_writer);
    });

    let wait = tokio::task::spawn_blocking(move || child.wait_with_output().unwrap());
    let output = match tokio::time::timeout(Duration::from_secs(10), wait).await {
        Ok(joined) => joined.unwrap(),
        Err(_) => {
            // Bound the failure instead of hanging the test suite forever: kill the leaked child
            // (the background writer task holds no further resources once the child is gone).
            let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL);
            panic!("harness ask did not exit within 10s with slowly piped stdin (child killed)")
        }
    };
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("saw the late data"));
}

// Review Focus: Ctrl+C must be honoured while `ask::run` is still waiting on piped stdin, including
// during the unbounded read-to-EOF phase (data has started arriving, but the pipe never closes — e.g.
// `tail -f | harness ask ...`). Before this fix the SIGINT listener task wasn't spawned until after
// the stdin wait returned, so a pipe that never closes could only be killed, never interrupted.
#[tokio::test(flavor = "multi_thread")]
async fn ctrl_c_during_a_never_closing_stdin_exits_130() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(stream(&[text_chunk("too late")]))
        .mount(&server)
        .await;
    let env = Env::new(&server.uri(), "model = \"mock/test-model\"");

    // `ask::run` installs its SIGINT handler first thing, but a SIGINT that lands before the child
    // gets that far (a slow start under load) kills it outright, which says nothing about the code
    // under test. So a child killed by the signal itself is retried with a longer delay; only an
    // exit code counts as a result.
    let mut delay = Duration::from_millis(700);
    let mut attempts = 1;
    let status = loop {
        let status = interrupt_while_stdin_stays_open(&env, delay).await;
        if status.signal() == Some(nix::sys::signal::Signal::SIGINT as i32) && attempts < 5 {
            attempts += 1;
            delay *= 2;
            continue;
        }
        break status;
    };
    assert_eq!(
        status.code(),
        Some(130),
        "{status:?} after {attempts} attempt(s)"
    );

    let received = server.received_requests().await.unwrap_or_default();
    assert!(
        received.is_empty(),
        "expected no chat request before SIGINT, got {}",
        received.len()
    );
}

/// Runs `harness ask` with a stdin pipe that gets one line and is never closed, sends SIGINT after
/// `delay`, and returns how the child ended.
async fn interrupt_while_stdin_stays_open(env: &Env, delay: Duration) -> std::process::ExitStatus {
    let mut child = std::process::Command::new(BIN)
        .args(["--model", "mock/test-model", "ask", "hi"])
        .current_dir(env.ws.path())
        .env("HARNESS_HOME", env.home.path())
        .isolate()
        .env("HARNESS_STDIN_WAIT_MS", "2000")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin_writer = child.stdin.take().unwrap();
    {
        use std::io::Write as _;
        // Write immediately so the first-data wait resolves almost instantly and the reader moves
        // into the unbounded read-to-EOF phase; the handle is then kept open (never closed), so that
        // phase never finishes on its own.
        stdin_writer.write_all(b"partial\n").unwrap();
    }
    let pid = nix::unistd::Pid::from_raw(child.id() as i32);

    tokio::time::sleep(delay).await;
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGINT).unwrap();

    let wait = tokio::task::spawn_blocking(move || child.wait().unwrap());
    let status = match tokio::time::timeout(Duration::from_secs(5), wait).await {
        Ok(joined) => joined.unwrap(),
        Err(_) => {
            let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL);
            panic!(
                "harness ask did not exit within 5s after SIGINT during a never-closing stdin pipe (child killed)"
            )
        }
    };
    drop(stdin_writer);
    status
}

// Review Focus: rule text and model-supplied command text reach the terminal in warnings and
// approval reasons; raw control characters there could rewrite what the user sees.
#[tokio::test(flavor = "multi_thread")]
async fn control_characters_reach_the_terminal_only_escaped() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_string_contains("\"role\":\"tool\""))
        .respond_with(stream(&[text_chunk("done")]))
        .with_priority(1)
        .mount(&server)
        .await;
    let args = json!({"command": "echo \u{1b}c\u{7}ok"}).to_string();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(stream(&[tool_chunk("c1", "bash", &args)]))
        .with_priority(2)
        .mount(&server)
        .await;
    let env = Env::new(
        &server.uri(),
        "model = \"mock/test-model\"\n[permissions]\nallow = [\"shell:\\u001B[2Jrm*\"]\n",
    );
    let output = tokio::task::spawn_blocking(move || {
        env.cmd()
            .env("HARNESS_SANDBOX", "none")
            .args(["--mode", "ask", "ask", "go"])
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(3), "{stderr}");
    assert!(
        !stderr.contains(['\u{1b}', '\u{7}']),
        "raw control characters on stderr: {stderr:?}"
    );
    let line = |needle: &str| {
        stderr
            .lines()
            .find(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("no line with {needle:?} in {stderr:?}"))
            .to_string()
    };
    assert!(
        line("names an unknown tool").contains("shell:\\u{1b}[2Jrm*"),
        "{stderr}"
    );
    assert!(
        line("blocked:").contains("echo \\u{1b}c\\u{7}ok"),
        "{stderr}"
    );
}

// Review Focus: a config parse error echoed a snippet of the offending source line. If that line
// contains a raw control byte (e.g. pasted from a terminal capture), it must not reach stderr raw
// — the same guarantee `terminal_safe` already gives rule text and model output. Since re-review
// F, R2, the line is not echoed at all (it can hold a key): only where it is.
#[tokio::test(flavor = "multi_thread")]
async fn invalid_config_with_an_escape_byte_is_escaped_on_stderr() {
    let server = MockServer::start().await;
    let env = Env::new(&server.uri(), "mdo\u{1b}e = \"auto\"");
    let output =
        tokio::task::spawn_blocking(move || env.cmd().args(["ask", "hi"]).output().unwrap())
            .await
            .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("config.toml"), "{stderr}");
    assert!(
        !stderr.contains('\u{1b}'),
        "raw ESC byte on stderr: {stderr:?}"
    );
    assert!(stderr.contains("line 1, column 4"), "{stderr}");
    assert!(!stderr.contains("mdo"), "{stderr}");
}

// Review Focus: `registry::resolve`'s errors (BadId/UnknownProvider) embed the raw model id or
// provider name verbatim, and that text can come straight from the config's `model = "…"`. It
// must reach stderr escaped, exactly like the config-parse-error case above.
#[tokio::test(flavor = "multi_thread")]
async fn a_model_id_with_an_escape_byte_is_escaped_on_stderr() {
    let server = MockServer::start().await;
    // TOML basic strings disallow a literal control byte; `\u001b` is the escape sequence that
    // decodes to a real ESC character in the resulting config string.
    let env = Env::new(&server.uri(), "model = \"unknownprov\\u001b/x\"");
    let output =
        tokio::task::spawn_blocking(move || env.cmd().args(["ask", "hi"]).output().unwrap())
            .await
            .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("unknown provider"), "{stderr}");
    assert!(
        !stderr.contains('\u{1b}'),
        "raw ESC byte on stderr: {stderr:?}"
    );
    assert!(stderr.contains("\\u{1b}"), "{stderr}");
}

#[test]
fn no_subcommand_explains_that_interactive_mode_is_not_ready() {
    Command::new(BIN)
        .assert()
        .code(2)
        .stderr(contains("harness ask"));
}
