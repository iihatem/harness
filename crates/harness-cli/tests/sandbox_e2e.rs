use std::process::Command as StdCommand;

use assert_cmd::Command;
use harness_core::tool::GitProtection;
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

/// On Linux with a sandbox, whether this host gets the basic git-protection tier (`Some(true)`)
/// or the full one (`Some(false)`); `None` elsewhere. With `HARNESS_EXPECT_LINUX_TIER` set (CI
/// sets `basic` or `full`), any other tier — including no sandbox at all — fails the test loudly,
/// rather than this quietly returning `None` and every caller skipping instead.
fn linux_basic_tier() -> Option<bool> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let expected = std::env::var("HARNESS_EXPECT_LINUX_TIER").unwrap_or_default();
    let detected = harness_sandbox::detect(harness_sandbox::SandboxSettings::default());
    let Some(sandbox) = detected else {
        assert!(
            expected.is_empty(),
            "HARNESS_EXPECT_LINUX_TIER={expected} but this host has no sandbox at all"
        );
        return None;
    };
    let protection = sandbox.git_protection();
    let basic = matches!(protection, GitProtection::Basic { .. });
    if !expected.is_empty() {
        let tier = if basic { "basic" } else { "full" };
        assert_eq!(tier, expected, "{protection:?}");
    }
    Some(basic)
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
            // A known window, so that the only warnings are the sandbox's.
            format!("model = \"mock/m\"\n{extra_config}\n[providers.mock]\nprotocol = \"openai-chat\"\nbase_url = \"{server_uri}/v1\"\n[profiles.\"mock/*\"]\ncontext_window = 32768\n"),
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
            .env("HARNESS_CREDENTIAL_STORE", "file")
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
async fn a_sandboxed_commands_exit_status_and_output_reach_the_result() {
    if !host_has_sandbox() {
        return;
    }
    // On Linux harness reaps orphans as a child subreaper; the command it waits for is never one.
    let (_env, out) = run_bash("echo out; exit 7", "", |_| {}).await;
    assert_eq!(out.status.code(), Some(0), "{}", tool_output(&out));
    assert!(
        tool_output(&out).starts_with("exit code 7\nout\n"),
        "{}",
        tool_output(&out)
    );
}

/// What the Linux git-metadata guard says reaches the tool result: here, that it could not scan
/// the whole workspace (an ignore file it cannot use), which does not block the command.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn what_the_linux_guard_reports_reaches_the_tool_result() {
    if !host_has_sandbox() {
        return;
    }
    let (_env, out) = run_bash("echo hi", "", |env| {
        std::os::unix::fs::symlink("elsewhere", env.ws.path().join(".gitignore")).unwrap();
    })
    .await;
    assert_eq!(out.status.code(), Some(0), "{}", tool_output(&out));
    assert!(
        tool_output(&out).contains("harness could not scan the whole workspace"),
        "{}",
        tool_output(&out)
    );
}

