//! The `chatgpt` provider: the Responses protocol to ChatGPT's backend with a signed-in
//! account, whose tokens are refreshed before they expire and after a 401. A mock server plays
//! both the backend and the authorization server; tokens live in a file store in a temporary
//! directory.
#![cfg(feature = "chatgpt-login")]

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
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
        Signed::in_profile(stored, "default")
    }

    fn in_profile(stored: &Tokens, profile: &str) -> Signed {
        let dir = tempfile::tempdir().unwrap();
        let credentials = Arc::new(Credentials::with_keychain(dir.path(), None));
        credentials
            .set("chatgpt", profile, &stored.to_json())
            .unwrap();
        credentials.use_profile("chatgpt", profile).unwrap();
        Signed { dir, credentials }
    }

    /// The account as another harness process would load it: with a store of its own over the
    /// same data directory.
    fn another_process(&self) -> Signed {
        Signed {
            dir: tempfile::tempdir().unwrap(),
            credentials: Arc::new(Credentials::with_keychain(self.dir.path(), None)),
        }
    }

    fn profile(&self) -> String {
        self.credentials.active_profile("chatgpt").unwrap()
    }

    fn auth(&self, server: &MockServer) -> ChatGptAuth {
        ChatGptAuth::load(
            self.credentials.clone(),
            &self.profile(),
            OAuth::new(&server.uri()).unwrap(),
        )
        .unwrap()
        .expect("signed in")
    }

    fn provider(&self, server: &MockServer) -> OpenAiResponses {
        OpenAiResponses::chatgpt(
            format!("{}/backend-api/codex", server.uri()),
            Arc::new(self.auth(server)),
        )
    }

    fn stored(&self) -> Tokens {
        let json = self
            .credentials
            .get("chatgpt", &self.profile())
            .unwrap()
            .unwrap();
        Tokens::from_json(&json).unwrap()
    }
}

/// The refresh requests `server` received.
async fn refreshes(server: &MockServer) -> Vec<Value> {
    server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.url.path() == "/oauth/token")
        .map(|r| r.body_json().unwrap())
        .collect()
}

/// Answers a refresh of `refresh_token` with `access` and the new refresh token `next`.
async fn mock_refresh(server: &MockServer, refresh_token: &str, access: &str, next: &str) {
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_string_contains(
            format!(r#""refresh_token":"{refresh_token}""#).as_str(),
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"access_token": access, "refresh_token": next})),
        )
        .mount(server)
        .await;
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

// Review A M1: every model ChatGPT's backend serves reasons, so each streams its summaries,
// whatever its name.
#[tokio::test]
async fn chatgpt_models_are_asked_for_reasoning_summaries() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(text_reply())
        .mount(&server)
        .await;
    let signed = Signed::new(&tokens(&access_token("a", 3600), "rt-1"));
    let request = ChatRequest {
        model: "codex-mini-latest".into(),
        ..request()
    };
    let events: Vec<_> = signed.provider(&server).stream(request).collect().await;
    assert!(events.iter().all(Result::is_ok), "{events:?}");
    let body: Value = server.received_requests().await.unwrap()[0]
        .body_json()
        .unwrap();
    assert_eq!(body["reasoning"], json!({"summary": "auto"}));
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
        // Review C, M8: refreshes identify as the Codex CLI too.
        .and(header("originator", "codex_cli_rs"))
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
    // Review C, M2: renewed and still refused, the sign-in has to be done again.
    assert!(
        error.to_string().contains("harness login chatgpt"),
        "{error}"
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

// Review B, I3 (and C, M3): a damaged `accounts.toml` must not quietly send the requests as the
// default profile's account.
#[test]
fn a_damaged_accounts_file_never_falls_back_to_the_default_account() {
    let dir = tempfile::tempdir().unwrap();
    let credentials = Arc::new(Credentials::with_keychain(dir.path(), None));
    credentials
        .set("chatgpt", "default", &tokens("at-default", "rt").to_json())
        .unwrap();
    credentials
        .set("chatgpt", "work", &tokens("at-work", "rt").to_json())
        .unwrap();
    credentials.use_profile("chatgpt", "work").unwrap();
    std::fs::write(
        dir.path().join("accounts.toml"),
        "[active]\nchatgpt = work\n",
    )
    .unwrap();
    let error =
        resolve("chatgpt/gpt-5.5", &BTreeMap::new(), Stored(credentials)).expect_err("an error");
    let text = error.to_string();
    assert!(text.contains("accounts.toml"), "{text}");
    assert!(text.contains("line 2"), "{text}");
    assert!(
        !matches!(error, ResolveError::NotSignedIn { .. }),
        "{error:?}"
    );
}

// Review C, M3: nor does a damaged credentials file read as "not signed in".
#[test]
fn a_damaged_credentials_file_is_not_taken_for_a_sign_out() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("credentials.json"), "{not json").unwrap();
    let credentials = Arc::new(Credentials::with_keychain(dir.path(), None));
    let error =
        resolve("chatgpt/gpt-5.5", &BTreeMap::new(), Stored(credentials)).expect_err("an error");
    assert!(error.to_string().contains("credentials.json"), "{error}");
    assert!(
        !matches!(error, ResolveError::NotSignedIn { .. }),
        "{error:?}"
    );
}

