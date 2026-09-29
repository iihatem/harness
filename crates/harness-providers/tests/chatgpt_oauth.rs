//! ChatGPT sign-in against a mock OAuth server: PKCE, the browser flow's localhost callback, the
//! device-code flow, and token refresh. Nothing here talks to OpenAI.
#![cfg(feature = "chatgpt-login")]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use harness_providers::chatgpt::oauth::{
    CLIENT_ID, CallbackServer, OAuth, OAuthError, Pkce, Tokens,
};
use serde_json::{Value, json};
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

fn jwt(claims: Value) -> String {
    let part = |value: &Value| URL_SAFE_NO_PAD.encode(serde_json::to_vec(value).unwrap());
    format!(
        "{}.{}.{}",
        part(&json!({"alg": "none"})),
        part(&claims),
        URL_SAFE_NO_PAD.encode(b"sig")
    )
}

/// What the auth server answers a code exchange with.
fn token_response() -> Value {
    json!({
        "id_token": jwt(json!({
            "email": "dev@example.com",
            "https://api.openai.com/auth": {"chatgpt_account_id": "acct-123", "chatgpt_plan_type": "plus"}
        })),
        "access_token": jwt(json!({"exp": 4_102_444_800u64})),
        "refresh_token": "rt-1",
    })
}

// RFC 7636, appendix B.
#[test]
fn the_pkce_challenge_is_the_verifiers_sha256_in_base64url() {
    let pkce = Pkce::from_verifier("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk");
    assert_eq!(
        pkce.challenge,
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
    );
    let fresh = Pkce::generate().unwrap();
    assert_eq!(fresh.verifier.len(), 86);
    assert!(
        fresh
            .verifier
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    );
    assert_ne!(fresh.verifier, Pkce::generate().unwrap().verifier);
}

#[test]
fn the_authorize_url_asks_for_a_code_with_pkce() {
    let oauth = OAuth::new("https://auth.example");
    let pkce = Pkce::from_verifier("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk");
    let url = reqwest::Url::parse(&oauth.authorize_url(
        "http://127.0.0.1:1455/auth/callback",
        &pkce,
        "st",
    ))
    .unwrap();
    assert_eq!(
        url.as_str().split('?').next(),
        Some("https://auth.example/oauth/authorize")
    );
    let query: std::collections::HashMap<String, String> = url.query_pairs().into_owned().collect();
    for (key, value) in [
        ("response_type", "code"),
        ("client_id", CLIENT_ID),
        ("redirect_uri", "http://127.0.0.1:1455/auth/callback"),
        (
            "scope",
            "openid profile email offline_access api.connectors.read api.connectors.invoke",
        ),
        (
            "code_challenge",
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM",
        ),
        ("code_challenge_method", "S256"),
        ("state", "st"),
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
        ("originator", "codex_cli_rs"),
    ] {
        assert_eq!(query.get(key).map(String::as_str), Some(value), "{key}");
    }
}

/// Plays the browser: follows the redirect back to the callback server.
async fn browser_returns(redirect_uri: &str, query: &str) -> (u16, String) {
    let response = reqwest::get(format!("{redirect_uri}?{query}"))
        .await
        .unwrap();
    (response.status().as_u16(), response.text().await.unwrap())
}