/// The spec's "Planting a hook in the Linux basic tier": the hook is moved to the quarantine in
/// the data directory, the tool result says so, and the command counts as blocked (exit 3).
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn a_hook_planted_in_the_linux_basic_tier_is_quarantined_and_blocks() {
    if !host_has_sandbox() {
        return;
    }
    let tier = harness_sandbox::detect(harness_sandbox::SandboxSettings::default())
        .map(|sandbox| sandbox.git_protection());
    if !matches!(tier, Some(harness_core::tool::GitProtection::Basic { .. })) {
        eprintln!("skipping: the sandbox here is not in the basic tier");
        return;
    }
    let (env, out) = run_bash("echo 'echo pwned' > .git/hooks/pre-commit", "", |_| {}).await;
    assert_eq!(out.status.code(), Some(3), "{}", tool_output(&out));
    assert!(!env.ws.path().join(".git/hooks/pre-commit").exists());
    let output = tool_output(&out);
    assert!(output.contains("[the sandbox undid changes"), "{output}");
    assert!(output.contains("- .git/hooks/pre-commit: "), "{output}");
    let quarantine = env.home.path().join("data/quarantine");
    let stored: Vec<_> = std::fs::read_dir(&quarantine)
        .unwrap_or_else(|e| panic!("no quarantine at {quarantine:?}: {e}"))
        .map(|entry| entry.unwrap().path().join("dot-git/hooks/pre-commit"))
        .filter(|path| path.exists())
        .collect();
    assert_eq!(stored.len(), 1, "{stored:?}");
    assert_eq!(std::fs::read_to_string(&stored[0]).unwrap(), "echo pwned\n");
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
        tool_output(&out).contains("[the sandbox may have blocked"),
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

/// Macos and the Linux full tier refuse the write; the Linux basic tier moves the hook to
/// quarantine after the command. Either way the command counts as blocked.
#[tokio::test(flavor = "multi_thread")]
async fn planting_a_git_hook_fails_in_the_sandbox() {
    if !host_has_sandbox() {
        return;
    }
    let basic = linux_basic_tier() == Some(true);
    let (env, out) = run_bash("echo 'echo pwned' > .git/hooks/pre-commit", "", |_| {}).await;
    assert!(!env.ws.path().join(".git/hooks/pre-commit").exists());
    assert_eq!(out.status.code(), Some(3), "{}", tool_output(&out));
    let expected = if basic {
        "- .git/hooks/pre-commit: new in a protected directory; moved to "
    } else {
        "[the sandbox may have blocked"
    };
    assert!(
        tool_output(&out).contains(expected),
        "{}",
        tool_output(&out)
    );
    if basic {
        let quarantine = env.home.path().join("data/quarantine");
        // The guard maps `.git` to `dot-git` in the quarantine (Task 4's I4/B2).
        let moved = std::fs::read_dir(&quarantine)
            .unwrap()
            .map(|d| d.unwrap().path().join("dot-git/hooks/pre-commit"))
            .find(|p| p.exists())
            .expect("the hook is in the quarantine");
        assert_eq!(std::fs::read_to_string(moved).unwrap(), "echo pwned\n");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn git_init_of_a_nested_repository_is_undone() {
    if !host_has_sandbox() {
        return;
    }
    let (env, out) = run_bash("git init -q sub", "", |_| {}).await;
    assert!(!env.ws.path().join("sub/.git").exists());
    assert_eq!(out.status.code(), Some(3), "{}", tool_output(&out));
    if cfg!(target_os = "linux") {
        assert!(
            tool_output(&out).contains("- sub/.git: a new repository; moved to "),
            "{}",
            tool_output(&out)
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_linux_basic_tier_warns_at_startup() {
    let Some(basic) = linux_basic_tier() else {
        return;
    };
    let (_env, out) = run_bash("true", "", |_| {}).await;
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        stderr.contains("run `harness sandbox doctor`"),
        basic,
        "{stderr}"
    );
    assert_eq!(out.status.code(), Some(0), "{stderr}");
}

#[tokio::test(flavor = "multi_thread")]
async fn required_git_protection_asks_before_every_command_in_the_basic_tier() {
    let Some(basic) = linux_basic_tier() else {
        return;
    };
    let (env, out) = run_bash(
        "touch made.txt",
        "[sandbox]\nlinux_git_protection = \"required\"\n",
        |_| {},
    )
    .await;
    let stderr = String::from_utf8_lossy(&out.stderr);
    if basic {
        assert_eq!(out.status.code(), Some(3), "{stderr}");
        assert!(
            stderr.contains("every shell command will need approval"),
            "{stderr}"
        );
        assert!(!env.ws.path().join("made.txt").exists());
    } else {
        assert_eq!(out.status.code(), Some(0), "{stderr}");
        assert!(env.ws.path().join("made.txt").exists());
    }
}

#[test]
fn sandbox_doctor_reports_the_mechanism_and_tier() {
    let home = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let out = Command::new(BIN)
        .args(["sandbox", "doctor"])
        .current_dir(ws.path())
        .env("HARNESS_HOME", home.path())
        .env("HARNESS_CREDENTIAL_STORE", "file")
        .env_remove("HARNESS_SANDBOX")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.starts_with("Sandbox: "), "{stdout}");
    match linux_basic_tier() {
        Some(true) => {
            assert!(
                stdout.contains("Git metadata protection: basic tier\n  Why: "),
                "{stdout}"
            );
            assert!(
                stdout.contains("harness does not change any of these settings itself."),
                "{stdout}"
            );
            let apparmor =
                std::fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")
                    .is_ok_and(|v| v.trim() == "1");
            if apparmor {
                assert!(
                    stdout.contains("sudo apparmor_parser -r /etc/apparmor.d/harness"),
                    "{stdout}"
                );
                assert!(
                    stdout
                        .contains("sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0"),
                    "{stdout}"
                );
            }
        }
        Some(false) => assert!(stdout.contains("full tier"), "{stdout}"),
        None if cfg!(target_os = "macos") => {
            assert!(stdout.starts_with("Sandbox: seatbelt\n"), "{stdout}")
        }
        None => {}
    }
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
    // `echo hi` is unlisted: it runs unapproved when this host has a sandbox (exit 0), and needs
    // approval it can't get otherwise (exit 3) — either way the unknown-rule warning and JSON
    // framing must hold.
    let expect_success = host_has_sandbox();
    let server = MockServer::start().await;
    bash_then_done(&server, "echo hi").await;
    let env = Env::new(&server.uri(), "[permissions]\nallow = [\"shell:rm*\"]\n");
    tokio::task::spawn_blocking(move || {
        let out = env.cmd().args(["ask", "--json", "go"]).output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(!stdout.trim().is_empty(), "expected some JSON on stdout");
        for line in stdout.lines() {
            assert!(
                serde_json::from_str::<Value>(line).is_ok(),
                "not JSON: {line}"
            );
        }
        let expected_code = if expect_success { 0 } else { 3 };
        assert_eq!(
            out.status.code(),
            Some(expected_code),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        let warnings = stderr.matches("names an unknown tool").count();
        assert_eq!(warnings, 1, "{stderr}");
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn plan_mode_without_a_sandbox_refuses_commands() {
    let server = MockServer::start().await;
    bash_then_done(&server, "touch made.txt").await;
    let env = Env::new(&server.uri(), "");
    let (env, out) = tokio::task::spawn_blocking(move || {
        let out = env
            .cmd()
            .env("HARNESS_SANDBOX", "none")
            .args(["--mode", "plan", "ask", "--json", "go"])
            .output()
            .unwrap();
        (env, out)
    })
    .await
    .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", tool_output(&out));
    assert!(
        tool_output(&out).contains("shell commands need the OS sandbox in plan and read-only mode"),
        "{}",
        tool_output(&out)
    );
    assert!(!env.ws.path().join("made.txt").exists());
}

/// Runs `command` through `harness ask --json` with `$HOME` set to `home(workspace)`.
async fn run_bash_with_home(
    command: &str,
    mode: &str,
    home: impl FnOnce(&std::path::Path) -> std::path::PathBuf,
) -> (Env, std::process::Output) {
    let server = MockServer::start().await;
    bash_then_done(&server, command).await;
    let env = Env::new(&server.uri(), "");
    let home = home(env.ws.path());
    std::fs::create_dir_all(&home).unwrap();
    let mode = mode.to_string();
    tokio::task::spawn_blocking(move || {
        let out = env
            .cmd()
            .env("HOME", &home)
            .args(["--mode", &mode, "ask", "--json", "go"])
            .output()
            .unwrap();
        (env, out)
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_workspace_at_or_above_home_gets_no_writable_sandbox() {
    // `$HOME` is the workspace itself, then a directory inside it.
    for home_below in [false, true] {
        let (env, out) = run_bash_with_home("echo hi > made.txt", "auto", |ws| {
            if home_below {
                ws.join("me")
            } else {
                ws.to_path_buf()
            }
        })
        .await;
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(3), "{stderr}");
        assert!(!env.ws.path().join("made.txt").exists(), "{stderr}");
        let warnings: Vec<&str> = stderr
            .lines()
            .filter(|l| l.starts_with("warning:"))
            .collect();
        assert!(
            warnings.len() == 1 && warnings[0].contains("home directory or above"),
            "{stderr}"
        );
    }
}

/// Runs the `write` tool through `harness ask --json` with `$HOME` set to `home(workspace)`.
async fn run_write_with_home(
    mode: &str,
    home: impl FnOnce(&std::path::Path) -> std::path::PathBuf,
) -> (Env, std::process::Output) {
    let server = MockServer::start().await;
    let args = json!({"path": "made.txt", "content": "hi\n"}).to_string();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_string_contains("\"role\":\"tool\""))
        .respond_with(stream(&[json!({"choices": [{"index": 0, "delta": {"content": "done"}, "finish_reason": "stop"}]})]))
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(stream(&[json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "id": "c1",
            "type": "function", "function": {"name": "write", "arguments": args}}]}, "finish_reason": "tool_calls"}]})]))
        .with_priority(2)
        .mount(&server)
        .await;
    let env = Env::new(&server.uri(), "");
    let home = home(env.ws.path());
    std::fs::create_dir_all(&home).unwrap();
    let mode = mode.to_string();
    tokio::task::spawn_blocking(move || {
        let out = env
            .cmd()
            .env("HOME", &home)
            .args(["--mode", &mode, "ask", "--json", "go"])
            .output()
            .unwrap();
        (env, out)
    })
    .await
    .unwrap()
}

// Review Focus: a too-broad workspace (`/`, `$HOME`, or an ancestor of it) gets no writable
// sandbox, but before this fix the write tool still wrote unapproved — including dotfiles, since
// `check_write`'s decision never consulted `sandbox_available`. It must now ask, exactly as `ask`
// mode would, even though the actual mode here is `auto`.
#[tokio::test(flavor = "multi_thread")]
async fn write_tool_needs_approval_when_the_workspace_is_too_broad() {
    for home_below in [false, true] {
        let (env, out) = run_write_with_home("auto", |ws| {
            if home_below {
                ws.join("me")
            } else {
                ws.to_path_buf()
            }
        })
        .await;
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(3), "{stderr}");
        assert!(!env.ws.path().join("made.txt").exists(), "{stderr}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn plan_mode_in_a_workspace_at_home_keeps_the_read_only_sandbox() {
    if !host_has_sandbox() {
        return;
    }
    let (env, out) =
        run_bash_with_home("ls && touch made.txt", "plan", |ws| ws.to_path_buf()).await;
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("home directory or above"), "{stderr}");
    assert!(
        tool_output(&out).contains("[the sandbox may have blocked"),
        "{}",
        tool_output(&out)
    );
    assert!(!env.ws.path().join("made.txt").exists());
}
