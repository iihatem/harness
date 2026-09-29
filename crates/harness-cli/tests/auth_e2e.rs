//! `harness auth add`, `auth use` and `logout`, and which key requests carry. Credentials go to
//! the file store (`HARNESS_CREDENTIAL_STORE=file`), so the tests never touch a real keychain.

use std::os::unix::fs::PermissionsExt;

use assert_cmd::Command;
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
            .env("HARNESS_CREDENTIAL_STORE", "file")
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME")
            .env_remove("XDG_STATE_HOME")
            .env_remove("MOCK_API_KEY")
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY");
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
        env.cmd()
            .args(["--model", "mock/m", "ask", "hi"])
            .assert()
            .code(2)
            .stderr(contains("harness auth add mock"));
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
        env.add("chatgpt", None, "k")
            .code(2)
            .stderr(contains("harness login chatgpt"));
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
