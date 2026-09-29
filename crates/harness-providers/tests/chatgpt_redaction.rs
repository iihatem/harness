//! Review F M4: the tokens of a signed-in ChatGPT account are secrets from the moment they are
//! loaded, and so are the tokens that replace them, whether this process refreshed them or
//! another harness process did. A mock server plays the authorization server; the tokens live in
//! a file store in a temporary directory.
#![cfg(feature = "chatgpt-login")]

use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use harness_core::redact::{REDACTED, Redactor};
use harness_providers::chatgpt::auth::ChatGptAuth;
use harness_providers::chatgpt::oauth::{OAuth, Tokens};
use harness_providers::credentials::Credentials;
use serde_json::{Value, json};
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// An access token that expires `secs` from now.
fn access_token(label: &str, secs: u64) -> String {
    let part = |value: &Value| URL_SAFE_NO_PAD.encode(serde_json::to_vec(value).unwrap());
    format!(
        "{}.{}.{}",
        part(&json!({"alg": "none"})),
        part(&json!({"exp": harness_core::time::now_unix() + secs, "label": label})),
        URL_SAFE_NO_PAD.encode(b"sig")
    )
}

fn tokens(access: &str, refresh: &str) -> Tokens {
    Tokens {
        access_token: access.to_string(),
        refresh_token: refresh.to_string(),
        account_id: Some("acct-123".into()),
        email: None,
    }
}

/// A store in `dir` signed in with `stored`, and the account loaded from it with a redactor.
fn signed_in(
    dir: &std::path::Path,
    stored: &Tokens,
    server: &MockServer,
) -> (Arc<Credentials>, ChatGptAuth, Arc<Redactor>) {
    let credentials = Arc::new(Credentials::with_keychain(dir, None));
    credentials
        .set("chatgpt", "default", &stored.to_json())
        .unwrap();
    let redactor = Arc::new(Redactor::default());
    let auth = ChatGptAuth::load(credentials.clone(), "default", OAuth::new(&server.uri()))
        .unwrap()
        .expect("signed in")
        .with_redactor(redactor.clone());
    (credentials, auth, redactor)
}

#[tokio::test]
async fn loaded_and_refreshed_tokens_are_secrets() {
    let server = MockServer::start().await;
    let old = access_token("old", 60);
    let new = access_token("new", 7200);
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_string_contains("rt-old-canary-1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"access_token": new, "refresh_token": "rt-new-canary-2"})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_credentials, auth, redactor) =
        signed_in(dir.path(), &tokens(&old, "rt-old-canary-1"), &server);
    for secret in [old.as_str(), "rt-old-canary-1"] {
        assert_eq!(redactor.redact(secret), REDACTED);
    }
    // The access token expires within five minutes: it is refreshed first.
    assert_eq!(auth.current().await.unwrap().access_token, new);
    for secret in [new.as_str(), "rt-new-canary-2"] {
        assert_eq!(redactor.redact(secret), REDACTED);
    }
}

#[tokio::test]
async fn tokens_another_process_stored_are_secrets() {
    // Nothing is mounted: a refresh would fail.
    let server = MockServer::start().await;
    let ours = access_token("ours", 3600);
    let theirs = access_token("theirs", 7200);
    let dir = tempfile::tempdir().unwrap();
    let (credentials, auth, redactor) =
        signed_in(dir.path(), &tokens(&ours, "rt-ours-canary-1"), &server);
    credentials
        .set(
            "chatgpt",
            "default",
            &tokens(&theirs, "rt-theirs-canary-2").to_json(),
        )
        .unwrap();
    // The server refused ours; another process has renewed them already.
    assert_eq!(
        auth.after_unauthorized(&ours).await.unwrap().access_token,
        theirs
    );
    for secret in [theirs.as_str(), "rt-theirs-canary-2"] {
        assert_eq!(redactor.redact(secret), REDACTED);
    }
}