// Review C, M2: the hint signs in the profile in use, not `default`.
#[tokio::test]
async fn a_refused_refresh_names_the_profile_to_sign_in_again() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_json(json!({"error": {"code": "refresh_token_expired"}})),
        )
        .mount(&server)
        .await;
    let signed = Signed::in_profile(&tokens(&access_token("old", 60), "rt-1"), "work");
    let error = signed.auth(&server).current().await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("`harness login chatgpt --profile work`"),
        "{error}"
    );
}

// Review C, I2: refreshed tokens are the only valid ones once the server has rotated the refresh
// token; a store that fails must not drop them.
#[tokio::test]
async fn refreshed_tokens_are_kept_when_they_cannot_be_stored() {
    let server = MockServer::start().await;
    let first = access_token("first", 7200);
    let second = access_token("second", 7200);
    mock_refresh(&server, "rt-1", &first, "rt-2").await;
    mock_refresh(&server, "rt-2", &second, "rt-3").await;
    let signed = Signed::new(&tokens(&access_token("old", 60), "rt-1"));
    let auth = signed.auth(&server);
    let data = signed.dir.path();
    std::fs::set_permissions(data, std::fs::Permissions::from_mode(0o500)).unwrap();
    let current = auth.current().await;
    std::fs::set_permissions(data, std::fs::Permissions::from_mode(0o700)).unwrap();
    // The request goes on with the fresh tokens, and the user hears why they are not stored.
    assert_eq!(current.unwrap().access_token, first);
    let warnings = signed.credentials.take_warnings();
    assert!(
        warnings.iter().any(|w| w.contains("could not be stored")),
        "{warnings:?}"
    );
    assert_eq!(signed.stored().refresh_token, "rt-1");
    // The next renewal uses the fresh refresh token, and stores what it gets.
    let renewed = auth.after_unauthorized(&first).await.unwrap();
    assert_eq!(renewed.access_token, second);
    let sent: Vec<Value> = refreshes(&server).await;
    let used: Vec<&str> = sent
        .iter()
        .map(|b| b["refresh_token"].as_str().unwrap())
        .collect();
    assert_eq!(used, ["rt-1", "rt-2"]);
    assert_eq!(signed.stored().refresh_token, "rt-3");
}

// Review C, I3: two processes renewing at once would both spend the single-use refresh token.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_processes_renewing_at_once_send_one_refresh() {
    let server = MockServer::start().await;
    let new = access_token("new", 7200);
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"access_token": new, "refresh_token": "rt-2"}))
                .set_delay(std::time::Duration::from_millis(300)),
        )
        .mount(&server)
        .await;
    let signed = Signed::new(&tokens(&access_token("old", 60), "rt-1"));
    let other = signed.another_process();
    let (ours, theirs) = (signed.auth(&server), other.auth(&server));
    let (a, b) = tokio::join!(ours.current(), theirs.current());
    assert_eq!(a.unwrap().access_token, new);
    assert_eq!(b.unwrap().access_token, new);
    assert_eq!(refreshes(&server).await.len(), 1);
    assert_eq!(signed.stored().refresh_token, "rt-2");
    // The lock file is private.
    let lock = std::fs::read_dir(signed.dir.path())
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.to_string_lossy().ends_with("chatgpt+default.lock"))
        .expect("a lock file");
    let mode = std::fs::metadata(lock).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}

