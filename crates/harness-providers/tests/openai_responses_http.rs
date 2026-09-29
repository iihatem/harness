//! The Responses provider over HTTP, against a mock server replaying the fixtures.

use futures::StreamExt;
use harness_core::message::{ChatRequest, Message};
use harness_core::provider::{FinishReason, Provider, ProviderError, ProviderEvent};
use harness_providers::openai_responses::OpenAiResponses;
use serde_json::json;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/tests/fixtures/openai-responses/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

fn sse(body: String) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
}

fn request() -> ChatRequest {
    ChatRequest {
        model: "gpt-5".into(),
        system: "s".into(),
        messages: vec![Message::User {
            content: "hi".into(),
        }],
        ..ChatRequest::default()
    }
}

#[tokio::test]
async fn streams_a_reply_with_the_api_key() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(header("authorization", "Bearer sk-test"))
        .and(body_partial_json(
            json!({"model": "gpt-5", "instructions": "s", "store": false, "stream": true}),
        ))
        .respond_with(sse(fixture("text.sse")))
        .mount(&server)
        .await;
    let provider = OpenAiResponses::new(format!("{}/v1", server.uri()), Some("sk-test".into()));
    let events: Vec<_> = provider.stream(request()).collect().await;
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            Ok(ProviderEvent::TextDelta(t)) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "Hello, world");
    assert_eq!(
        events.last(),
        Some(&Ok(ProviderEvent::Finished(FinishReason::Stop)))
    );
}

#[tokio::test]
async fn tool_calls_come_through() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(sse(fixture("tool_call.sse")))
        .mount(&server)
        .await;
    let provider = OpenAiResponses::new(format!("{}/v1", server.uri()), None);
    let events: Vec<_> = provider.stream(request()).collect().await;
    let names: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            Ok(ProviderEvent::ToolCall(call)) => Some(call.name.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(names, ["read", "glob"]);
}

#[tokio::test]
async fn a_context_overflow_response_is_recognised() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(400).set_body_string(
            r#"{"error":{"message":"Your input exceeds the context window of this model. Please adjust your input and try again.","type":"invalid_request_error","param":"input","code":"context_length_exceeded"}}"#,
        ))
        .mount(&server)
        .await;
    let provider = OpenAiResponses::new(format!("{}/v1", server.uri()), None);
    let first = provider.stream(request()).next().await.unwrap();
    let error = first.unwrap_err();
    assert!(
        matches!(error, ProviderError::Http { status: 400, .. }),
        "{error:?}"
    );
    assert!(error.is_context_overflow());
}

// A connection that drops before `response.completed` is not a finished reply.
#[tokio::test]
async fn a_stream_cut_before_completion_is_a_network_error() {
    let server = MockServer::start().await;
    let cut: String = fixture("text.sse")
        .split("event: response.completed")
        .next()
        .unwrap()
        .to_string();
    Mock::given(method("POST"))
        .respond_with(sse(cut))
        .mount(&server)
        .await;
    let provider = OpenAiResponses::new(format!("{}/v1", server.uri()), None);
    let events: Vec<_> = provider.stream(request()).collect().await;
    assert!(
        matches!(events.last(), Some(Err(ProviderError::Network(_)))),
        "{events:?}"
    );
}

#[tokio::test]
async fn a_failed_response_ends_the_stream_with_its_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(sse(fixture("failed.sse")))
        .mount(&server)
        .await;
    let provider = OpenAiResponses::new(format!("{}/v1", server.uri()), None);
    let events: Vec<_> = provider.stream(request()).collect().await;
    assert!(
        matches!(events.last(), Some(Err(e)) if e.is_context_overflow()),
        "{events:?}"
    );
}

// Review A M1: reasoning summaries are asked for by default now, but OpenAI refuses them to an
// organization it has not verified. The request is then sent again without them, and later
// requests do not ask.
#[tokio::test]
async fn summaries_an_organization_may_not_have_are_dropped() {
    let server = MockServer::start().await;
    let refusal = json!({"error": {
        "message": "Your organization must be verified to generate reasoning summaries. Please go to: https://platform.openai.com/settings/organization/general and click on Verify Organization.",
        "type": "invalid_request_error", "param": "reasoning.summary", "code": "unsupported_value"}});
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"reasoning": {"summary": "auto"}})))
        .respond_with(ResponseTemplate::new(400).set_body_json(refusal))
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(sse(fixture("text.sse")))
        .with_priority(2)
        .mount(&server)
        .await;
    let provider = OpenAiResponses::new(format!("{}/v1", server.uri()), Some("sk-test".into()));
    for _ in 0..2 {
        let events: Vec<_> = provider.stream(request()).collect().await;
        assert_eq!(
            events.last(),
            Some(&Ok(ProviderEvent::Finished(FinishReason::Stop))),
            "{events:?}"
        );
    }
    let bodies: Vec<serde_json::Value> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| r.body_json().unwrap())
        .collect();
    assert_eq!(
        bodies.len(),
        3,
        "the first request is refused, and only once"
    );
    assert_eq!(bodies[0]["reasoning"], json!({"summary": "auto"}));
    assert!(bodies[1].get("reasoning").is_none(), "{}", bodies[1]);
    assert!(bodies[2].get("reasoning").is_none(), "{}", bodies[2]);

    // Any other 400 is the error it was.
    let other = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": {
            "message": "Invalid value", "type": "invalid_request_error", "param": "input"}})))
        .mount(&other)
        .await;
    let provider = OpenAiResponses::new(format!("{}/v1", other.uri()), None);
    let events: Vec<_> = provider.stream(request()).collect().await;
    assert!(
        matches!(
            events.last(),
            Some(Err(ProviderError::Http { status: 400, .. }))
        ),
        "{events:?}"
    );
    assert_eq!(other.received_requests().await.unwrap().len(), 1);
}
