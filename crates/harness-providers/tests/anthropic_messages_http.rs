//! The Messages provider over HTTP, against a mock server replaying the fixtures.

use futures::StreamExt;
use harness_core::message::{ChatRequest, Message};
use harness_core::provider::{FinishReason, Provider, ProviderError, ProviderEvent};
use harness_providers::anthropic_messages::AnthropicMessages;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/tests/fixtures/anthropic-messages/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

fn sse(body: String) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
}

fn request() -> ChatRequest {
    ChatRequest {
        model: "claude-sonnet-4-5".into(),
        system: "s".into(),
        messages: vec![Message::User {
            content: "hi".into(),
        }],
        ..ChatRequest::default()
    }
}

#[tokio::test]
async fn streams_a_reply_with_the_api_key_and_version() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "sk-ant-api03-test"))
        .and(header("anthropic-version", "2023-06-01"))
        .respond_with(sse(fixture("text.sse")))
        .mount(&server)
        .await;
    let provider = AnthropicMessages::new(
        format!("{}/v1", server.uri()),
        Some("sk-ant-api03-test".into()),
    );
    let events: Vec<_> = provider.stream(request()).collect().await;
    assert_eq!(events[0], Ok(ProviderEvent::TextDelta("Hello".into())));
    assert_eq!(
        events.last(),
        Some(&Ok(ProviderEvent::Finished(FinishReason::Stop)))
    );
    // The key goes in its own header, never as a bearer token.
    let sent = &server.received_requests().await.unwrap()[0];
    assert!(sent.headers.get("authorization").is_none());
}

// Carried from P3's review: both of Anthropic's overflow errors must lead to compaction.
#[tokio::test]
async fn both_context_overflow_errors_are_recognised() {
    for message in [
        "prompt is too long: 208310 tokens > 200000 maximum",
        "input length and `max_tokens` exceed context limit: 188240 + 21333 > 200000, decrease input length or `max_tokens` and try again",
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(400).set_body_string(format!(
                r#"{{"type":"error","error":{{"type":"invalid_request_error","message":"{}"}}}}"#,
                message
            )))
            .mount(&server)
            .await;
        let provider = AnthropicMessages::new(format!("{}/v1", server.uri()), Some("k".into()));
        let error = provider
            .stream(request())
            .next()
            .await
            .unwrap()
            .unwrap_err();
        assert!(error.is_context_overflow(), "{error}");
    }
}

#[tokio::test]
async fn an_overloaded_stream_is_retryable() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(sse(fixture("overloaded.sse")))
        .mount(&server)
        .await;
    let provider = AnthropicMessages::new(format!("{}/v1", server.uri()), Some("k".into()));
    let events: Vec<_> = provider.stream(request()).collect().await;
    assert!(
        matches!(events.last(), Some(Err(e)) if e.is_retryable()),
        "{events:?}"
    );
}

#[tokio::test]
async fn a_stream_cut_before_message_stop_is_a_network_error() {
    let server = MockServer::start().await;
    let cut = fixture("text.sse")
        .split("event: message_delta")
        .next()
        .unwrap()
        .to_string();
    Mock::given(method("POST"))
        .respond_with(sse(cut))
        .mount(&server)
        .await;
    let provider = AnthropicMessages::new(format!("{}/v1", server.uri()), Some("k".into()));
    let events: Vec<_> = provider.stream(request()).collect().await;
    assert!(
        matches!(events.last(), Some(Err(ProviderError::Network(_)))),
        "{events:?}"
    );
}

/// A server that answers one request with its headers, then sends nothing for a minute. It runs
/// on a thread of its own, on real time.
fn silent_after_headers() -> String {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        let _ = socket.read(&mut [0; 8192]);
        let _ = socket.write_all(
            b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n",
        );
        std::thread::sleep(std::time::Duration::from_secs(60));
    });
    url
}

// Ruling on review A M7: a server that sends its headers and then pauses gets 300 s to start its
// reply when hosted, and 30 minutes when the profile says it is local (it may be reading a long
// prompt on a CPU). The test's clock is paused, so those waits pass at once.
#[tokio::test(start_paused = true)]
async fn a_local_server_gets_longer_than_a_hosted_one_to_start_its_reply() {
    for (local, wait, shown) in [(false, 300, "300 s"), (true, 1_800, "30 min")] {
        let provider = AnthropicMessages::new(silent_after_headers(), Some("k".into()));
        let mut request = request();
        request.options.local = local;
        let started = tokio::time::Instant::now();
        let events: Vec<_> = provider.stream(request).collect().await;
        let waited = started.elapsed().as_secs();
        match events.last() {
            // Final review, I-1: the local server's is not retried.
            Some(Err(
                error @ ProviderError::NoStart {
                    message,
                    local: said,
                },
            )) => {
                assert!(
                    message.contains(&format!("did not start its reply within {shown}")),
                    "{message}"
                );
                assert_eq!(*said, local);
                assert_eq!(error.is_retryable(), !local);
            }
            other => panic!("{other:?}"),
        }
        assert!((wait..wait + 5).contains(&waited), "{waited}");
    }
}