#[tokio::test]
async fn the_browser_flow_exchanges_the_code_from_the_callback() {
    let server = MockServer::start().await;
    let callback = CallbackServer::bind(&[0]).await.unwrap();
    let redirect_uri = callback.redirect_uri();
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_string_contains("grant_type=authorization_code"))
        .and(body_string_contains("code=the-code"))
        .and(body_string_contains(
            "code_verifier=dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk",
        ))
        .and(body_string_contains(
            format!("client_id={CLIENT_ID}").as_str(),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(token_response()))
        .expect(1)
        .mount(&server)
        .await;
    let browser = tokio::spawn({
        let redirect_uri = redirect_uri.clone();
        async move { browser_returns(&redirect_uri, "code=the-code&state=st-1").await }
    });
    let code = callback.wait_for_code("st-1").await.unwrap();
    assert_eq!(code, "the-code");
    let (status, page) = browser.await.unwrap();
    assert_eq!(status, 200);
    assert!(page.contains("signed in"), "{page}");
    let oauth = OAuth::new(&server.uri());
    let pkce = Pkce::from_verifier("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk");
    let tokens = oauth
        .exchange_code(&code, &redirect_uri, &pkce.verifier)
        .await
        .unwrap();
    assert_eq!(tokens.account_id.as_deref(), Some("acct-123"));
    assert_eq!(tokens.email.as_deref(), Some("dev@example.com"));
    assert_eq!(tokens.refresh_token, "rt-1");
    assert_eq!(tokens.expires_at(), Some(4_102_444_800));
}

// Review Focus: a callback with another state (another tab, or a forged link) is refused, and the
// server keeps waiting for the real one.
#[tokio::test]
async fn a_callback_with_the_wrong_state_is_refused() {
    let callback = CallbackServer::bind(&[0]).await.unwrap();
    let redirect_uri = callback.redirect_uri();
    let browser = tokio::spawn(async move {
        let wrong = browser_returns(&redirect_uri, "code=forged&state=other").await;
        let lost = reqwest::get(redirect_uri.replace("/auth/callback", "/favicon.ico"))
            .await
            .unwrap()
            .status()
            .as_u16();
        let right = browser_returns(&redirect_uri, "code=real&state=st-2").await;
        (wrong, lost, right)
    });
    assert_eq!(callback.wait_for_code("st-2").await.unwrap(), "real");
    let ((status, _), lost, (right, _)) = browser.await.unwrap();
    assert_eq!(status, 400);
    assert_eq!(lost, 404);
    assert_eq!(right, 200);
}

#[tokio::test]
async fn a_refused_sign_in_ends_the_wait_with_the_reason() {
    let callback = CallbackServer::bind(&[0]).await.unwrap();
    let redirect_uri = callback.redirect_uri();
    let browser = tokio::spawn(async move {
        browser_returns(
            &redirect_uri,
            "error=access_denied&error_description=The+user+declined&state=st-3",
        )
        .await
    });
    let error = callback.wait_for_code("st-3").await.unwrap_err();
    assert!(
        matches!(&error, OAuthError::Denied(reason) if reason.contains("The user declined")),
        "{error:?}"
    );
    let (_, page) = browser.await.unwrap();
    // A plain-text page: nothing from the query is rendered as HTML.
    assert!(page.contains("The user declined"), "{page}");
}

#[tokio::test]
async fn the_callback_moves_to_the_next_port_when_one_is_taken() {
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = taken.local_addr().unwrap().port();
    let callback = CallbackServer::bind(&[port, 0]).await.unwrap();
    assert_ne!(callback.port(), port);
    assert!(callback.redirect_uri().starts_with("http://127.0.0.1:"));
    assert!(callback.redirect_uri().ends_with("/auth/callback"));
}

async fn mock_usercode(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/api/accounts/deviceauth/usercode"))
        .and(body_string_contains(CLIENT_ID))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_auth_id": "device-auth-123",
            "user_code": "CODE-12345",
            "interval": "0"
        })))
        .mount(server)
        .await;
}

