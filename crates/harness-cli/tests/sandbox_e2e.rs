use std::process::Command as StdCommand;

use assert_cmd::Command;
use predicates::str::contains;
use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const BIN: &str = env!("CARGO_BIN_EXE_harness");

/// Skips the test when this host has no OS sandbox, unless CI has set
/// `HARNESS_REQUIRE_LINUX_SANDBOX=1` (the Linux CI job does), in which case that would silently
/// hide a broken sandbox backend — so it panics instead.
fn host_has_sandbox() -> bool {
    let ok = harness_sandbox::detect(harness_sandbox::SandboxSettings::default()).is_some();
    if !ok {
        if std::env::var("HARNESS_REQUIRE_LINUX_SANDBOX").as_deref() == Ok("1") {
            panic!(
                "HARNESS_REQUIRE_LINUX_SANDBOX=1 but harness_sandbox::detect() found no sandbox on this host"
            );
        }
        eprintln!("skipping: no OS sandbox on this host");
    }
    ok
}

fn sse(chunks: &[Value]) -> String {
    let mut body: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
    body.push_str("data: [DONE]\n\n");
    body
}

fn stream(chunks: &[Value]) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(sse(chunks), "text/event-stream")
}

/// The model runs `command` with the bash tool once, then answers "done".
async fn bash_then_done(server: &MockServer, command: &str) {
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_string_contains("\"role\":\"tool\""))
        .respond_with(stream(&[json!({"choices": [{"index": 0, "delta": {"content": "done"}, "finish_reason": "stop"}]})]))
        .with_priority(1)
        .mount(server)
        .await;
    let args = json!({ "command": command }).to_string();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(stream(&[json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "id": "c1",
            "type": "function", "function": {"name": "bash", "arguments": args}}]}, "finish_reason": "tool_calls"}]})]))
        .with_priority(2)
        .mount(server)
        .await;
}

struct Env {
    home: TempDir,
    ws: TempDir,
}

impl Env {
    fn new(server_uri: &str, extra_config: &str) -> Env {
        let env = Env {
            home: tempfile::tempdir().unwrap(),
            ws: tempfile::tempdir().unwrap(),
        };
        std::fs::create_dir_all(env.home.path().join("config")).unwrap();
        std::fs::write(
            env.home.path().join("config/config.toml"),
            format!("model = \"mock/m\"\n{extra_config}\n[providers.mock]\nprotocol = \"openai-chat\"\nbase_url = \"{server_uri}/v1\"\n"),
        )
        .unwrap();
        let git = StdCommand::new("git")
            .args(["init", "-q"])
            .current_dir(env.ws.path())
            .status()
            .unwrap();
        assert!(git.success());
        env
    }

    fn cmd(&self) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.current_dir(self.ws.path())
            .env("HARNESS_HOME", self.home.path())
            .env_remove("HARNESS_SANDBOX")
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME")
            .env_remove("XDG_STATE_HOME");
        cmd
    }
}

async fn run_bash(
    command: &str,
    extra_config: &str,
    setup: impl FnOnce(&Env),
) -> (Env, std::process::Output) {
    let server = MockServer::start().await;
    bash_then_done(&server, command).await;
    let env = Env::new(&server.uri(), extra_config);
    setup(&env);
    tokio::task::spawn_blocking(move || {
        let out = env.cmd().args(["ask", "--json", "go"]).output().unwrap();
        (env, out)
    })
    .await
    .unwrap()
}

