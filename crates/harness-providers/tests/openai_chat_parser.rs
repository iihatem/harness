use harness_core::message::{ChatRequest, Message, ToolCall, ToolSpec, Usage};
use harness_core::provider::{FinishReason, ProviderError, ProviderEvent};
use harness_providers::openai_chat::{ChatStreamParser, request_body};
use serde_json::json;

fn parse(chunks: &[&str]) -> Vec<ProviderEvent> {
    let mut parser = ChatStreamParser::default();
    let mut events = Vec::new();
    for chunk in chunks {
        events.extend(parser.push(chunk).unwrap());
    }
    events.extend(parser.finish());
    events
}

#[test]
fn text_usage_and_finish() {
    let events = parse(&[
        r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":"Hel"},"finish_reason":null}]}"#,
        r#"{"choices":[{"index":0,"delta":{"content":"lo"},"finish_reason":"stop"}]}"#,
        r#"{"choices":[],"usage":{"prompt_tokens":12,"completion_tokens":2,"prompt_tokens_details":{"cached_tokens":8}}}"#,
        "[DONE]",
    ]);
    assert_eq!(
        events,
        vec![
            ProviderEvent::TextDelta("Hel".into()),
            ProviderEvent::TextDelta("lo".into()),
            ProviderEvent::Usage(Usage {
                input_tokens: 12,
                output_tokens: 2,
                cached_tokens: 8
            }),
            ProviderEvent::Finished(FinishReason::Stop),
        ]
    );
}

#[test]
fn tool_call_fragments_are_assembled() {
    let events = parse(&[
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_a","type":"function","function":{"name":"read","arguments":""}}]}}]}"#,
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":"}}]}}]}"#,
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a.txt\"}"}}]}}]}"#,
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        "[DONE]",
    ]);
    assert_eq!(
        events,
        vec![
            ProviderEvent::ToolCall(ToolCall {
                id: "call_a".into(),
                name: "read".into(),
                arguments: r#"{"path":"a.txt"}"#.into()
            }),
            ProviderEvent::Finished(FinishReason::ToolCalls),
        ]
    );
}

// Review Focus: servers that omit tool-call ids or indexes.
#[test]
fn missing_ids_and_indexes_still_yield_distinct_calls() {
    let events = parse(&[
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"function":{"name":"read","arguments":{"path":"a"}}}]}}]}"#,
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"function":{"name":"glob","arguments":"{\"pattern\":\"*\"}"}}]}}]}"#,
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
    ]);
    assert_eq!(
        events,
        vec![
            ProviderEvent::ToolCall(ToolCall {
                id: "call_0".into(),
                name: "read".into(),
                arguments: r#"{"path":"a"}"#.into()
            }),
            ProviderEvent::ToolCall(ToolCall {
                id: "call_1".into(),
                name: "glob".into(),
                arguments: r#"{"pattern":"*"}"#.into()
            }),
            ProviderEvent::Finished(FinishReason::ToolCalls),
        ]
    );
}

#[test]
fn empty_arguments_become_an_empty_object() {
    let events = parse(&[
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"glob"}}]},"finish_reason":"tool_calls"}]}"#,
    ]);
    assert_eq!(
        events[0],
        ProviderEvent::ToolCall(ToolCall {
            id: "c".into(),
            name: "glob".into(),
            arguments: "{}".into()
        })
    );
}

// Review Focus: an empty-string tool-call id must be treated like a missing one, not a real id
// that happens to be "" — otherwise it both keeps an empty id downstream and can be mistaken for
// the start of a brand-new call on a continuation fragment that has no `index`.
#[test]
fn empty_string_id_falls_back_and_does_not_start_a_new_call() {
    let events = parse(&[
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"id":"","function":{"name":"read","arguments":""}}]}}]}"#,
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"id":"","function":{"arguments":"{}"}}]}}]}"#,
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
    ]);
    assert_eq!(
        events,
        vec![
            ProviderEvent::ToolCall(ToolCall {
                id: "call_0".into(),
                name: "read".into(),
                arguments: "{}".into(),
            }),
            ProviderEvent::Finished(FinishReason::ToolCalls),
        ]
    );
}

