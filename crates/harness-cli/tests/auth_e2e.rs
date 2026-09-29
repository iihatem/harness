//! `harness auth add`, `auth use` and `logout`, and which key requests carry. Credentials go to
//! the file store (`HARNESS_CREDENTIAL_STORE=file`), so the tests never touch a real keychain.

mod common;
use common::Isolate;

use std::os::unix::fs::PermissionsExt;

use assert_cmd::Command;
use predicates::prelude::*;
use predicates::str::contains;
use serde_json::json;
use tempfile::TempDir;
use wiremock::matchers::{header, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

const BIN: &str = env!("CARGO_BIN_EXE_harness");

fn answer(text: &str) -> ResponseTemplate {
    let chunk =
        json!({"choices": [{"index": 0, "delta": {"content": text}, "finish_reason": "stop"}]});
    ResponseTemplate::new(200).set_body_raw(
        format!("data: {chunk}\n\ndata: [DONE]\n\n"),
        "text/event-stream",
    )
}

/// Answers with which key a request carried, by name: an answer that repeated the key would be
/// redacted.
async fn echo_keys(server: &MockServer) {
    for (key, name) in [
        ("sk-stored", "the stored key"),
        ("sk-env", "the environment's key"),
        ("sk-work", "the work key"),
    ] {
        Mock::given(method("POST"))
            .and(header("authorization", format!("Bearer {key}").as_str()))
            .respond_with(answer(&format!("used {name}")))
            .mount(server)
            .await;
    }
}

struct Env {
    home: TempDir,
    ws: TempDir,
    user_home: TempDir,
}

impl Env {
    /// Provider `mock` at `server_uri` takes its key from `MOCK_API_KEY`; `extra` is more config.
    fn new(server_uri: &str, extra: &str) -> Env {
        let home = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let user_home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("config")).unwrap();
        std::fs::write(
            home.path().join("config/config.toml"),
            format!(
                "[providers.mock]\nprotocol = \"openai-chat\"\nbase_url = \"{server_uri}/v1\"\napi_key_env = \"MOCK_API_KEY\"\n{extra}"
            ),
        )
        .unwrap();
        std::fs::create_dir(ws.path().join(".git")).unwrap();
        Env {
            home,
            ws,
            user_home,
        }
    }

    fn cmd(&self) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.current_dir(self.ws.path())
            .env("HARNESS_HOME", self.home.path())
            .env("HOME", self.user_home.path())
            .isolate()
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME")
            .env_remove("XDG_STATE_HOME")
            .env_remove("MOCK_API_KEY");
        cmd
    }

    fn add(&self, provider: &str, profile: Option<&str>, key: &str) -> assert_cmd::assert::Assert {
        let mut cmd = self.cmd();
        cmd.args(["auth", "add", provider]);
        if let Some(profile) = profile {
            cmd.args(["--profile", profile]);
        }
        cmd.write_stdin(key).assert()
    }

    fn credentials(&self) -> std::path::PathBuf {
        self.home.path().join("data/credentials.json")
    }
}

// Spec: "Environment variable wins", and a piped key keeps no trailing newline.
#[tokio::test(flavor = "multi_thread")]
async fn a_stored_key_is_used_unless_the_environment_has_one() {
    let server = MockServer::start().await;
    echo_keys(&server).await;
    let env = Env::new(&server.uri(), "");
    tokio::task::spawn_blocking(move || {
        env.add("mock", None, "sk-stored\n")
            .success()
            .stdout(contains("Stored the API key for mock (profile default)"));
        let file = env.credentials();
        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        // Never in the configuration directory, which people sync to dotfile repositories.
        let config = std::fs::read_to_string(env.home.path().join("config/config.toml")).unwrap();
        assert!(!config.contains("sk-stored"));
        env.cmd()
            .args(["--model", "mock/m", "ask", "hi"])
            .assert()
            .success()
            .stdout(contains("used the stored key"));
        env.cmd()
            .env("MOCK_API_KEY", "sk-env")
            .args(["--model", "mock/m", "ask", "hi"])
            .assert()
            .success()
            .stdout(contains("used the environment's key"));
    })
    .await
    .unwrap();
}

