//! The Anthropic Messages protocol (`POST /messages`, server-sent events), with an API key only:
//! Claude subscription credentials are never used.

use std::collections::BTreeMap;

use harness_core::{
    message::{ChatRequest, Message, ToolCall, Usage},
    provider::{FinishReason, Provider, ProviderError, ProviderEvent, ProviderStream},
};
use serde_json::{Map, Value, json};

use crate::sse::{self, EventParser};

/// The API version harness speaks.
pub const API_VERSION: &str = "2023-06-01";
/// `max_tokens` when the profile sets no output limit: the protocol requires one.
pub const DEFAULT_MAX_TOKENS: u64 = 16_384;
/// `max_tokens` is never fitted below this, however little room the window has left: a request
/// that does not fit then is refused as too long, which leads to compaction.
pub const MIN_MAX_TOKENS: u64 = 1_024;

/// Builds a streaming Messages request body. Consecutive messages of one role become one message:
/// tool results are user content here, so a note or prompt after them joins their message.
/// What the API rejects, and a conversation held on other providers can hold, is left out or
/// made to fit: text that is only whitespace, empty assistant messages, and tool-call ids with
/// characters outside its pattern (`tool_use_id`). The system prompt, the last message and the
/// previous request's last message are prompt-cache breakpoints; `max_tokens` is fitted to the
/// room the window has left.
pub fn request_body(req: &ChatRequest) -> Value {
    let mut messages: Vec<(&str, Vec<Value>)> = Vec::new();
    let mut push = |role: &'static str, blocks: Vec<Value>| {
        if blocks.is_empty() {
            return;
        }
        match messages.last_mut() {
            Some((last, content)) if *last == role => content.extend(blocks),
            _ => messages.push((role, blocks)),
        }
    };
    for message in &req.messages {
        match message {
            Message::User { content } => push("user", text_block(content)),
            Message::Assistant {
                content,
                tool_calls,
                ..
            } => {
                let mut blocks = text_block(content);
                for call in tool_calls {
                    // Only an object is accepted: a call whose arguments were not valid JSON is
                    // sent without them (its result says what was wrong).
                    let input = serde_json::from_str::<Value>(&call.arguments)
                        .ok()
                        .filter(Value::is_object)
                        .unwrap_or_else(|| Value::Object(Map::new()));
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": tool_use_id(&call.id),
                        "name": call.name,
                        "input": input,
                    }));
                }
                push("assistant", blocks);
            }
            Message::Tool {
                call_id,
                content,
                is_error,
            } => {
                let mut block = json!({"type": "tool_result", "tool_use_id": tool_use_id(call_id)});
                if !content.trim().is_empty() {
                    block["content"] = json!(content);
                }
                if *is_error {
                    block["is_error"] = json!(true);
                }
                push("user", vec![block]);
            }
        }
    }
    // The cache is looked up only about 20 blocks back from a breakpoint, and one step with many
    // calls adds more: the previous request's last message, where that request wrote the cache,
    // is marked too. With the system prompt, that is three of the four breakpoints allowed.
    let users: Vec<usize> = (0..messages.len())
        .filter(|&i| messages[i].0 == "user")
        .collect();
    let previous = users.iter().rev().nth(1).copied();
    for i in previous.into_iter().chain(messages.len().checked_sub(1)) {
        if let Some(block) = messages[i].1.last_mut() {
            block["cache_control"] = json!({"type": "ephemeral"});
        }
    }
    let messages: Vec<Value> = messages
        .into_iter()
        .map(|(role, content)| json!({"role": role, "content": content}))
        .collect();
    let mut body = json!({
        "model": req.model,
        "max_tokens": max_tokens(req),
        "messages": messages,
        "stream": true,
    });
    if !req.system.is_empty() {
        body["system"] = json!([{
            "type": "text",
            "text": req.system,
            "cache_control": {"type": "ephemeral"},
        }]);
    }
    if !req.tools.is_empty() {
        body["tools"] = Value::Array(
            req.tools
                .iter()
                .map(|t| {
                    json!({"name": t.name, "description": t.description, "input_schema": t.parameters})
                })
                .collect(),
        );
    }
    if let Some(temperature) = req.options.temperature {
        body["temperature"] = json!(temperature);
    }
    body
}

