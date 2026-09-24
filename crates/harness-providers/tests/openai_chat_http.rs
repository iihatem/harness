use std::time::Duration;

use futures::StreamExt;
use harness_core::message::ChatRequest;
use harness_core::provider::{FinishReason, Provider, ProviderError, ProviderEvent};
use harness_providers::openai_chat::OpenAiChat;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn sse(chunks: &[&str]) -> String {
    chunks.iter().map(|c| format!("data: {c}\n\n")).collect()
}

fn request() -> ChatRequest {
    ChatRequest {
        model: "m".into(),
        system: "s".into(),
        messages: vec![],
        tools: vec![],
    }
}

#[tokio::test]
async fn streams_events_from_the_server_with_the_api_key() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer sk-test"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            sse(&[
                r#"{"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":"stop"}]}"#,
                "[DONE]",
            ]),
            "text/event-stream",
        ))
        .mount(&server)
        .await;

    let provider = OpenAiChat::new(format!("{}/v1", server.uri()), Some("sk-test".into()));
    let events: Vec<_> = provider.stream(request()).collect().await;
    assert_eq!(
        events,
        vec![
            Ok(ProviderEvent::TextDelta("hi".into())),
            Ok(ProviderEvent::Finished(FinishReason::Stop))
        ]
    );
}

#[tokio::test]
async fn http_errors_carry_status_body_and_retry_after() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "3")
                .set_body_string("slow down"),
        )
        .mount(&server)
        .await;
    let provider = OpenAiChat::new(format!("{}/v1", server.uri()), None);
    let first = provider.stream(request()).next().await.unwrap();
    assert_eq!(
        first,
        Err(ProviderError::Http {
            status: 429,
            body: "slow down".into(),
            retry_after: Some(Duration::from_secs(3))
        })
    );
}

#[tokio::test]
async fn http_date_retry_after_is_ignored_safely() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(503)
                .insert_header("retry-after", "Wed, 21 Oct 2026 07:28:00 GMT"),
        )
        .mount(&server)
        .await;
    let provider = OpenAiChat::new(format!("{}/v1", server.uri()), None);
    let first = provider.stream(request()).next().await.unwrap();
    assert!(matches!(
        first,
        Err(ProviderError::Http {
            status: 503,
            retry_after: None,
            ..
        })
    ));
}

#[tokio::test]
async fn unreachable_servers_are_network_errors() {
    let provider = OpenAiChat::new("http://127.0.0.1:9/v1", None);
    let first = provider.stream(request()).next().await.unwrap();
    assert!(matches!(first, Err(ProviderError::Network(_))));
}
