//! Redirects: a provider's or the sign-in server's client follows one only within the origin
//! (scheme, host and port) of the request, so that a key or token sent in a header or a body never
//! reaches another host. Two mock servers on different ports are two origins.

use futures::StreamExt;
use harness_config::config::Protocol;
use harness_core::message::{ChatRequest, Message};
use harness_core::provider::{Provider, ProviderError, ProviderEvent};
use harness_providers::anthropic_messages::AnthropicMessages;
use harness_providers::discovery::{Endpoint, REMOTE_PROBE_TIMEOUT, list_models};
use harness_providers::openai_chat::OpenAiChat;
use harness_providers::openai_responses::OpenAiResponses;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const KEY: &str = "sk-ant-api03-PROBEKEY-0123456789";

/// The redirect statuses a client could follow: 302 re-sends the headers, 307 and 308 the body
/// too.
const REDIRECTS: [u16; 3] = [302, 307, 308];

/// A server that redirects every request, with `status`, to the same path on `elsewhere`.
async fn redirecting_to(elsewhere: &MockServer, status: u16) -> MockServer {
    let server = MockServer::start().await;
    let target = elsewhere.uri();
    Mock::given(wiremock::matchers::any())
        .respond_with(move |request: &wiremock::Request| {
            ResponseTemplate::new(status)
                .insert_header("location", format!("{target}{}", request.url.path()))
        })
        .mount(&server)
        .await;
    server
}

/// A server that answers anything, to see whether anything reached it.
async fn elsewhere() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    server
}

fn request() -> ChatRequest {
    ChatRequest {
        model: "m".into(),
        system: "s".into(),
        messages: vec![Message::User {
            content: "hi".into(),
        }],
        ..ChatRequest::default()
    }
}

/// The error `provider`'s stream ends with.
async fn error_of(provider: &dyn Provider) -> ProviderError {
    let events: Vec<_> = provider.stream(request()).collect().await;
    match events.last() {
        Some(Err(error)) => error.clone(),
        other => panic!("{other:?}"),
    }
}

/// Checks that a redirect from `from` to `to` was refused, saying so, and that `to` received
/// nothing.
async fn refused(error: &ProviderError, from: &MockServer, to: &MockServer) {
    let text = error.to_string();
    assert!(text.contains(&from.uri()), "{text}");
    assert!(text.contains(&to.uri()), "{text}");
    assert!(text.contains("another origin"), "{text}");
    assert!(!text.contains(KEY), "{text}");
    // Following it would be refused again.
    assert!(!error.is_retryable(), "{error:?}");
    let reached = to.received_requests().await.unwrap();
    assert!(reached.is_empty(), "{reached:?}");
}

// Final review, M-1: reqwest keeps `x-api-key` on a redirect to another host (it strips only
// `Authorization`, cookies and proxy credentials), and re-sends the body on a 307 or 308.
#[tokio::test]
async fn an_api_key_never_follows_a_redirect_to_another_origin() {
    for status in REDIRECTS {
        let to = elsewhere().await;
        let from = redirecting_to(&to, status).await;
        let providers: [Box<dyn Provider>; 3] = [
            Box::new(AnthropicMessages::new(
                format!("{}/v1", from.uri()),
                Some(KEY.into()),
            )),
            Box::new(OpenAiChat::new(
                format!("{}/v1", from.uri()),
                Some(KEY.into()),
            )),
            Box::new(OpenAiResponses::new(
                format!("{}/v1", from.uri()),
                Some(KEY.into()),
            )),
        ];
        for provider in providers {
            let error = error_of(provider.as_ref()).await;
            refused(&error, &from, &to).await;
        }
    }
}

