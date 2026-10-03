//! 1.7: polling `GET /wham/usage` with the signed-in account.
#![cfg(feature = "chatgpt-login")]

use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use harness_core::meter::WindowSource;
use harness_core::provider::Provider;
use harness_providers::chatgpt::auth::ChatGptAuth;
use harness_providers::chatgpt::oauth::{OAuth, Tokens};
use harness_providers::credentials::Credentials;
use harness_providers::openai_responses::OpenAiResponses;
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn jwt(claims: Value) -> String {
    let part = |value: &Value| URL_SAFE_NO_PAD.encode(serde_json::to_vec(value).unwrap());
    format!(
        "{}.{}.{}",
        part(&json!({"alg": "none"})),
        part(&claims),
        URL_SAFE_NO_PAD.encode(b"sig")
    )
}

fn provider(server: &MockServer) -> (tempfile::TempDir, OpenAiResponses, String) {
    let access = jwt(json!({"exp": harness_core::time::now_unix() + 3600}));
    let tokens = Tokens {
        access_token: access.clone(),
        refresh_token: "refresh".into(),
        account_id: Some("acct-123".into()),
        email: None,
    };
    let dir = tempfile::tempdir().unwrap();
    let credentials = Arc::new(Credentials::with_keychain(dir.path(), None));
    credentials
        .set("chatgpt", "default", &tokens.to_json())
        .unwrap();
    credentials.use_profile("chatgpt", "default").unwrap();
    let auth = ChatGptAuth::load(credentials, "default", OAuth::new(&server.uri()).unwrap())
        .unwrap()
        .expect("signed in");
    let provider = OpenAiResponses::chatgpt(
        format!("{}/backend-api/codex", server.uri()),
        Arc::new(auth),
    );
    (dir, provider, access)
}

#[tokio::test]
async fn the_usage_endpoint_is_asked_with_the_account_and_gives_windows() {
    let server = MockServer::start().await;
    let (_dir, provider, access) = provider(&server);
    Mock::given(method("GET"))
        .and(path("/backend-api/wham/usage"))
        .and(header("authorization", format!("Bearer {access}").as_str()))
        .and(header("chatgpt-account-id", "acct-123"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "rate_limit": {"primary_window": {"used_percent": 62, "limit_window_seconds": 18000, "reset_at": 1790946000},
                           "secondary_window": {"used_percent": 20, "limit_window_seconds": 604800, "reset_at": 1791142400}}
        })))
        .expect(1)
        .mount(&server)
        .await;
    let snapshot = provider
        .windows()
        .expect("ChatGPT can be polled")
        .await
        .unwrap();
    assert_eq!(snapshot.windows.len(), 2);
    assert_eq!(snapshot.windows[0].label(), "5h");
    assert_eq!(snapshot.windows[0].used_percent, Some(62.0));
    assert!(
        snapshot
            .windows
            .iter()
            .all(|w| w.source == WindowSource::Poll)
    );
}

#[tokio::test]
async fn a_poll_that_fails_says_so_without_quoting_the_body() {
    let server = MockServer::start().await;
    let (_dir, provider, _) = provider(&server);
    Mock::given(method("GET"))
        .and(path("/backend-api/wham/usage"))
        .respond_with(ResponseTemplate::new(500).set_body_string("secret-looking body"))
        .mount(&server)
        .await;
    let err = provider.windows().unwrap().await.unwrap_err();
    assert!(err.contains("500"), "{err}");
    assert!(!err.contains("secret-looking"), "{err}");
}
