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
        ..ChatRequest::default()
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

// Review Focus: a server that just closes the connection mid-reply (no finish_reason, no [DONE])
// must not be mistaken for a normal, complete stop.
#[tokio::test]
async fn stream_ending_without_a_finish_reason_or_done_is_a_network_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            sse(&[r#"{"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":null}]}"#]),
            "text/event-stream",
        ))
        .mount(&server)
        .await;

    let provider = OpenAiChat::new(format!("{}/v1", server.uri()), None);
    let events: Vec<_> = provider.stream(request()).collect().await;
    assert!(
        matches!(events.last(), Some(Err(ProviderError::Network(_)))),
        "{events:?}"
    );
}

#[tokio::test]
async fn unreachable_servers_are_network_errors() {
    let provider = OpenAiChat::new("http://127.0.0.1:9/v1", None);
    let first = provider.stream(request()).next().await.unwrap();
    assert!(matches!(first, Err(ProviderError::Network(_))));
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
        let provider = OpenAiChat::new(silent_after_headers(), None);
        let mut request = request();
        request.options.local = local;
        let started = tokio::time::Instant::now();
        let events: Vec<_> = provider.stream(request).collect().await;
        let waited = started.elapsed().as_secs();
        match events.last() {
            Some(Err(error @ ProviderError::Network(message))) => {
                assert!(
                    message.contains(&format!("did not start its reply within {shown}")),
                    "{message}"
                );
                assert!(error.is_retryable());
            }
            other => panic!("{other:?}"),
        }
        assert!((wait..wait + 5).contains(&waited), "{waited}");
    }
}