// Review C, I3: a refresh refused because another process (without the lock: an older harness)
// spent the refresh token first is saved by the tokens that process stored.
#[tokio::test]
async fn a_refused_refresh_falls_back_on_tokens_stored_meanwhile() {
    let server = MockServer::start().await;
    let theirs = access_token("theirs", 7200);
    let signed = Signed::new(&tokens(&access_token("old", 60), "rt-1"));
    let other = signed.another_process();
    let stored_meanwhile = tokens(&theirs, "rt-2").to_json();
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(move |_: &wiremock::Request| {
            other
                .credentials
                .set("chatgpt", "default", &stored_meanwhile)
                .unwrap();
            ResponseTemplate::new(400)
                .set_body_json(json!({"error": {"code": "refresh_token_reused"}}))
        })
        .expect(1)
        .mount(&server)
        .await;
    let current = signed.auth(&server).current().await.unwrap();
    assert_eq!(current.access_token, theirs);
}

// Review C, M10: signing the same profile in to another account elsewhere does not move a running
// session to that account, nor does the session's renewal undo that sign-in.
#[tokio::test]
async fn a_renewal_keeps_the_sessions_account() {
    let server = MockServer::start().await;
    let renewed = access_token("renewed", 7200);
    mock_refresh(&server, "rt-1", &renewed, "rt-2").await;
    let signed = Signed::new(&tokens(&access_token("old", 60), "rt-1"));
    let auth = signed.auth(&server);
    let elsewhere = Tokens {
        account_id: Some("acct-OTHER".into()),
        ..tokens(&access_token("other", 7200), "rt-other")
    };
    signed
        .another_process()
        .credentials
        .set("chatgpt", "default", &elsewhere.to_json())
        .unwrap();
    let current = auth.current().await.unwrap();
    assert_eq!(current.access_token, renewed);
    assert_eq!(current.account_id.as_deref(), Some("acct-123"));
    assert_eq!(signed.stored(), elsewhere);
    let warnings = signed.credentials.take_warnings();
    assert_eq!(
        warnings
            .iter()
            .filter(|w| w.contains("another ChatGPT account"))
            .count(),
        1,
        "{warnings:?}"
    );
    // Once is enough.
    auth.after_unauthorized(&renewed).await.ok();
    let warnings = signed.credentials.take_warnings();
    assert!(
        !warnings
            .iter()
            .any(|w| w.contains("another ChatGPT account")),
        "{warnings:?}"
    );
}

// Review C, M1: only the sign-in server saying the refresh token is no good means signing in
// again. A server that cannot answer now is retried later, and the tokens stay.
#[tokio::test]
async fn only_a_refused_refresh_token_means_signing_in_again() {
    for (status, body, signed_out) in [
        (503, json!({"error": "unavailable"}), false),
        (429, json!({"error": "slow down"}), false),
        (500, json!({}), false),
        (401, json!({"error": "unauthorized"}), true),
        (400, json!({"error": "invalid_grant"}), true),
        (
            400,
            json!({"error": {"code": "refresh_token_reused"}}),
            true,
        ),
        (
            400,
            json!({"error": {"code": "refresh_token_invalidated"}}),
            true,
        ),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(ResponseTemplate::new(status).set_body_json(body.clone()))
            .mount(&server)
            .await;
        let before = tokens(&access_token("old", 60), "rt-1");
        let signed = Signed::new(&before);
        let error = signed.auth(&server).current().await.unwrap_err();
        let says_sign_in = error.to_string().contains("harness login chatgpt");
        assert_eq!(says_sign_in, signed_out, "{status} {body}: {error}");
        assert_eq!(
            error.is_retryable(),
            !signed_out,
            "{status} {body}: {error}"
        );
        assert_eq!(signed.stored(), before);
    }
    // An unreachable server too.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let issuer = format!("http://{}", closed.local_addr().unwrap());
    drop(closed);
    let signed = Signed::new(&tokens(&access_token("old", 60), "rt-1"));
    let auth = ChatGptAuth::load(
        signed.credentials.clone(),
        "default",
        OAuth::new(&issuer).unwrap(),
    )
    .unwrap()
    .unwrap();
    let error = auth.current().await.unwrap_err();
    assert!(error.is_retryable(), "{error}");
    assert!(!error.to_string().contains("harness login"), "{error}");
}

