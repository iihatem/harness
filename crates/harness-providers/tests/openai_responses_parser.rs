//! The Responses stream parser and request body, against fixtures built from the documented
//! streaming examples (`tests/fixtures/openai-responses/`).

use std::time::Duration;

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

/// The events a parser yields for these payloads.
fn events_of(payloads: &[serde_json::Value]) -> Vec<ProviderEvent> {
    let mut parser = ResponsesStreamParser::default();
    payloads
        .iter()
        .flat_map(|p| parser.push(&p.to_string()).unwrap())
        .collect()
}

// Review A M1: parts of a summary are separate paragraphs, as Codex shows them.
#[test]
fn summary_parts_are_separated() {
    let part = |index: u64| {
        json!({"type": "response.reasoning_summary_part.added",
        "item_id": "rs_1", "output_index": 0, "summary_index": index,
        "part": {"type": "summary_text", "text": ""}})
    };
    let delta = |index: u64, text: &str| {
        json!({"type": "response.reasoning_summary_text.delta",
        "item_id": "rs_1", "output_index": 0, "summary_index": index, "delta": text})
    };
    assert_eq!(
        events_of(&[
            part(0),
            delta(0, "**First**"),
            part(1),
            delta(1, "**Second**")
        ]),
        [
            ProviderEvent::ReasoningDelta("**First**".into()),
            ProviderEvent::ReasoningDelta("\n\n".into()),
            ProviderEvent::ReasoningDelta("**Second**".into()),
        ]
    );
}

// Review A M2: a refusal is what the model answered; it must reach the user.
#[test]
fn a_refusal_arrives_as_text() {
    let events = events_of(&[
        json!({"type": "response.refusal.delta", "item_id": "msg_1", "output_index": 0,
            "content_index": 0, "delta": "I can't help"}),
        json!({"type": "response.refusal.delta", "item_id": "msg_1", "output_index": 0,
            "content_index": 0, "delta": " with that."}),
        json!({"type": "response.refusal.done", "item_id": "msg_1", "output_index": 0,
            "content_index": 0, "refusal": "I can't help with that."}),
        json!({"type": "response.completed", "response": {"status": "completed"}}),
    ]);
    assert_eq!(
        events,
        [
            ProviderEvent::TextDelta("I can't help".into()),
            ProviderEvent::TextDelta(" with that.".into()),
            ProviderEvent::Finished(FinishReason::Stop),
        ]
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
            matches!(error, ProviderError::Reported { status: s, .. } if s == status),
            "{error:?}"
        );
        assert!(error.is_retryable());
        // Re-review A, N2: no HTTP status was received.
        assert!(!error.to_string().contains("HTTP"), "{error}");
    }
    let mut parser = ResponsesStreamParser::default();
    let data = json!({"type": "error", "code": "invalid_prompt", "message": "bad prompt"});
    let error = parser.push(&data.to_string()).unwrap_err();
    assert_eq!(
        error,
        ProviderError::InStream("invalid_prompt: bad prompt".into())
    );
}

/// The error a `response.failed` event with `error` ends the stream with.
fn failed(error: serde_json::Value) -> ProviderError {
    let mut parser = ResponsesStreamParser::default();
    let data = json!({"type": "response.failed", "response": {"status": "failed", "error": error}});
    parser.push(&data.to_string()).unwrap_err()
}

// Review A I1: OpenAI's overload and slow-down codes are transient, as Codex treats them.
#[test]
fn an_overloaded_server_or_a_slow_down_in_the_stream_is_retried() {
    for (code, status) in [("server_is_overloaded", 503), ("slow_down", 429)] {
        let error = failed(json!({"code": code, "message": "busy"}));
        assert_eq!(
            error,
            ProviderError::Reported {
                status,
                body: format!("{code}: busy"),
                retry_after: None,
            }
        );
        assert!(error.is_retryable(), "{code}");
    }
}

// Review A I1: an `error` event can carry its details under `error`, where Codex reads them.
#[test]
fn a_nested_error_event_keeps_its_code() {
    let mut parser = ResponsesStreamParser::default();
    let data = json!({"type": "error", "sequence_number": 3,
        "error": {"type": "server_error", "code": "server_is_overloaded", "message": "busy"}});
    let error = parser.push(&data.to_string()).unwrap_err();
    assert!(
        matches!(error, ProviderError::Reported { status: 503, .. }),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        "the provider reported an overload: server_is_overloaded: busy"
    );
    let mut parser = ResponsesStreamParser::default();
    let data = json!({"type": "error", "error": {"code": "context_length_exceeded",
        "message": "Your input exceeds the context window of this model."}});
    let error = parser.push(&data.to_string()).unwrap_err();
    assert!(error.is_context_overflow(), "{error:?}");
    assert!(!error.is_retryable());
}