// Spec: "Sign-in over SSH".
#[tokio::test]
async fn the_device_flow_polls_until_the_user_approves() {
    let server = MockServer::start().await;
    mock_usercode(&server).await;
    let polls = Arc::new(AtomicUsize::new(0));
    let counter = polls.clone();
    Mock::given(method("POST"))
        .and(path("/api/accounts/deviceauth/token"))
        .and(body_string_contains("device-auth-123"))
        .respond_with(move |_: &Request| {
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                ResponseTemplate::new(403)
            } else {
                ResponseTemplate::new(200).set_body_json(json!({
                    "authorization_code": "poll-code",
                    "code_challenge": "challenge",
                    "code_verifier": "poll-verifier"
                }))
            }
        })
        .mount(&server)
        .await;
    let callback = format!("{}/deviceauth/callback", server.uri());
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_string_contains("code=poll-code"))
        .and(body_string_contains("code_verifier=poll-verifier"))
        .and(body_string_contains(
            format!("redirect_uri={}", urlencode(&callback)).as_str(),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(token_response()))
        .mount(&server)
        .await;
    let oauth = OAuth::new(&server.uri());
    let device = oauth.request_device_code().await.unwrap();
    assert_eq!(device.user_code, "CODE-12345");
    assert_eq!(
        device.verification_url,
        format!("{}/codex/device", server.uri())
    );
    // Review B, M11: with the user code, the device auth id fetches the tokens.
    let shown = format!("{device:?}");
    assert!(!shown.contains("device-auth-123"), "{shown}");
    let tokens = oauth
        .poll_device_code(&device, Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(tokens.account_id.as_deref(), Some("acct-123"));
    assert_eq!(polls.load(Ordering::SeqCst), 2);
}

fn urlencode(text: &str) -> String {
    text.replace(':', "%3A").replace('/', "%2F")
}

#[tokio::test]
async fn the_device_flow_gives_up_after_its_time_limit() {
    let server = MockServer::start().await;
    mock_usercode(&server).await;
    Mock::given(method("POST"))
        .and(path("/api/accounts/deviceauth/token"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let oauth = OAuth::new(&server.uri());
    let device = oauth.request_device_code().await.unwrap();
    let error = oauth
        .poll_device_code(&device, Duration::from_millis(200))
        .await
        .unwrap_err();
    assert!(matches!(error, OAuthError::TimedOut), "{error:?}");
}

#[tokio::test]
async fn a_refresh_keeps_what_the_server_did_not_replace() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_string_contains(r#""grant_type":"refresh_token""#))
        .and(body_string_contains(r#""refresh_token":"rt-1""#))
        .and(body_string_contains(
            format!(r#""client_id":"{CLIENT_ID}""#).as_str(),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": jwt(json!({"exp": 4_102_444_900u64})),
        })))
        .mount(&server)
        .await;
    let oauth = OAuth::new(&server.uri());
    let before = Tokens {
        access_token: "old".into(),
        refresh_token: "rt-1".into(),
        account_id: Some("acct-123".into()),
        email: Some("dev@example.com".into()),
    };
    let after = oauth.refresh(&before).await.unwrap();
    assert_eq!(after.expires_at(), Some(4_102_444_900));
    assert_eq!(after.refresh_token, "rt-1");
    assert_eq!(after.account_id.as_deref(), Some("acct-123"));
}

#[tokio::test]
async fn a_rejected_refresh_says_to_sign_in_again() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_json(json!({"error": {"code": "refresh_token_reused"}})),
        )
        .mount(&server)
        .await;
    let oauth = OAuth::new(&server.uri());
    let tokens = Tokens {
        access_token: "old".into(),
        refresh_token: "rt-used".into(),
        account_id: None,
        email: None,
    };
    let error = oauth.refresh(&tokens).await.unwrap_err();
    assert!(
        matches!(error, OAuthError::Rejected { status: 400, .. }),
        "{error:?}"
    );
    assert!(
        error.to_string().contains("harness login chatgpt"),
        "{error}"
    );
}

#[test]
fn tokens_round_trip_through_storage_and_never_print() {
    let tokens = Tokens {
        access_token: "at-secret".into(),
        refresh_token: "rt-secret".into(),
        account_id: Some("acct-123".into()),
        email: Some("dev@example.com".into()),
    };
    let stored = tokens.to_json();
    assert_eq!(Tokens::from_json(&stored).unwrap(), tokens);
    let shown = format!("{tokens:?}");
    assert!(!shown.contains("secret"), "{shown}");
    assert!(Tokens::from_json("{}").is_none());
}