/// The output limit: the profile's or the default, within the room the window has left after the
/// input (input plus `max_tokens` must fit the window), but not below [`MIN_MAX_TOKENS`].
fn max_tokens(req: &ChatRequest) -> u64 {
    let limit = req.options.max_output_tokens.unwrap_or(DEFAULT_MAX_TOKENS);
    req.output_room
        .map_or(limit, |room| limit.min(room.max(MIN_MAX_TOKENS)))
}

/// A text block, or none for text that is empty or only whitespace, which the API rejects.
fn text_block(text: &str) -> Vec<Value> {
    if text.trim().is_empty() {
        Vec::new()
    } else {
        vec![json!({"type": "text", "text": text})]
    }
}

/// `id` as the API accepts tool-use ids (`^[a-zA-Z0-9_-]+$`): each other character becomes `_`,
/// and an id that changed gets a short hash of the original appended, so that two ids never
/// become one. The same id always maps the same way, so a call and its result still match, and so
/// does the prompt cache from one request to the next. Ids from Anthropic and harness fit already.
fn tool_use_id(id: &str) -> String {
    let fits = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    if !id.is_empty() && id.chars().all(fits) {
        return id.to_string();
    }
    let kept: String = id.chars().map(|c| if fits(c) { c } else { '_' }).collect();
    // FNV-1a, which is stable across builds and platforms.
    let hash = id.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    });
    format!("{kept}_{:08x}", (hash >> 32) as u32 ^ hash as u32)
}

#[derive(Debug, Default)]
struct PartialCall {
    id: String,
    name: String,
    /// The input its start event gave, which some compatible servers send whole.
    input: Option<String>,
    arguments: String,
}

/// The token counts a stream reported, as the server gave them: in `message_start`, and updated
/// by `message_delta`.
#[derive(Debug, Default)]
struct Counts {
    input: Option<u64>,
    cache_writes: u64,
    cache_reads: u64,
    output: u64,
}

impl Counts {
    fn update(&mut self, usage: &Value) {
        let count = |key: &str| usage[key].as_u64();
        self.input = count("input_tokens").or(self.input);
        self.cache_writes = count("cache_creation_input_tokens").unwrap_or(self.cache_writes);
        self.cache_reads = count("cache_read_input_tokens").unwrap_or(self.cache_reads);
        self.output = count("output_tokens").unwrap_or(self.output);
    }

    /// The usage, when the server said how much was sent. Input is everything sent: uncached,
    /// written to the cache, and read from it.
    fn usage(&self) -> Option<Usage> {
        Some(Usage {
            input_tokens: self.input? + self.cache_writes + self.cache_reads,
            output_tokens: self.output,
            cached_tokens: self.cache_reads,
        })
    }
}

/// Turns Messages stream events into [`ProviderEvent`]s. Tool calls are buffered by content block
/// and emitted whole, with the usage, when the message stops.
#[derive(Debug, Default)]
pub struct MessagesStreamParser {
    calls: BTreeMap<u64, PartialCall>,
    counts: Counts,
    finish: Option<FinishReason>,
    done: bool,
}

