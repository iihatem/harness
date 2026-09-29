//! The Messages stream parser and request body, against fixtures built from the documented
//! streaming examples (`tests/fixtures/anthropic-messages/`).

use harness_core::message::{ChatRequest, Message, RequestOptions, ToolCall, ToolSpec, Usage};
use harness_core::provider::{FinishReason, ProviderError, ProviderEvent};
use harness_providers::anthropic_messages::{
    DEFAULT_MAX_TOKENS, MessagesStreamParser, request_body,
};
use serde_json::json;

fn payloads(name: &str) -> Vec<String> {
    let path = format!(
        "{}/tests/fixtures/anthropic-messages/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter_map(|line| line.strip_prefix("data: ").map(String::from))
        .collect()
}

fn parse(name: &str) -> Result<Vec<ProviderEvent>, ProviderError> {
    let mut parser = MessagesStreamParser::default();
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
fn text_arrives_as_deltas_and_usage_counts_cached_input() {
    assert_eq!(
        parse("text.sse").unwrap(),
        vec![
            ProviderEvent::TextDelta("Hello".into()),
            ProviderEvent::TextDelta("!".into()),
            // Input is what was sent: uncached, written to the cache, and read from it.
            ProviderEvent::Usage(Usage {
                input_tokens: 2125,
                output_tokens: 15,
                cached_tokens: 2000,
            }),
            ProviderEvent::Finished(FinishReason::Stop),
        ]
    );
}

#[test]
fn tool_use_is_emitted_whole_after_the_text() {
    let events = parse("tool_use.sse").unwrap();
    assert_eq!(
        events[0],
        ProviderEvent::TextDelta("Let me read it.".into())
    );
    assert_eq!(
        events[2..],
        [
            ProviderEvent::ToolCall(ToolCall {
                id: "toolu_01T1x1fJ34qAmk2tNTrN7Up6".into(),
                name: "read".into(),
                arguments: r#"{"path": "src/lib.rs"}"#.into(),
            }),
            ProviderEvent::Finished(FinishReason::ToolCalls),
        ]
    );
    assert!(matches!(events[1], ProviderEvent::Usage(_)));
}

#[test]
fn a_reply_stopped_by_max_tokens_finishes_with_length() {
    let events = parse("max_tokens.sse").unwrap();
    assert_eq!(
        events[0],
        ProviderEvent::ReasoningDelta("The file is large.".into())
    );
    assert!(events.contains(&ProviderEvent::ToolCall(ToolCall {
        id: "toolu_cut".into(),
        name: "write".into(),
        arguments: r#"{"path": "big.txt", "content": "aaaa"#.into(),
    })));
    assert_eq!(
        events.last(),
        Some(&ProviderEvent::Finished(FinishReason::Length))
    );
}

#[test]
fn an_overloaded_error_in_the_stream_can_be_retried() {
    let error = parse("overloaded.sse").unwrap_err();
    assert!(
        matches!(error, ProviderError::Http { status: 529, .. }),
        "{error:?}"
    );
    assert!(error.is_retryable());
}

#[test]
fn other_stream_errors_name_their_type() {
    let mut parser = MessagesStreamParser::default();
    let data = json!({"type": "error", "error": {"type": "invalid_request_error",
        "message": "prompt is too long: 208000 tokens > 200000 maximum"}});
    let error = parser.push(&data.to_string()).unwrap_err();
    assert_eq!(
        error,
        ProviderError::InStream(
            "invalid_request_error: prompt is too long: 208000 tokens > 200000 maximum".into()
        )
    );
    assert!(error.is_context_overflow());
}

#[test]
fn finish_is_idempotent() {
    let mut parser = MessagesStreamParser::default();
    for data in payloads("text.sse") {
        parser.push(&data).unwrap();
    }
    assert!(parser.is_done());
    assert!(parser.finish().is_empty());
}

fn call(id: &str, arguments: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: "read".into(),
        arguments: arguments.into(),
    }
}

fn request(messages: Vec<Message>) -> ChatRequest {
    ChatRequest {
        model: "claude-sonnet-4-5".into(),
        system: "be brief".into(),
        messages,
        tools: vec![ToolSpec {
            name: "read".into(),
            description: "Read".into(),
            parameters: json!({"type": "object"}),
        }],
        options: RequestOptions::default(),
    }
}

#[test]
fn the_request_uses_the_messages_shapes_and_marks_cache_breakpoints() {
    let body = request_body(&request(vec![Message::User {
        content: "hi".into(),
    }]));
    assert_eq!(body["model"], "claude-sonnet-4-5");
    assert_eq!(body["stream"], true);
    assert_eq!(body["max_tokens"], DEFAULT_MAX_TOKENS);
    assert_eq!(
        body["system"],
        json!([{"type": "text", "text": "be brief", "cache_control": {"type": "ephemeral"}}])
    );
    assert_eq!(
        body["messages"],
        json!([{"role": "user", "content": [
            {"type": "text", "text": "hi", "cache_control": {"type": "ephemeral"}}
        ]}])
    );
    assert_eq!(
        body["tools"],
        json!([{"name": "read", "description": "Read", "input_schema": {"type": "object"}}])
    );
    assert!(body.get("temperature").is_none());
    assert!(body.get("thinking").is_none());
}

#[test]
fn tool_results_and_the_next_prompt_share_one_user_message() {
    let body = request_body(&request(vec![
        Message::User {
            content: "read a and b".into(),
        },
        Message::Assistant {
            content: String::new(),
            tool_calls: vec![call("t1", r#"{"path":"a"}"#), call("t2", "{not json")],
            model: "anthropic/claude-sonnet-4-5".into(),
        },
        Message::Tool {
            call_id: "t1".into(),
            content: "A".into(),
            is_error: false,
        },
        Message::Tool {
            call_id: "t2".into(),
            content: "arguments for `read` are not valid JSON".into(),
            is_error: true,
        },
        Message::User {
            content: "[harness] The approval mode is now ask".into(),
        },
    ]));
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3, "{messages:?}");
    assert_eq!(
        messages[1],
        json!({"role": "assistant", "content": [
            {"type": "tool_use", "id": "t1", "name": "read", "input": {"path": "a"}},
            // Anthropic takes only objects: an invalid call is sent with no arguments.
            {"type": "tool_use", "id": "t2", "name": "read", "input": {}},
        ]})
    );
    assert_eq!(
        messages[2],
        json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": "A"},
            {"type": "tool_result", "tool_use_id": "t2",
                "content": "arguments for `read` are not valid JSON", "is_error": true},
            {"type": "text", "text": "[harness] The approval mode is now ask",
                "cache_control": {"type": "ephemeral"}},
        ]})
    );
}

