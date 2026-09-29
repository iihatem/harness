//! The Responses stream parser and request body, against fixtures built from the documented
//! streaming examples (`tests/fixtures/openai-responses/`).

use harness_core::message::{ChatRequest, Message, RequestOptions, ToolCall, ToolSpec, Usage};
use harness_core::provider::{FinishReason, ProviderError, ProviderEvent};
use harness_providers::openai_responses::{ResponsesStreamParser, request_body};
use serde_json::json;

/// The `data:` payloads of a fixture, in order.
fn payloads(name: &str) -> Vec<String> {
    let path = format!(
        "{}/tests/fixtures/openai-responses/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter_map(|line| line.strip_prefix("data: ").map(String::from))
        .collect()
}

/// Feeds a fixture to a parser, as the provider does: until the parser is done, then whatever
/// `finish` still has.
fn parse(name: &str) -> Result<Vec<ProviderEvent>, ProviderError> {
    let mut parser = ResponsesStreamParser::default();
    let mut events = Vec::new();
    for data in payloads(name) {
        events.extend(parser.push(&data)?);
        if parser.is_done() {
            break;
        }
    }
    events.extend(parser.finish());
    Ok(events)
}

#[test]
fn text_arrives_as_deltas_with_usage_and_a_stop() {
    assert_eq!(
        parse("text.sse").unwrap(),
        vec![
            ProviderEvent::TextDelta("Hello".into()),
            ProviderEvent::TextDelta(", world".into()),
            ProviderEvent::Usage(Usage {
                input_tokens: 36,
                output_tokens: 87,
                cached_tokens: 12,
            }),
            ProviderEvent::Finished(FinishReason::Stop),
        ]
    );
}

#[test]
fn function_calls_are_emitted_whole_in_output_order() {
    let events = parse("tool_call.sse").unwrap();
    assert_eq!(
        events[1..],
        [
            ProviderEvent::ToolCall(ToolCall {
                id: "call_abc".into(),
                name: "read".into(),
                arguments: r#"{"path":"src/lib.rs"}"#.into(),
            }),
            ProviderEvent::ToolCall(ToolCall {
                id: "call_def".into(),
                name: "glob".into(),
                arguments: r#"{"pattern":"*.md"}"#.into(),
            }),
            ProviderEvent::Finished(FinishReason::ToolCalls),
        ]
    );
    assert!(matches!(events[0], ProviderEvent::Usage(_)));
}

#[test]
fn reasoning_summaries_are_reasoning_deltas() {
    let events = parse("reasoning.sse").unwrap();
    assert_eq!(
        events[..2],
        [
            ProviderEvent::ReasoningDelta("**Checking the tests**".into()),
            ProviderEvent::TextDelta("All green.".into()),
        ]
    );
    assert_eq!(
        events.last(),
        Some(&ProviderEvent::Finished(FinishReason::Stop))
    );
}

#[test]
fn a_reply_stopped_by_the_output_limit_finishes_with_length() {
    let events = parse("incomplete.sse").unwrap();
    // The cut-off call is still reported, whole as far as it goes: the agent decides what to do.
    assert!(events.contains(&ProviderEvent::ToolCall(ToolCall {
        id: "call_cut".into(),
        name: "write".into(),
        arguments: r#"{"path":"big.txt","content":"aaaa"#.into(),
    })));
    assert_eq!(
        events.last(),
        Some(&ProviderEvent::Finished(FinishReason::Length))
    );
}

#[test]
fn a_failed_response_is_an_error_that_names_its_code() {
    let error = parse("failed.sse").unwrap_err();
    assert!(matches!(error, ProviderError::InStream(_)), "{error:?}");
    assert!(
        error.to_string().contains("context_length_exceeded"),
        "{error}"
    );
    assert!(error.is_context_overflow());
}

#[test]
fn server_errors_and_rate_limits_in_the_stream_can_be_retried() {
    for (code, status) in [("server_error", 500), ("rate_limit_exceeded", 429)] {
        let mut parser = ResponsesStreamParser::default();
        let data = json!({"type": "response.failed", "response": {"status": "failed",
            "error": {"code": code, "message": "try again"}}});
        let error = parser.push(&data.to_string()).unwrap_err();
        assert!(
            matches!(error, ProviderError::Http { status: s, .. } if s == status),
            "{error:?}"
        );
        assert!(error.is_retryable());
    }
    let mut parser = ResponsesStreamParser::default();
    let data = json!({"type": "error", "code": "invalid_prompt", "message": "bad prompt"});
    let error = parser.push(&data.to_string()).unwrap_err();
    assert_eq!(
        error,
        ProviderError::InStream("invalid_prompt: bad prompt".into())
    );
}

#[test]
fn unparseable_events_are_protocol_errors() {
    let mut parser = ResponsesStreamParser::default();
    assert!(matches!(
        parser.push("{not json"),
        Err(ProviderError::Protocol(_))
    ));
}

#[test]
fn finish_is_idempotent() {
    let mut parser = ResponsesStreamParser::default();
    for data in payloads("text.sse") {
        parser.push(&data).unwrap();
    }
    assert!(parser.is_done());
    assert!(parser.finish().is_empty());
}

fn conversation() -> ChatRequest {
    ChatRequest {
        model: "gpt-5".into(),
        system: "be brief".into(),
        messages: vec![
            Message::User {
                content: "hi".into(),
            },
            Message::Assistant {
                content: "Looking.".into(),
                tool_calls: vec![ToolCall {
                    id: "call_1".into(),
                    name: "read".into(),
                    arguments: r#"{"path":"a"}"#.into(),
                }],
                model: "openai/gpt-5".into(),
            },
            Message::Tool {
                call_id: "call_1".into(),
                content: "data".into(),
                is_error: false,
            },
        ],
        tools: vec![ToolSpec {
            name: "read".into(),
            description: "Read".into(),
            parameters: json!({"type": "object"}),
        }],
        options: RequestOptions::default(),
    }
}

#[test]
fn the_request_carries_the_whole_conversation_without_server_state() {
    let body = request_body(&conversation());
    assert_eq!(body["model"], "gpt-5");
    assert_eq!(body["instructions"], "be brief");
    assert_eq!(body["stream"], true);
    assert_eq!(body["store"], false);
    assert_eq!(
        body["input"],
        json!([
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Looking."}]},
            {"type": "function_call", "call_id": "call_1", "name": "read", "arguments": "{\"path\":\"a\"}"},
            {"type": "function_call_output", "call_id": "call_1", "output": "data"},
        ])
    );
    // Strict schemas would require every property: harness's tools have optional ones.
    assert_eq!(
        body["tools"],
        json!([{"type": "function", "name": "read", "description": "Read",
            "parameters": {"type": "object"}, "strict": false}])
    );
    assert_eq!(body["tool_choice"], "auto");
    for absent in [
        "max_output_tokens",
        "temperature",
        "reasoning",
        "previous_response_id",
    ] {
        assert!(body.get(absent).is_none(), "{absent} should be absent");
    }
}

#[test]
fn profile_options_reach_the_request() {
    let mut request = conversation();
    request.options = RequestOptions {
        max_output_tokens: Some(4096),
        temperature: Some(0.2),
        reasoning_effort: Some("high".into()),
    };
    let body = request_body(&request);
    assert_eq!(body["max_output_tokens"], 4096);
    assert_eq!(body["temperature"], 0.2);
    assert_eq!(
        body["reasoning"],
        json!({"effort": "high", "summary": "auto"})
    );
}

#[test]
fn an_assistant_message_with_only_tool_calls_sends_no_empty_text() {
    let mut request = conversation();
    if let Message::Assistant { content, .. } = &mut request.messages[1] {
        content.clear();
    }
    let body = request_body(&request);
    assert_eq!(body["input"][1]["type"], "function_call");
    assert!(request_body(&ChatRequest::default()).get("tools").is_none());
}