impl MessagesStreamParser {
    pub fn push(&mut self, data: &str) -> Result<Vec<ProviderEvent>, ProviderError> {
        let event: Value = serde_json::from_str(data)
            .map_err(|e| ProviderError::Protocol(format!("{e} in event: {data}")))?;
        let mut out = Vec::new();
        match event["type"].as_str().unwrap_or_default() {
            "message_start" => self.counts.update(&event["message"]["usage"]),
            "content_block_start" => {
                let block = &event["content_block"];
                if block["type"] == "tool_use" {
                    let index = event["index"].as_u64().unwrap_or(0);
                    let call = self.calls.entry(index).or_default();
                    call.id = block["id"].as_str().unwrap_or_default().to_string();
                    call.name = block["name"].as_str().unwrap_or_default().to_string();
                    call.input = block
                        .get("input")
                        .filter(|input| input.as_object().is_some_and(|o| !o.is_empty()))
                        .map(Value::to_string);
                }
            }
            "content_block_delta" => {
                let delta = &event["delta"];
                match delta["type"].as_str().unwrap_or_default() {
                    "text_delta" => {
                        if let Some(text) = delta["text"].as_str().filter(|t| !t.is_empty()) {
                            out.push(ProviderEvent::TextDelta(text.to_string()));
                        }
                    }
                    "thinking_delta" => {
                        if let Some(text) = delta["thinking"].as_str().filter(|t| !t.is_empty()) {
                            out.push(ProviderEvent::ReasoningDelta(text.to_string()));
                        }
                    }
                    "input_json_delta" => {
                        let index = event["index"].as_u64().unwrap_or(0);
                        if let Some(fragment) = delta["partial_json"].as_str() {
                            self.calls
                                .entry(index)
                                .or_default()
                                .arguments
                                .push_str(fragment);
                        }
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(reason) = event["delta"]["stop_reason"].as_str() {
                    self.finish = Some(match reason {
                        "end_turn" | "stop_sequence" => FinishReason::Stop,
                        "tool_use" => FinishReason::ToolCalls,
                        // Stopped at the end of the context window: cut off, as at its limit.
                        "max_tokens" | "model_context_window_exceeded" => FinishReason::Length,
                        other => FinishReason::Other(other.to_string()),
                    });
                }
                self.counts.update(&event["usage"]);
            }
            "message_stop" => out.extend(self.finish()),
            "error" => return Err(stream_error(&event["error"])),
            _ => {}
        }
        Ok(out)
    }

    /// Emits the usage, buffered calls, and `Finished`. Calling it again yields nothing.
    pub fn finish(&mut self) -> Vec<ProviderEvent> {
        if self.done {
            return Vec::new();
        }
        self.done = true;
        let mut out: Vec<ProviderEvent> = self
            .counts
            .usage()
            .map(ProviderEvent::Usage)
            .into_iter()
            .collect();
        // Deltas, when they came, are the input; else what the start event gave, if anything.
        out.extend(std::mem::take(&mut self.calls).into_values().map(|call| {
            ProviderEvent::ToolCall(ToolCall {
                id: call.id,
                name: call.name,
                arguments: if !call.arguments.trim().is_empty() {
                    call.arguments
                } else {
                    call.input.unwrap_or_else(|| "{}".into())
                },
            })
        }));
        out.push(ProviderEvent::Finished(
            self.finish.take().unwrap_or(FinishReason::Stop),
        ));
        out
    }

    pub fn is_done(&self) -> bool {
        self.done
    }
}

impl EventParser for MessagesStreamParser {
    fn push(&mut self, data: &str) -> Result<Vec<ProviderEvent>, ProviderError> {
        MessagesStreamParser::push(self, data)
    }

    fn finish(&mut self) -> Vec<ProviderEvent> {
        MessagesStreamParser::finish(self)
    }

    fn is_done(&self) -> bool {
        MessagesStreamParser::is_done(self)
    }
}

/// The error an `error` event reports. Overload, API and rate-limit errors are retried as their
/// HTTP forms (529, 500, 429) are.
fn stream_error(error: &Value) -> ProviderError {
    let kind = error["type"].as_str().unwrap_or_default();
    let message = error["message"].as_str().unwrap_or_default();
    let text = if kind.is_empty() {
        error.to_string()
    } else {
        format!("{kind}: {message}")
    };
    let status = match kind {
        "overloaded_error" => 529,
        "api_error" => 500,
        "rate_limit_error" => 429,
        _ => return ProviderError::InStream(text),
    };
    ProviderError::Reported {
        status,
        body: text,
        retry_after: None,
    }
}

/// A provider speaking the Messages protocol with an Anthropic API key.
pub struct AnthropicMessages {
    client: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
}

impl AnthropicMessages {
    pub fn new(base_url: impl Into<String>, api_key: Option<String>) -> Self {
        AnthropicMessages {
            client: crate::http::client()
                .build()
                .expect("an HTTP client builds"),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key,
        }
    }
}

impl Provider for AnthropicMessages {
    fn stream(&self, request: ChatRequest) -> ProviderStream {
        let mut http = self
            .client
            .post(format!("{}/messages", self.base_url))
            .header("anthropic-version", API_VERSION)
            .json(&request_body(&request));
        if let Some(key) = &self.api_key {
            http = http.header("x-api-key", key);
        }
        let local = request.options.local;
        sse::events(
            sse::send(http, local),
            MessagesStreamParser::default(),
            local,
        )
    }
}