// The API rejects empty text blocks and messages without content.
#[test]
fn empty_assistant_messages_and_empty_text_are_left_out() {
    let body = request_body(&request(vec![
        Message::User {
            content: "one".into(),
        },
        Message::Assistant {
            content: String::new(),
            tool_calls: vec![],
            model: "anthropic/claude-sonnet-4-5".into(),
        },
        Message::User {
            content: "two".into(),
        },
    ]));
    assert_eq!(
        body["messages"],
        json!([{"role": "user", "content": [
            {"type": "text", "text": "one"},
            {"type": "text", "text": "two", "cache_control": {"type": "ephemeral"}},
        ]}])
    );
    let mut no_system = request(vec![Message::User {
        content: "hi".into(),
    }]);
    no_system.system.clear();
    assert!(request_body(&no_system).get("system").is_none());
}

#[test]
fn profile_options_reach_the_request() {
    let mut req = request(vec![Message::User {
        content: "hi".into(),
    }]);
    req.options = RequestOptions {
        max_output_tokens: Some(4096),
        temperature: Some(0.2),
        reasoning_effort: Some("high".into()),
    };
    let body = request_body(&req);
    assert_eq!(body["max_tokens"], 4096);
    assert_eq!(body["temperature"], 0.2);
    // Extended thinking is not requested in M1.
    assert!(body.get("thinking").is_none());
}