fn tool_output(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|e| e["type"] == "tool_call_finished")
        .map(|e| e["output"].as_str().unwrap_or_default().to_string())
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread")]
async fn auto_mode_runs_commands_in_the_sandbox_without_approval() {
    if !host_has_sandbox() {
        return;
    }
    let (env, out) = run_bash("echo hi > made.txt", "", |_| {}).await;
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(env.ws.path().join("made.txt")).unwrap(),
        "hi\n"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn writes_outside_the_workspace_are_blocked_headless() {
    if !host_has_sandbox() {
        return;
    }
    let target = std::path::PathBuf::from(std::env::var("HOME").unwrap())
        .join(format!(".harness-e2e-{}", std::process::id()));
    let (_env, out) = run_bash(&format!("touch {}", target.display()), "", |_| {}).await;
    let escaped = target.exists();
    let _ = std::fs::remove_file(&target);
    assert!(
        !escaped,
        "the sandbox let a write outside the workspace through"
    );
    assert_eq!(out.status.code(), Some(3), "{}", tool_output(&out));
}

#[tokio::test(flavor = "multi_thread")]
async fn network_access_is_blocked_headless() {
    if !host_has_sandbox() {
        return;
    }
    let (_env, out) = run_bash("curl -sS --max-time 5 https://example.com", "", |_| {}).await;
    assert_eq!(out.status.code(), Some(3), "{}", tool_output(&out));
    assert!(
        tool_output(&out).contains("[the sandbox blocked"),
        "{}",
        tool_output(&out)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn git_commit_works_in_the_sandbox() {
    if !host_has_sandbox() {
        return;
    }
    // The commit identity is set on the host, not via harness: `git -c ...` always needs
    // approval (it can run arbitrary programs through keys like `core.pager`/`alias.*`), which
    // is an earlier ruling unrelated to what this test is checking — that a plain commit works
    // inside the sandbox.
    let (env, out) = run_bash("git commit -q --allow-empty -m sandboxed", "", |env| {
        for args in [
            ["config", "user.email", "a@b.c"],
            ["config", "user.name", "a"],
            ["config", "commit.gpgsign", "false"],
        ] {
            let status = StdCommand::new("git")
                .args(args)
                .current_dir(env.ws.path())
                .status()
                .unwrap();
            assert!(status.success());
        }
    })
    .await;
    assert_eq!(out.status.code(), Some(0), "{}", tool_output(&out));
    let log = StdCommand::new("git")
        .args(["log", "--oneline"])
        .current_dir(env.ws.path())
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&log.stdout).contains("sandboxed"));
}

#[tokio::test(flavor = "multi_thread")]
#[cfg_attr(
    target_os = "linux",
    ignore = "Landlock cannot protect .git/hooks inside a writable workspace; see README Known limitations"
)]
async fn planting_a_git_hook_fails_in_the_sandbox() {
    if !host_has_sandbox() {
        return;
    }
    let (env, out) = run_bash("echo 'echo pwned' > .git/hooks/pre-commit", "", |_| {}).await;
    assert!(!env.ws.path().join(".git/hooks/pre-commit").exists());
    assert_eq!(out.status.code(), Some(3), "{}", tool_output(&out));
    assert!(
        tool_output(&out).contains("[the sandbox blocked"),
        "{}",
        tool_output(&out)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn deny_rules_refuse_commands() {
    let (_env, out) = run_bash(
        "echo hi",
        "[permissions]\ndeny = [\"bash:echo*\"]\n",
        |_| {},
    )
    .await;
    assert_eq!(out.status.code(), Some(0));
    assert!(
        tool_output(&out).contains("denied"),
        "{}",
        tool_output(&out)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn destructive_commands_need_approval_headless() {
    let (_env, out) = run_bash("git reset --hard", "", |_| {}).await;
    assert_eq!(out.status.code(), Some(3));
}

#[tokio::test(flavor = "multi_thread")]
async fn disabling_the_sandbox_makes_every_command_need_approval() {
    let server = MockServer::start().await;
    bash_then_done(&server, "echo hi").await;
    let env = Env::new(&server.uri(), "");
    tokio::task::spawn_blocking(move || {
        env.cmd()
            .env("HARNESS_SANDBOX", "none")
            .args(["ask", "go"])
            .assert()
            .code(3)
            .stderr(contains("sandbox is disabled by HARNESS_SANDBOX=none"));
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_rule_tool_warns_once_and_json_output_stays_parseable() {
    let server = MockServer::start().await;
    bash_then_done(&server, "echo hi").await;
    let env = Env::new(&server.uri(), "[permissions]\nallow = [\"shell:rm*\"]\n");
    tokio::task::spawn_blocking(move || {
        let out = env.cmd().args(["ask", "--json", "go"]).output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        for line in stdout.lines() {
            assert!(
                serde_json::from_str::<Value>(line).is_ok(),
                "not JSON: {line}"
            );
        }
        let stderr = String::from_utf8_lossy(&out.stderr);
        let warnings = stderr.matches("names an unknown tool").count();
        assert_eq!(warnings, 1, "{stderr}");
    })
    .await
    .unwrap();
}