// A redirect within the origin is followed.
#[tokio::test]
async fn a_redirect_within_the_origin_is_followed() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(307).insert_header("location", "/v2/messages"))
        .mount(&server)
        .await;
    let events = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"moved\"}}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
    Mock::given(method("POST"))
        .and(path("/v2/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(events, "text/event-stream"))
        .mount(&server)
        .await;
    let provider = AnthropicMessages::new(format!("{}/v1", server.uri()), Some(KEY.into()));
    let events: Vec<_> = provider.stream(request()).collect().await;
    assert!(
        events.contains(&Ok(ProviderEvent::TextDelta("moved".into()))),
        "{events:?}"
    );
}

// Listing models sends the key too.
#[tokio::test]
async fn a_listing_never_follows_a_redirect_to_another_origin() {
    for status in REDIRECTS {
        let to = elsewhere().await;
        let from = redirecting_to(&to, status).await;
        let endpoints: Vec<Endpoint> = [Protocol::AnthropicMessages, Protocol::OpenaiChat]
            .into_iter()
            .map(|protocol| Endpoint {
                provider: "p".into(),
                base_url: format!("{}/v1", from.uri()),
                api_key: Some(KEY.into()),
                protocol,
            })
            .collect();
        assert!(
            list_models(&endpoints, REMOTE_PROBE_TIMEOUT)
                .await
                .is_empty()
        );
        let reached = to.received_requests().await.unwrap();
        assert!(reached.is_empty(), "{reached:?}");
    }
}

#[cfg(feature = "chatgpt-login")]
mod chatgpt {
    use std::sync::Arc;

    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use harness_providers::chatgpt::auth::ChatGptAuth;
    use harness_providers::chatgpt::oauth::{OAuth, Tokens};
    use harness_providers::credentials::Credentials;
    use serde_json::{Value, json};

    use super::*;

    fn jwt(claims: Value) -> String {
        let part = |value: &Value| URL_SAFE_NO_PAD.encode(serde_json::to_vec(value).unwrap());
        format!(
            "{}.{}.{}",
            part(&json!({"alg": "none"})),
            part(&claims),
            URL_SAFE_NO_PAD.encode(b"sig")
        )
    }

    fn tokens(expires_in: u64) -> Tokens {
        Tokens {
            access_token: jwt(json!({"exp": harness_core::time::now_unix() + expires_in})),
            refresh_token: "rt-PROBE-refresh-token".into(),
            account_id: Some("acct-123".into()),
            email: None,
        }
    }

    // Final review, M-1: the refresh token travels in a JSON body, which a 307 or 308 re-sends.
    #[tokio::test]
    async fn a_refresh_token_never_follows_a_redirect_to_another_origin() {
        for status in REDIRECTS {
            let to = elsewhere().await;
            let from = redirecting_to(&to, status).await;
            let oauth = OAuth::new(&from.uri()).unwrap();
            let error = oauth.refresh(&tokens(3600)).await.unwrap_err().to_string();
            assert!(error.contains(&to.uri()), "{error}");
            assert!(error.contains("another origin"), "{error}");
            let reached = to.received_requests().await.unwrap();
            assert!(reached.is_empty(), "{reached:?}");
        }
    }

    // ChatGPT's backend is sent the access token.
    #[tokio::test]
    async fn chatgpts_backend_never_follows_a_redirect_to_another_origin() {
        for status in REDIRECTS {
            let to = elsewhere().await;
            let from = redirecting_to(&to, status).await;
            let dir = tempfile::tempdir().unwrap();
            let credentials = Arc::new(Credentials::with_keychain(dir.path(), None));
            credentials
                .set("chatgpt", "default", &tokens(3600).to_json())
                .unwrap();
            let auth = ChatGptAuth::load(credentials, "default", OAuth::new(&from.uri()).unwrap())
                .unwrap()
                .unwrap();
            let provider = OpenAiResponses::chatgpt(
                format!("{}/backend-api/codex", from.uri()),
                Arc::new(auth),
            );
            let error = error_of(&provider).await;
            let text = error.to_string();
            assert!(text.contains(&to.uri()), "{text}");
            assert!(!error.is_retryable(), "{error:?}");
            let reached = to.received_requests().await.unwrap();
            assert!(reached.is_empty(), "{reached:?}");
        }
    }
}