// Spec: "Switching ChatGPT accounts", with an API-key provider; and "Logout".
#[tokio::test(flavor = "multi_thread")]
async fn profiles_are_chosen_with_auth_use_and_removed_with_logout() {
    let server = MockServer::start().await;
    echo_keys(&server).await;
    let env = Env::new(&server.uri(), "");
    tokio::task::spawn_blocking(move || {
        env.add("mock", None, "sk-stored").success();
        env.add("mock", Some("work"), "sk-work").success();
        env.cmd()
            .args(["auth", "use", "mock", "work"])
            .assert()
            .success()
            .stdout(contains("mock now uses profile work"));
        env.cmd()
            .args(["--model", "mock/m", "ask", "hi"])
            .assert()
            .success()
            .stdout(contains("used the work key"));
        env.cmd()
            .args(["logout", "mock"])
            .assert()
            .success()
            .stdout(contains(
                "Removed the stored credentials for mock (profile work)",
            ));
        // Review B, M4: the hint stores under the profile in use, not under `default`.
        env.cmd()
            .args(["--model", "mock/m", "ask", "hi"])
            .assert()
            .code(2)
            .stderr(contains("`harness auth add mock --profile work`"));
        env.cmd()
            .args(["logout", "mock", "--profile", "work"])
            .assert()
            .success()
            .stdout(contains(
                "No credentials are stored for mock (profile work)",
            ));
        env.cmd()
            .args(["auth", "use", "mock", "default"])
            .assert()
            .success();
        env.cmd()
            .args(["--model", "mock/m", "ask", "hi"])
            .assert()
            .success()
            .stdout(contains("used the stored key"));
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn auth_add_refuses_what_it_cannot_store() {
    let server = MockServer::start().await;
    let env = Env::new(&server.uri(), "");
    tokio::task::spawn_blocking(move || {
        env.add("mock", None, "\n")
            .code(2)
            .stderr(contains("no API key"));
        env.add("nope", None, "k")
            .code(2)
            .stderr(contains("unknown provider `nope`"));
        env.add("ollama", None, "k")
            .code(2)
            .stderr(contains("needs no API key"));
        // A build without sign-in says it has none (no_sign_in_e2e.rs).
        let sign_in = if cfg!(feature = "chatgpt-login") {
            "harness login chatgpt"
        } else {
            "made without ChatGPT sign-in"
        };
        env.add("chatgpt", None, "k")
            .code(2)
            .stderr(contains(sign_in));
        env.add("mock", Some("../x"), "k").code(2);
        assert!(!env.credentials().exists());
    })
    .await
    .unwrap();
}

/// A signed-in Claude Code in the fake home directory, with a canary for a token.
fn sign_in_claude_code(env: &Env) {
    let claude = env.user_home.path().join(".claude");
    std::fs::create_dir_all(&claude).unwrap();
    std::fs::write(
        claude.join(".credentials.json"),
        r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-CANARY","refreshToken":"sk-ant-ort01-CANARY"}}"#,
    )
    .unwrap();
}

// Spec: "Claude Code signed in, no API key" and "A subscription token in the environment".
#[tokio::test(flavor = "multi_thread")]
async fn claude_subscription_credentials_are_never_used() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    // The built-in provider, pointed at the mock server so that nothing leaves the machine.
    let anthropic = format!(
        "[providers.anthropic]\nprotocol = \"anthropic-messages\"\nbase_url = \"{}/v1\"\napi_key_env = \"ANTHROPIC_API_KEY\"\n",
        server.uri()
    );
    let env = Env::new(&server.uri(), &anthropic);
    sign_in_claude_code(&env);
    let env = tokio::task::spawn_blocking(move || {
        env.cmd()
            .args(["--model", "anthropic/claude-sonnet-4-5", "ask", "hi"])
            .assert()
            .code(2)
            .stderr(contains("needs an API key"));
        env.cmd()
            .env("ANTHROPIC_API_KEY", "sk-ant-oat01-CANARY")
            .args(["--model", "anthropic/claude-sonnet-4-5", "ask", "hi"])
            .assert()
            .code(2)
            .stderr(contains("Claude subscription token"));
        env.add("anthropic", None, "sk-ant-oat01-CANARY")
            .code(2)
            .stderr(contains("Claude subscription token"));
        assert!(!env.credentials().exists());
        env
    })
    .await
    .unwrap();
    assert!(server.received_requests().await.unwrap().is_empty());
    drop(env);
}

// Review C, I1: a `[providers.chatgpt]` definition is refused, so no command can hand the stored
// ChatGPT sign-in to it as a key, and `auth add chatgpt` cannot overwrite that sign-in.
#[test]
fn a_config_cannot_define_the_chatgpt_provider() {
    let chatgpt = "[providers.chatgpt]\nprotocol = \"openai-responses\"\nbase_url = \"http://127.0.0.1:9/v1\"\napi_key_env = \"PROXY_KEY\"\n";
    let env = Env::new("http://127.0.0.1:9", chatgpt);
    for args in [
        &["auth", "add", "chatgpt"][..],
        &["--model", "chatgpt/gpt-5.5", "ask", "hi"],
        &["models"],
    ] {
        env.cmd()
            .args(args)
            .write_stdin("sk-proxy\n")
            .assert()
            .code(2)
            .stderr(contains("config.toml"))
            .stderr(contains("reserved for ChatGPT sign-in"));
    }
    assert!(!env.credentials().exists());
}

// Review B, M5: a Claude subscription token is refused for every provider, whatever its
// protocol, even behind a byte-order mark.
#[test]
fn auth_add_refuses_claude_subscription_tokens_for_every_provider() {
    let env = Env::new("http://127.0.0.1:9", "");
    for token in ["sk-ant-oat01-CANARY\n", "\u{feff}sk-ant-oat01-CANARY\n"] {
        for provider in ["openrouter", "mock"] {
            env.add(provider, None, token)
                .code(2)
                .stderr(contains("Claude subscription token"));
        }
    }
    assert!(!env.credentials().exists());
    // A key is stored without the mark.
    env.add("mock", None, "\u{feff}sk-marked\n").success();
    let stored = std::fs::read_to_string(env.credentials()).unwrap();
    assert!(stored.contains("\"sk-marked\""), "{stored:?}");
}

// Review B, M4: every hint leads somewhere: `auth use` refuses a provider there is nothing to
// store for, rather than suggesting an `auth add` that is refused too; and `auth add` says when
// the environment's key wins over the one just stored.
#[test]
fn hints_lead_somewhere() {
    let env = Env::new("http://127.0.0.1:9", "");
    env.cmd()
        .args(["auth", "use", "ollama", "work"])
        .assert()
        .code(2)
        .stderr(contains("needs no credentials"))
        .stdout(contains("auth add").not());
    env.cmd()
        .env("MOCK_API_KEY", "sk-from-env")
        .args(["auth", "add", "mock"])
        .write_stdin("sk-stored\n")
        .assert()
        .success()
        .stderr(contains("$MOCK_API_KEY is set"));
    env.cmd()
        .args(["auth", "use", "mock", "work"])
        .assert()
        .success()
        .stdout(contains("`harness auth add mock --profile work`"));
}

/// Runs `auth add mock` with `stdin` written to it and left open, and returns how it ended, or
/// `None` when it was still waiting after ten seconds (it is then killed).
fn add_with_open_pipe(env: &Env, stdin: &[u8]) -> Option<std::process::Output> {
    use std::io::Write;
    let mut child = std::process::Command::new(BIN)
        .current_dir(env.ws.path())
        .env("HARNESS_HOME", env.home.path())
        .env("HOME", env.user_home.path())
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("XDG_STATE_HOME")
        .env_remove("MOCK_API_KEY")
        .isolate()
        .args(["auth", "add", "mock"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut pipe = child.stdin.take().unwrap();
    pipe.write_all(stdin).unwrap();
    let started = std::time::Instant::now();
    while started.elapsed() < std::time::Duration::from_secs(10) {
        if child.try_wait().unwrap().is_some() {
            drop(pipe);
            return Some(child.wait_with_output().unwrap());
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    child.kill().unwrap();
    child.wait().unwrap();
    None
}

// Review B, M6: a password manager that keeps the pipe open does not hold `auth add` up: the key
// is the first line with something on it.
#[test]
fn a_piped_key_is_read_up_to_its_line_only() {
    let env = Env::new("http://127.0.0.1:9", "");
    let output = add_with_open_pipe(&env, b"\n  sk-first \nsk-second\n").expect("it returned");
    assert!(output.status.success(), "{output:?}");
    let stored = std::fs::read_to_string(env.credentials()).unwrap();
    assert!(stored.contains("\"sk-first\""), "{stored}");
    assert!(!stored.contains("sk-second"), "{stored}");
}

// Review B, M6: there is a limit to what is read, and what is not text is an input error.
#[test]
fn a_key_too_long_or_not_text_is_refused() {
    let env = Env::new("http://127.0.0.1:9", "");
    let long = vec![b'k'; 100 * 1024];
    let output = add_with_open_pipe(&env, &long).expect("it returned");
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("too long"),
        "{output:?}"
    );
    let mut cmd = env.cmd();
    cmd.args(["auth", "add", "mock"])
        .write_stdin(vec![0xff, 0xfe, b'\n'])
        .assert()
        .code(2)
        .stderr(contains("UTF-8"));
    assert!(!env.credentials().exists());
}

// Review B, M6: Ctrl+C at the hidden prompt leaves the terminal as it found it, echo included.
#[test]
fn ctrl_c_at_the_hidden_prompt_restores_the_terminal() {
    use nix::sys::signal::{Signal, kill};
    use nix::sys::termios::{LocalFlags, tcgetattr};
    use nix::unistd::Pid;
    use std::os::unix::process::ExitStatusExt;
    let env = Env::new("http://127.0.0.1:9", "");
    let pty = nix::pty::openpty(None, None).unwrap();
    let tty = || std::process::Stdio::from(pty.slave.try_clone().unwrap());
    assert!(
        tcgetattr(&pty.slave)
            .unwrap()
            .local_flags
            .contains(LocalFlags::ECHO)
    );
    let mut child = std::process::Command::new(BIN)
        .current_dir(env.ws.path())
        .env("HARNESS_HOME", env.home.path())
        .env("HOME", env.user_home.path())
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("XDG_STATE_HOME")
        .isolate()
        .args(["auth", "add", "mock"])
        .stdin(tty())
        .stdout(tty())
        .stderr(tty())
        .spawn()
        .unwrap();
    // Wait for the prompt to turn echo off.
    let started = std::time::Instant::now();
    while tcgetattr(&pty.slave)
        .unwrap()
        .local_flags
        .contains(LocalFlags::ECHO)
    {
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "echo was never turned off"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    kill(Pid::from_raw(child.id() as i32), Signal::SIGINT).unwrap();
    let status = child.wait().unwrap();
    assert!(
        status.signal() == Some(libc_sigint()) || status.code() == Some(130),
        "{status:?}"
    );
    assert!(
        tcgetattr(&pty.slave)
            .unwrap()
            .local_flags
            .contains(LocalFlags::ECHO)
    );
    assert!(!env.credentials().exists());
}

fn libc_sigint() -> i32 {
    nix::sys::signal::Signal::SIGINT as i32
}