// Re-review B+C, R4: after `harness logout` elsewhere, a running session must not go on renewing
// and using the account. It ends at its next renewal, as Codex's does, naming the profile.
#[tokio::test]
async fn a_profile_signed_out_elsewhere_ends_the_session_at_its_next_renewal() {
    let server = MockServer::start().await;
    mock_refresh(&server, "rt-1", &access_token("renewed", 7200), "rt-2").await;
    let old = access_token("old", 60);
    let signed = Signed::in_profile(&tokens(&old, "rt-1"), "work");
    let auth = signed.auth(&server);
    assert!(
        signed
            .another_process()
            .credentials
            .remove("chatgpt", "work")
            .unwrap()
    );
    let error = auth.current().await.unwrap_err();
    let text = error.to_string();
    assert!(text.contains("signed out"), "{text}");
    assert!(text.contains("profile `work`"), "{text}");
    assert!(
        text.contains("`harness login chatgpt --profile work`"),
        "{text}"
    );
    assert!(!error.is_retryable(), "{text}");
    // Every later request ends the same way, a 401 included, and nothing is refreshed or stored.
    assert!(auth.current().await.is_err());
    assert!(auth.after_unauthorized(&old).await.is_err());
    assert!(refreshes(&server).await.is_empty());
    assert_eq!(signed.credentials.get("chatgpt", "work").unwrap(), None);
}

// Re-review B+C, R5: a refusal that retrying will not fix says what to do should it persist; one
// that retrying may fix does not (see `only_a_refused_refresh_token_means_signing_in_again`).
#[tokio::test]
async fn a_refresh_failing_for_an_unknown_reason_says_what_to_do_if_it_persists() {
    for (status, body) in [
        (400, json!({"error": "unsupported_something"})),
        (403, json!({})),
        // A 200 without a token.
        (200, json!({"token_type": "Bearer"})),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(ResponseTemplate::new(status).set_body_json(body.clone()))
            .mount(&server)
            .await;
        let before = tokens(&access_token("old", 60), "rt-1");
        let signed = Signed::in_profile(&before, "work");
        let error = signed.auth(&server).current().await.unwrap_err();
        let text = error.to_string();
        assert!(text.contains("if this persists"), "{status} {body}: {text}");
        assert!(
            text.contains("`harness login chatgpt --profile work`"),
            "{status} {body}: {text}"
        );
        assert!(!error.is_retryable(), "{status} {body}: {text}");
        assert_eq!(signed.stored(), before);
    }
}

// Re-review B+C, R5: waiting for another process's renewal (up to 180 s) is not silent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_renewal_waiting_for_another_process_says_so_once() {
    let server = MockServer::start().await;
    mock_refresh(&server, "rt-1", &access_token("new", 7200), "rt-2").await;
    let signed = Signed::new(&tokens(&access_token("old", 60), "rt-1"));
    // That the test store has no keychain.
    signed.credentials.take_warnings();
    let other = signed.another_process();
    let held = other
        .credentials
        .try_lock_renewal("chatgpt", "default")
        .unwrap()
        .expect("the renewal lock");
    let auth = Arc::new(signed.auth(&server));
    let renewal = tokio::spawn({
        let auth = auth.clone();
        async move { auth.current().await }
    });
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    assert!(signed.credentials.take_warnings().is_empty());
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    let warnings = signed.credentials.take_warnings();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    for part in [
        "waiting for another harness process",
        "ChatGPT sign-in",
        "profile `default`",
    ] {
        assert!(warnings[0].contains(part), "{part}: {warnings:?}");
    }
    drop(held);
    renewal.await.unwrap().unwrap();
    let warnings = signed.credentials.take_warnings();
    assert!(
        !warnings.iter().any(|w| w.contains("waiting")),
        "{warnings:?}"
    );
}

// Re-review B+C, R3: a sign-out that lands while a renewal is in flight waits for it, instead of
// having the renewal store its tokens over it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_sign_out_during_a_renewal_is_not_undone() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(
                    json!({"access_token": access_token("new", 7200), "refresh_token": "rt-2"}),
                )
                .set_delay(std::time::Duration::from_millis(600)),
        )
        .mount(&server)
        .await;
    let signed = Signed::new(&tokens(&access_token("old", 60), "rt-1"));
    let auth = Arc::new(signed.auth(&server));
    let renewal = tokio::spawn({
        let auth = auth.clone();
        async move { auth.current().await }
    });
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    let other = signed.another_process();
    let mut notes = Vec::new();
    let lock = other
        .credentials
        .lock_renewal("chatgpt", "default", "signing out of ChatGPT", |note| {
            notes.push(note)
        })
        .await;
    assert!(lock.is_some());
    assert!(other.credentials.remove("chatgpt", "default").unwrap());
    drop(lock);
    renewal.await.unwrap().unwrap();
    assert_eq!(signed.credentials.get("chatgpt", "default").unwrap(), None);
    assert!(notes.is_empty(), "{notes:?}");
}
