//! The `chatgpt` provider: the Responses protocol to ChatGPT's backend with a signed-in
//! account, whose tokens are refreshed before they expire and after a 401. A mock server plays
//! both the backend and the authorization server; tokens live in a file store in a temporary
//! directory.
#![cfg(feature = "chatgpt-login")]

use std::collections::BTreeMap;
use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures::StreamExt;
use harness_config::config::Protocol;
use harness_core::message::{ChatRequest, Message, RequestOptions};
use harness_core::provider::{Provider, ProviderError, ProviderEvent};
use harness_providers::chatgpt::auth::ChatGptAuth;
use harness_providers::chatgpt::oauth::{OAuth, Tokens};
use harness_providers::credentials::Credentials;
use harness_providers::openai_responses::OpenAiResponses;
use harness_providers::registry::{ResolveError, Secrets, resolve};
use serde_json::{Value, json};
use wiremock::matchers::{body_string_contains, header, method, path};
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

/// An access token that expires `secs` from now.
fn access_token(label: &str, secs: u64) -> String {
    jwt(json!({"exp": harness_core::time::now_unix() + secs, "label": label}))
}

fn tokens(access: &str, refresh: &str) -> Tokens {
    Tokens {
        access_token: access.to_string(),
        refresh_token: refresh.to_string(),
        account_id: Some("acct-123".into()),
        email: Some("dev@example.com".into()),
    }
}

fn text_reply() -> ResponseTemplate {
    let body = [
        json!({"type": "response.output_text.delta", "output_index": 0, "delta": "hi from chatgpt"}),
        json!({"type": "response.completed", "response": {"status": "completed"}}),
    ]
    .iter()
    .map(|e| format!("data: {e}\n\n"))
    .collect::<String>();
    ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
}

struct Signed {
    dir: tempfile::TempDir,
    credentials: Arc<Credentials>,
}

impl Signed {
    fn new(stored: &Tokens) -> Signed {
        let dir = tempfile::tempdir().unwrap();
        let credentials = Arc::new(Credentials::with_keychain(dir.path(), None));
        credentials
            .set("chatgpt", "default", &stored.to_json())
            .unwrap();
        Signed { dir, credentials }
    }

    fn provider(&self, server: &MockServer) -> OpenAiResponses {
        let auth = ChatGptAuth::load(
            self.credentials.clone(),
            "default",
            OAuth::new(&server.uri()),
        )
        .unwrap()
        .expect("signed in");
        OpenAiResponses::chatgpt(
            format!("{}/backend-api/codex", server.uri()),
            Arc::new(auth),
        )
    }

    fn stored(&self) -> Tokens {
        Tokens::from_json(&self.credentials.get("chatgpt", "default").unwrap().unwrap()).unwrap()
    }
}

fn request() -> ChatRequest {
    ChatRequest {
        model: "gpt-5.5".into(),
        system: "s".into(),
        messages: vec![Message::User {
            content: "hi".into(),
        }],
        options: RequestOptions {
            max_output_tokens: Some(1000),
            ..RequestOptions::default()
        },
        ..ChatRequest::default()
    }
}

async fn text_of(provider: &OpenAiResponses) -> Result<String, ProviderError> {
    let mut text = String::new();
    let mut stream = provider.stream(request());
    while let Some(event) = stream.next().await {
        if let ProviderEvent::TextDelta(delta) = event? {
            text.push_str(&delta);
        }
    }
    Ok(text)
}

#[tokio::test]
async fn requests_carry_the_token_and_the_account() {
    let server = MockServer::start().await;
    let token = access_token("a", 3600);
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/responses"))
        .and(header("authorization", format!("Bearer {token}").as_str()))
        .and(header("chatgpt-account-id", "acct-123"))
        .and(header("originator", "codex_cli_rs"))
        .respond_with(text_reply())
        .expect(1)
        .mount(&server)
        .await;
    let signed = Signed::new(&tokens(&token, "rt-1"));
    let text = text_of(&signed.provider(&server)).await.unwrap();
    assert_eq!(text, "hi from chatgpt");
    let body: Value = server.received_requests().await.unwrap()[0]
        .body_json()
        .unwrap();
    assert_eq!(body["store"], false);
    // ChatGPT's backend takes no output limit.
    assert!(body.get("max_output_tokens").is_none(), "{body}");
}

