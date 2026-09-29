//! The Messages stream parser and request body, against fixtures built from the documented
//! streaming examples (`tests/fixtures/anthropic-messages/`).

use harness_core::message::{ChatRequest, Message, RequestOptions, ToolCall, ToolSpec, Usage};
use harness_core::provider::{FinishReason, ProviderError, ProviderEvent};
use harness_providers::anthropic_messages::{
    DEFAULT_MAX_TOKENS, MIN_MAX_TOKENS, MessagesStreamParser, request_body,
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

/// The events a parser yields for these payloads, then what `finish` still has.
fn events_of(payloads: &[serde_json::Value]) -> Vec<ProviderEvent> {
    let mut parser = MessagesStreamParser::default();
    let mut events: Vec<ProviderEvent> = payloads
        .iter()
        .flat_map(|p| parser.push(&p.to_string()).unwrap())
        .collect();
    events.extend(parser.finish());
    events
}

fn start(usage: Option<serde_json::Value>) -> serde_json::Value {
    let mut message = json!({"id": "msg_1", "type": "message", "role": "assistant",
        "content": [], "model": "claude", "stop_reason": null});
    if let Some(usage) = usage {
        message["usage"] = usage;
    }
    json!({"type": "message_start", "message": message})
}

fn stop_with(reason: &str, usage: serde_json::Value) -> serde_json::Value {
    json!({"type": "message_delta", "delta": {"stop_reason": reason}, "usage": usage})
}

fn usage_of(events: &[ProviderEvent]) -> Vec<Usage> {
    events
        .iter()
        .filter_map(|e| match e {
            ProviderEvent::Usage(usage) => Some(*usage),
            _ => None,
        })
        .collect()
}

// Review A M3: a compatible server that gives no usage at the start reports none, not a request
// of 0 tokens, which would stop proactive compaction; counts a `message_delta` carries update
// what the start said.
#[test]
fn usage_is_taken_from_where_the_server_gives_it() {
    let stop = json!({"type": "message_stop"});
    let none = events_of(&[
        start(None),
        stop_with("end_turn", json!({"output_tokens": 5})),
        stop.clone(),
    ]);
    assert!(usage_of(&none).is_empty(), "{none:?}");
    let at_the_end = events_of(&[
        start(None),
        stop_with(
            "end_turn",
            json!({"input_tokens": 5000, "cache_read_input_tokens": 1000, "output_tokens": 20}),
        ),
        stop.clone(),
    ]);
    assert_eq!(
        usage_of(&at_the_end),
        [Usage {
            input_tokens: 6000,
            output_tokens: 20,
            cached_tokens: 1000,
        }]
    );
    let updated = events_of(&[
        start(Some(
            json!({"input_tokens": 25, "cache_creation_input_tokens": 100,
            "cache_read_input_tokens": 2000, "output_tokens": 1}),
        )),
        stop_with("end_turn", json!({"input_tokens": 30, "output_tokens": 15})),
        stop,
    ]);
    assert_eq!(
        usage_of(&updated),
        [Usage {
            input_tokens: 2130,
            output_tokens: 15,
            cached_tokens: 2000,
        }]
    );
}

// Review A M4: a compatible server can give a call's whole input in its start event, with no
// deltas; deltas, when they come, are the input.
#[test]
fn a_tool_uses_input_in_its_start_event_is_kept() {
    let tool = |input: serde_json::Value| {
        json!({"type": "content_block_start", "index": 0,
        "content_block": {"type": "tool_use", "id": "toolu_1", "name": "read", "input": input}})
    };
    let delta = |json: &str| {
        json!({"type": "content_block_delta", "index": 0,
        "delta": {"type": "input_json_delta", "partial_json": json}})
    };
    let stop = json!({"type": "message_stop"});
    let arguments = |events: Vec<ProviderEvent>| {
        events
            .into_iter()
            .find_map(|e| match e {
                ProviderEvent::ToolCall(call) => Some(call.arguments),
                _ => None,
            })
            .unwrap()
    };
    assert_eq!(
        arguments(events_of(&[
            tool(json!({"path": "src/lib.rs"})),
            stop.clone()
        ])),
        r#"{"path":"src/lib.rs"}"#
    );
    assert_eq!(
        arguments(events_of(&[
            tool(json!({"path": "old"})),
            delta(""),
            delta(r#"{"path": "new"}"#),
            stop.clone()
        ])),
        r#"{"path": "new"}"#
    );
    assert_eq!(arguments(events_of(&[tool(json!({})), stop])), "{}");
}

// Review A M5: a model that stops at the end of its window was cut off, as at `max_tokens`.
#[test]
fn stopping_at_the_context_window_is_a_cut_off() {
    let events = events_of(&[
        start(Some(json!({"input_tokens": 10, "output_tokens": 1}))),
        stop_with(
            "model_context_window_exceeded",
            json!({"output_tokens": 100}),
        ),
        json!({"type": "message_stop"}),
    ]);
    assert_eq!(
        events.last(),
        Some(&ProviderEvent::Finished(FinishReason::Length))
    );
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
        ..ChatRequest::default()
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

/// The blocks of `body` marked as prompt-cache breakpoints, as (message index, block index).
fn breakpoints(body: &serde_json::Value) -> Vec<(usize, usize)> {
    let mut marked = Vec::new();
    for (m, message) in body["messages"].as_array().unwrap().iter().enumerate() {
        for (b, block) in message["content"].as_array().unwrap().iter().enumerate() {
            if block.get("cache_control").is_some() {
                marked.push((m, b));
            }
        }
    }
    marked
}

// Review A M6: the cache is looked up only about 20 blocks back from a breakpoint, and a step
// with many parallel calls adds more. The previous request's last message is marked too, where
// that request wrote the cache, so it is found whatever came since.
#[test]
fn the_previous_requests_end_is_a_cache_breakpoint_too() {
    let calls: Vec<ToolCall> = (0..12)
        .map(|i| call(&format!("t{i}"), r#"{"path":"a"}"#))
        .collect();
    let mut messages = vec![
        Message::User {
            content: "first".into(),
        },
        Message::Assistant {
            content: "Done.".into(),
            tool_calls: vec![],
            model: "anthropic/claude-sonnet-4-5".into(),
        },
        Message::User {
            content: "read all twelve".into(),
        },
        Message::Assistant {
            content: "Reading.".into(),
            tool_calls: calls.clone(),
            model: "anthropic/claude-sonnet-4-5".into(),
        },
    ];
    messages.extend(calls.iter().map(|c| Message::Tool {
        call_id: c.id.clone(),
        content: "A".into(),
        is_error: false,
    }));
    let body = request_body(&request(messages));
    // The request before this one ended with "read all twelve".
    assert_eq!(breakpoints(&body), [(2, 0), (4, 11)]);
    assert_eq!(body["messages"][2]["content"][0]["text"], "read all twelve");
    // With the system prompt, three of the four breakpoints the API allows.
    assert!(body["system"][0].get("cache_control").is_some());
    // A first request has one message to mark.
    let first = request_body(&request(vec![Message::User {
        content: "hi".into(),
    }]));
    assert_eq!(breakpoints(&first), [(0, 0)]);
}

/// Whether a tool-use id fits the API's pattern, `^[a-zA-Z0-9_-]+$`.
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Checks `body` against the Messages API's rules for a conversation: it starts with a user
/// message and roles alternate; no message is empty; text blocks hold more than whitespace;
/// tool-use ids fit the pattern and are unique; and each tool use is answered, in the next
/// message, by a result with its id.
fn assert_valid(body: &serde_json::Value) {
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages[0]["role"], "user", "{body:#}");
    let mut seen = std::collections::HashSet::new();
    let mut unanswered: Vec<String> = Vec::new();
    for (i, message) in messages.iter().enumerate() {
        let role = message["role"].as_str().unwrap();
        if i > 0 {
            assert_ne!(messages[i - 1]["role"], role, "roles alternate: {body:#}");
        }
        let blocks = message["content"].as_array().unwrap();
        assert!(!blocks.is_empty(), "an empty message: {body:#}");
        let answered: Vec<&str> = blocks
            .iter()
            .filter_map(|b| b["tool_use_id"].as_str())
            .collect();
        assert_eq!(
            answered, unanswered,
            "each call is answered in the next message: {body:#}"
        );
        unanswered.clear();
        for block in blocks {
            match block["type"].as_str().unwrap() {
                "text" => assert!(
                    !block["text"].as_str().unwrap().trim().is_empty(),
                    "whitespace-only text: {body:#}"
                ),
                "tool_use" => {
                    let id = block["id"].as_str().unwrap();
                    assert!(valid_id(id), "{id}");
                    assert!(seen.insert(id.to_string()), "a repeated id: {id}");
                    unanswered.push(id.to_string());
                }
                "tool_result" => {
                    if let Some(content) = block.get("content") {
                        assert!(!content.as_str().unwrap().trim().is_empty(), "{block}");
                    }
                }
                other => panic!("unexpected block {other}"),
            }
        }
    }
    assert!(unanswered.is_empty(), "{body:#}");
}

// Review A I2: a conversation held on other providers carries what the Messages API rejects:
// text that is only whitespace (a local model's "\n\n" beside its calls), and tool-call ids with
// characters outside its pattern (Kimi K2 through OpenRouter writes `functions.read:0`). It is
// sent without them, so that it can go on with Claude.
#[test]
fn a_conversation_from_another_provider_is_sent_as_the_api_accepts_it() {
    let calls = vec![
        call("functions.read:0", r#"{"path":"a"}"#),
        call("functions.read:1", r#"{"path":"b"}"#),
        // These two differ only in characters the pattern leaves out.
        call("a.b", r#"{"path":"c"}"#),
        call("a:b", r#"{"path":"d"}"#),
        // Already fits: kept as it is.
        call("call_h3", r#"{"path":"e"}"#),
    ];
    let mut messages = vec![
        Message::User {
            content: "read them".into(),
        },
        Message::Assistant {
            content: "\n\n".into(),
            tool_calls: calls.clone(),
            model: "openrouter/moonshotai/kimi-k2".into(),
        },
    ];
    for c in &calls {
        messages.push(Message::Tool {
            call_id: c.id.clone(),
            content: if c.id == "a:b" {
                " \n".into()
            } else {
                "data".into()
            },
            is_error: false,
        });
    }
    messages.extend([
        Message::Assistant {
            content: " \n\t".into(),
            tool_calls: vec![],
            model: "ollama/qwen3".into(),
        },
        Message::User {
            content: "\n".into(),
        },
        Message::Assistant {
            content: "Read all five.".into(),
            tool_calls: vec![],
            model: "ollama/qwen3".into(),
        },
        Message::User {
            content: "thanks".into(),
        },
    ]);
    let body = request_body(&request(messages));
    assert_valid(&body);
    let sent = body["messages"].as_array().unwrap();
    assert_eq!(sent.len(), 5, "{body:#}");
    let ids: Vec<&str> = sent[1]["content"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids[4], "call_h3");
    assert!(ids[0].starts_with("functions_read_0_"), "{ids:?}");
    assert!(ids[2].starts_with("a_b_") && ids[3].starts_with("a_b_"));
    // An id maps the same way in every request, so the prompt cache still matches.
    let earlier = request_body(&request(vec![
        Message::User {
            content: "read them".into(),
        },
        Message::Assistant {
            content: String::new(),
            tool_calls: calls,
            model: "openrouter/moonshotai/kimi-k2".into(),
        },
    ]));
    assert_eq!(earlier["messages"][1]["content"][0]["id"], ids[0]);
    assert_eq!(earlier["messages"][1]["content"][3]["id"], ids[3]);
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
        ..RequestOptions::default()
    };
    let body = request_body(&req);
    assert_eq!(body["max_tokens"], 4096);
    assert_eq!(body["temperature"], 0.2);
    // Extended thinking is not requested in M1.
    assert!(body.get("thinking").is_none());
}

// Review A M5 (decision 4): input plus `max_tokens` must fit the window, or the API refuses the
// request. `max_tokens` is the smaller of the profile's limit (or the default) and the room the
// window has left, but never below a floor: with less room than that, a request that is refused
// as too long leads to compaction.
#[test]
fn max_tokens_fits_the_room_left_in_the_window() {
    let sent = |limit: Option<u64>, room: Option<u64>| {
        let mut req = request(vec![Message::User {
            content: "hi".into(),
        }]);
        req.options.max_output_tokens = limit;
        req.output_room = room;
        request_body(&req)["max_tokens"].as_u64().unwrap()
    };
    assert_eq!(sent(None, None), DEFAULT_MAX_TOKENS);
    assert_eq!(sent(None, Some(100_000)), DEFAULT_MAX_TOKENS);
    assert_eq!(sent(None, Some(9_000)), 9_000);
    assert_eq!(sent(None, Some(100)), MIN_MAX_TOKENS);
    assert_eq!(sent(Some(4_096), Some(100_000)), 4_096);
    assert_eq!(sent(Some(4_096), Some(2_000)), 2_000);
    assert_eq!(sent(Some(512), Some(100)), 512);
}