#[test]
fn reasoning_fields_become_reasoning_deltas() {
    let events = parse(&[
        r#"{"choices":[{"index":0,"delta":{"reasoning_content":"think"}}]}"#,
        r#"{"choices":[{"index":0,"delta":{"reasoning":"more"},"finish_reason":"length"}]}"#,
    ]);
    assert_eq!(
        events,
        vec![
            ProviderEvent::ReasoningDelta("think".into()),
            ProviderEvent::ReasoningDelta("more".into()),
            ProviderEvent::Finished(FinishReason::Length),
        ]
    );
}

#[test]
fn invalid_json_is_a_protocol_error_and_an_error_payload_the_providers_own() {
    let mut parser = ChatStreamParser::default();
    assert!(matches!(
        parser.push("{not json"),
        Err(ProviderError::Protocol(_))
    ));
    let error = parser
        .push(r#"{"error":{"message":"model not found"}}"#)
        .unwrap_err();
    assert!(matches!(error, ProviderError::InStream(_)), "{error:?}");
    assert_eq!(
        error.to_string(),
        r#"provider error: {"message":"model not found"}"#
    );
}

#[test]
fn finish_is_idempotent() {
    let mut parser = ChatStreamParser::default();
    parser.push("[DONE]").unwrap();
    assert!(parser.is_done());
    assert!(parser.finish().is_empty());
}

#[test]
fn request_body_maps_messages_and_tools() {
    let req = ChatRequest {
        model: "qwen3:14b".into(),
        system: "be brief".into(),
        messages: vec![
            Message::User {
                content: "hi".into(),
            },
            Message::Assistant {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "read".into(),
                    arguments: "{}".into(),
                }],
                model: "ollama/qwen3:14b".into(),
            },
            Message::Tool {
                call_id: "c1".into(),
                content: "data".into(),
                is_error: false,
            },
        ],
        tools: vec![ToolSpec {
            name: "read".into(),
            description: "Read".into(),
            parameters: json!({"type": "object"}),
        }],
        ..ChatRequest::default()
    };
    let body = request_body(&req);
    assert_eq!(body["model"], "qwen3:14b");
    assert_eq!(body["stream"], true);
    assert_eq!(body["stream_options"]["include_usage"], true);
    assert_eq!(
        body["messages"][0],
        json!({"role": "system", "content": "be brief"})
    );
    assert_eq!(
        body["messages"][1],
        json!({"role": "user", "content": "hi"})
    );
    assert_eq!(body["messages"][2]["content"], serde_json::Value::Null);
    assert_eq!(
        body["messages"][2]["tool_calls"][0]["function"]["name"],
        "read"
    );
    assert_eq!(
        body["messages"][3],
        json!({"role": "tool", "tool_call_id": "c1", "content": "data"})
    );
    assert_eq!(body["tools"][0]["type"], "function");
    assert_eq!(body["tools"][0]["function"]["name"], "read");
}

#[test]
fn request_body_omits_empty_tools() {
    let req = ChatRequest {
        model: "m".into(),
        system: String::new(),
        messages: vec![],
        tools: vec![],
        ..ChatRequest::default()
    };
    assert!(request_body(&req).get("tools").is_none());
}

#[test]
fn profile_options_reach_the_chat_request() {
    use harness_core::message::RequestOptions;
    let plain = ChatRequest {
        model: "m".into(),
        ..ChatRequest::default()
    };
    let body = request_body(&plain);
    for absent in ["max_tokens", "temperature", "reasoning_effort"] {
        assert!(body.get(absent).is_none(), "{absent}");
    }
    let tuned = ChatRequest {
        options: RequestOptions {
            max_output_tokens: Some(2048),
            temperature: Some(0.2),
            reasoning_effort: Some("low".into()),
            ..RequestOptions::default()
        },
        ..plain
    };
    let body = request_body(&tuned);
    assert_eq!(body["max_tokens"], 2048);
    assert_eq!(body["temperature"], 0.2);
    assert_eq!(body["reasoning_effort"], "low");
}