// Spec: "Expired access token".
#[tokio::test]
async fn a_401_refreshes_the_token_and_retries_once() {
    let server = MockServer::start().await;
    let old = access_token("old", 3600);
    let new = access_token("new", 7200);
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/responses"))
        .and(header("authorization", format!("Bearer {old}").as_str()))
        .respond_with(ResponseTemplate::new(401).set_body_string("token expired"))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/responses"))
        .and(header("authorization", format!("Bearer {new}").as_str()))
        .respond_with(text_reply())
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_string_contains(r#""refresh_token":"rt-1""#))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"access_token": new, "refresh_token": "rt-2"})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let signed = Signed::new(&tokens(&old, "rt-1"));
    assert_eq!(
        text_of(&signed.provider(&server)).await.unwrap(),
        "hi from chatgpt"
    );
    // The new tokens are stored, for the next run.
    let stored = signed.stored();
    assert_eq!(stored.access_token, new);
    assert_eq!(stored.refresh_token, "rt-2");
    assert_eq!(stored.account_id.as_deref(), Some("acct-123"));
    drop(signed.dir);
}

#[tokio::test]
async fn a_token_about_to_expire_is_refreshed_first() {
    let server = MockServer::start().await;
    let old = access_token("old", 60);
    let new = access_token("new", 7200);
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/responses"))
        .and(header("authorization", format!("Bearer {new}").as_str()))
        .respond_with(text_reply())
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token": new})))
        .expect(1)
        .mount(&server)
        .await;
    let signed = Signed::new(&tokens(&old, "rt-1"));
    text_of(&signed.provider(&server)).await.unwrap();
    assert_eq!(signed.stored().refresh_token, "rt-1");
}

// Spec: "Two sessions refresh at once". Refresh tokens are used once: asking again with the
// same one would fail, so the tokens another process stored are used instead.
#[tokio::test]
async fn tokens_another_process_refreshed_are_used_without_asking_again() {
    let server = MockServer::start().await;
    let old = access_token("old", 3600);
    let theirs = access_token("theirs", 7200);
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/responses"))
        .and(header("authorization", format!("Bearer {old}").as_str()))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/responses"))
        .and(header("authorization", format!("Bearer {theirs}").as_str()))
        .respond_with(text_reply())
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_json(json!({"error": {"code": "refresh_token_reused"}})),
        )
        .expect(0)
        .mount(&server)
        .await;
    let signed = Signed::new(&tokens(&old, "rt-1"));
    let provider = signed.provider(&server);
    // Another harness process refreshes after this one loaded the tokens.
    signed
        .credentials
        .set("chatgpt", "default", &tokens(&theirs, "rt-2").to_json())
        .unwrap();
    assert_eq!(text_of(&provider).await.unwrap(), "hi from chatgpt");
}

#[tokio::test]
async fn a_refused_refresh_says_to_sign_in_again() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/responses"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_json(json!({"error": {"code": "refresh_token_expired"}})),
        )
        .mount(&server)
        .await;
    let signed = Signed::new(&tokens(&access_token("old", 3600), "rt-1"));
    let error = text_of(&signed.provider(&server)).await.unwrap_err();
    assert!(
        error.to_string().contains("harness login chatgpt"),
        "{error}"
    );
    assert!(!error.is_retryable());
}

#[tokio::test]
async fn a_second_401_is_an_error_not_a_loop() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/responses"))
        .respond_with(ResponseTemplate::new(401).set_body_string("no"))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"access_token": access_token("new", 7200)})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let signed = Signed::new(&tokens(&access_token("old", 3600), "rt-1"));
    let error = text_of(&signed.provider(&server)).await.unwrap_err();
    assert!(
        matches!(error, ProviderError::Http { status: 401, .. }),
        "{error:?}"
    );
}

/// The credential store and nothing in the environment.
struct Stored(Arc<Credentials>);

impl Secrets for Stored {
    fn env(&self, _var: &str) -> Option<String> {
        None
    }

    fn credentials(&self) -> Option<Arc<Credentials>> {
        Some(self.0.clone())
    }
}

#[test]
fn chatgpt_models_need_a_signed_in_account() {
    let dir = tempfile::tempdir().unwrap();
    let credentials = Arc::new(Credentials::with_keychain(dir.path(), None));
    let none = BTreeMap::new();
    let error = resolve("chatgpt/gpt-5.5", &none, Stored(credentials.clone())).unwrap_err();
    assert_eq!(
        error,
        ResolveError::NotSignedIn {
            provider: "chatgpt".into(),
            profile: "default".into()
        }
    );
    assert!(
        error.to_string().contains("harness login chatgpt"),
        "{error}"
    );
    credentials
        .set("chatgpt", "default", &tokens("at", "rt").to_json())
        .unwrap();
    let r = resolve("chatgpt/gpt-5.5", &none, Stored(credentials.clone())).unwrap();
    assert_eq!(r.model, "gpt-5.5");
    assert_eq!(r.protocol, Protocol::OpenaiResponses);
    assert_eq!(r.base_url, "https://chatgpt.com/backend-api/codex");
    // Another profile is signed in separately.
    credentials.use_profile("chatgpt", "work").unwrap();
    let error = resolve("chatgpt/gpt-5.5", &none, Stored(credentials)).unwrap_err();
    assert!(error.to_string().contains("--profile work"), "{error}");
}