// Review A I1: as in Codex, a code harness does not know is worth another try; the ones known to
// be final are not retried.
#[test]
fn unknown_codes_in_the_stream_are_retried_and_known_final_ones_are_not() {
    for error in [
        json!({"code": "something_new", "message": "try later"}),
        json!({"message": "An error occurred while processing your request."}),
        json!(null),
    ] {
        let error = failed(error);
        assert!(error.is_retryable(), "{error:?}");
    }
    for code in ["context_length_exceeded", "invalid_prompt"] {
        let error = failed(json!({"code": code, "message": "no"}));
        assert!(matches!(error, ProviderError::InStream(_)), "{error:?}");
        assert!(!error.is_retryable(), "{code}");
    }
}

// Review A I1: "try again in N s" in a rate limit's message is how long to wait, as Codex reads it.
#[test]
fn the_wait_a_stream_error_names_becomes_its_retry_after() {
    for (message, wait) in [
        (
            "Rate limit reached for gpt-5 in organization org-AAA on tokens per min (TPM): Limit 30000, Used 22999, Requested 12528. Please try again in 11.054s. Visit https://platform.openai.com/account/rate-limits to learn more.",
            Some(Duration::from_millis(11_054)),
        ),
        ("Please try again in 28ms.", Some(Duration::from_millis(28))),
        ("Try again in 35 seconds", Some(Duration::from_secs(35))),
        ("Rate limit reached.", None),
        ("try again in a moment", None),
    ] {
        let error = failed(json!({"code": "rate_limit_exceeded", "message": message}));
        assert_eq!(error.retry_after(), wait, "{message}");
        assert!(error.is_retryable());
    }
}

// Review A M11: an exhausted quota reported in the stream reads as one (decision 17): not retried,
// with its reset time.
#[test]
fn an_exhausted_quota_in_the_stream_is_reported_as_one() {
    for code in [
        "insufficient_quota",
        "usage_limit_reached",
        "usage_not_included",
        "credit_balance_exhausted",
        "organization_spend_limit_exceeded",
        "project_spend_limit_exceeded",
    ] {
        let error = failed(
            json!({"code": code, "message": "You exceeded your current quota",
            "resets_at": 1_900_000_000u64}),
        );
        assert!(error.is_quota_exhausted(), "{error:?}");
        assert!(!error.is_retryable(), "{code}");
        assert_eq!(error.resets_at(), Some(1_900_000_000));
    }
    let error = failed(json!({"code": "insufficient_quota", "message": "You exceeded your quota"}));
    assert!(error.is_quota_exhausted() && error.resets_at().is_none());
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
        ..ChatRequest::default()
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
    // Reasoning is asked for by model: see `reasoning_models_are_asked_for_summaries`.
    for absent in [
        "max_output_tokens",
        "temperature",
        "previous_response_id",
        "include",
    ] {
        assert!(body.get(absent).is_none(), "{absent} should be absent");
    }
}

// Review A M1 (decision 3): a reasoning model streams its summaries even when no profile sets a
// reasoning effort; the API's default effort is kept. Other models get no `reasoning`.
#[test]
fn reasoning_models_are_asked_for_summaries() {
    for (model, reasons) in [
        ("gpt-5", true),
        ("gpt-5-codex", true),
        ("gpt-5.1-mini", true),
        ("o3", true),
        ("o4-mini", true),
        ("o1-pro", true),
        ("gpt-5-chat-latest", false),
        ("gpt-4.1", false),
        ("gpt-4o-mini", false),
        ("omni-moderation-latest", false),
    ] {
        let mut request = conversation();
        request.model = model.into();
        let body = request_body(&request);
        if reasons {
            assert_eq!(body["reasoning"], json!({"summary": "auto"}), "{model}");
        } else {
            assert!(body.get("reasoning").is_none(), "{model}");
        }
    }
    // A profile's effort says the model reasons, whatever its name.
    let mut request = conversation();
    request.model = "my-reasoner".into();
    request.options.reasoning_effort = Some("low".into());
    assert_eq!(
        request_body(&request)["reasoning"],
        json!({"effort": "low", "summary": "auto"})
    );
}

#[test]
fn profile_options_reach_the_request() {
    let mut request = conversation();
    request.options = RequestOptions {
        max_output_tokens: Some(4096),
        temperature: Some(0.2),
        reasoning_effort: Some("high".into()),
        ..RequestOptions::default()
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
