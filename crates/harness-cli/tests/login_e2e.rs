//! `harness login` against a mock OAuth server that also plays ChatGPT's backend. The tests use
//! the device flow, which needs no browser; tokens go to the file store.
#![cfg(feature = "chatgpt-login")]

use std::os::unix::fs::PermissionsExt;

use assert_cmd::Command;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use predicates::prelude::*;
use predicates::str::contains;
use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const BIN: &str = env!("CARGO_BIN_EXE_harness");

fn jwt(claims: Value) -> String {
    let part = |value: &Value| URL_SAFE_NO_PAD.encode(serde_json::to_vec(value).unwrap());
    format!(
        "{}.{}.{}",
        part(&json!({"alg": "none"})),
        part(&claims),
        URL_SAFE_NO_PAD.encode(b"sig")
    )
}

/// The access token the mock server issues, valid until 2100.
fn access_token() -> String {
    jwt(json!({"exp": 4_102_444_800u64}))
}

/// A device-code sign-in that the user approves at once.
async fn mock_sign_in(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/api/accounts/deviceauth/usercode"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_auth_id": "device-auth-1",
            "user_code": "ABCD-1234",
            "interval": "0"
        })))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/accounts/deviceauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "authorization_code": "code-1",
            "code_challenge": "challenge",
            "code_verifier": "verifier"
        })))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id_token": jwt(json!({
                "email": "dev@example.com",
                "https://api.openai.com/auth": {"chatgpt_account_id": "acct-123"}
            })),
            "access_token": access_token(),
            "refresh_token": "rt-1"
        })))
        .mount(server)
        .await;
}

/// ChatGPT's backend: answers requests that carry the signed-in account.
async fn mock_backend(server: &MockServer) {
    let body = [
        json!({"type": "response.output_text.delta", "output_index": 0, "delta": "hi from chatgpt"}),
        json!({"type": "response.completed", "response": {"status": "completed"}}),
    ]
    .iter()
    .map(|e| format!("data: {e}\n\n"))
    .collect::<String>();
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/responses"))
        .and(header(
            "authorization",
            format!("Bearer {}", access_token()).as_str(),
        ))
        .and(header("chatgpt-account-id", "acct-123"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .mount(server)
        .await;
}

struct Env {
    home: TempDir,
    ws: TempDir,
    server_uri: String,
}

impl Env {
    fn new(server_uri: &str) -> Env {
        let home = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        std::fs::create_dir(ws.path().join(".git")).unwrap();
        Env {
            home,
            ws,
            server_uri: server_uri.to_string(),
        }
    }

    fn cmd(&self) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.current_dir(self.ws.path())
            .env("HARNESS_HOME", self.home.path())
            .env("HARNESS_CREDENTIAL_STORE", "file")
            .env("HARNESS_CHATGPT_ISSUER", &self.server_uri)
            .env(
                "HARNESS_CHATGPT_BASE_URL",
                format!("{}/backend-api/codex", self.server_uri),
            )
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME")
            .env_remove("XDG_STATE_HOME")
            .env_remove("SSH_CONNECTION")
            .env_remove("SSH_TTY");
        cmd
    }

    fn credentials(&self) -> std::path::PathBuf {
        self.home.path().join("data/credentials.json")
    }
}

// Spec: "Sign-in over SSH" (with --device), then a turn on the account, and "Logout".
#[tokio::test(flavor = "multi_thread")]
async fn a_device_sign_in_lets_chatgpt_models_answer_until_logout() {
    let server = MockServer::start().await;
    mock_sign_in(&server).await;
    mock_backend(&server).await;
    let env = Env::new(&server.uri());
    tokio::task::spawn_blocking(move || {
        env.cmd()
            .args(["login", "chatgpt", "--device"])
            .assert()
            .success()
            .stderr(contains("not a contractual guarantee"))
            .stderr(contains("ABCD-1234"))
            .stderr(contains("/codex/device"))
            .stdout(contains(
                "Signed in to ChatGPT as dev@example.com (profile default)",
            ));
        let stored = std::fs::read_to_string(env.credentials()).unwrap();
        assert!(stored.contains("acct-123"), "{stored}");
        let mode = std::fs::metadata(env.credentials())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        env.cmd()
            .args(["--model", "chatgpt/gpt-5.5", "ask", "hi"])
            .assert()
            .success()
            .stdout(contains("hi from chatgpt"));
        env.cmd()
            .args(["logout", "chatgpt"])
            .assert()
            .success()
            .stdout(contains("Removed the stored credentials for chatgpt"));
        env.cmd()
            .args(["--model", "chatgpt/gpt-5.5", "ask", "hi"])
            .assert()
            .code(2)
            .stderr(contains("not signed in to chatgpt"))
            .stderr(contains("harness login chatgpt"));
    })
    .await
    .unwrap();
}

// Over SSH no browser can be opened here, so the device flow is used without --device.
#[tokio::test(flavor = "multi_thread")]
async fn over_ssh_the_device_flow_is_used() {
    let server = MockServer::start().await;
    mock_sign_in(&server).await;
    let env = Env::new(&server.uri());
    tokio::task::spawn_blocking(move || {
        env.cmd()
            .env("SSH_CONNECTION", "10.0.0.2 50000 10.0.0.1 22")
            .args(["login", "chatgpt"])
            .assert()
            .success()
            .stderr(contains("ABCD-1234"));
    })
    .await
    .unwrap();
}

// Spec: "Switching ChatGPT accounts".
#[tokio::test(flavor = "multi_thread")]
async fn each_profile_signs_in_on_its_own() {
    let server = MockServer::start().await;
    mock_sign_in(&server).await;
    mock_backend(&server).await;
    let env = Env::new(&server.uri());
    tokio::task::spawn_blocking(move || {
        env.cmd()
            .args(["login", "chatgpt", "--device", "--profile", "work"])
            .assert()
            .success()
            .stdout(contains("(profile work)"));
        env.cmd()
            .args(["--model", "chatgpt/gpt-5.5", "ask", "hi"])
            .assert()
            .code(2)
            .stderr(contains("not signed in to chatgpt (profile default)"));
        env.cmd()
            .args(["auth", "use", "chatgpt", "work"])
            .assert()
            .success();
        env.cmd()
            .args(["--model", "chatgpt/gpt-5.5", "ask", "hi"])
            .assert()
            .success()
            .stdout(contains("hi from chatgpt"));
    })
    .await
    .unwrap();
}

// Spec: "Claude subscription credentials are never used": there is no Claude sign-in.
#[test]
fn only_chatgpt_can_be_signed_in_to() {
    let env = Env::new("http://127.0.0.1:9");
    for provider in ["anthropic", "claude"] {
        env.cmd()
            .args(["login", provider])
            .assert()
            .code(2)
            .stderr(contains("Claude Code"))
            .stderr(contains("harness auth add anthropic"));
    }
    env.cmd()
        .args(["login", "openai"])
        .assert()
        .code(2)
        .stderr(contains("harness auth add openai"));
    env.cmd()
        .args(["login", "nope"])
        .assert()
        .code(2)
        .stderr(contains("unknown provider `nope`"));
    assert!(!env.credentials().exists());
}

#[test]
fn help_lists_login_with_its_flags() {
    Command::new(BIN)
        .arg("--help")
        .assert()
        .success()
        .stdout(contains("login"));
    Command::new(BIN)
        .args(["login", "--help"])
        .assert()
        .success()
        .stdout(contains("--device").and(contains("--profile")));
}
