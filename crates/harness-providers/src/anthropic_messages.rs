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

/// Builds a streaming Messages request body. Consecutive messages of one role become one message:
/// tool results are user content here, so a note or prompt after them joins their message.
/// Empty text and empty assistant messages are left out, since the API rejects them. The system
/// prompt and the last message are prompt-cache breakpoints.
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
                        "id": call.id,
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
                let mut block = json!({"type": "tool_result", "tool_use_id": call_id});
                if !content.is_empty() {
                    block["content"] = json!(content);
                }
                if *is_error {
                    block["is_error"] = json!(true);
                }
                push("user", vec![block]);
            }
        }
    }
    if let Some(block) = messages
        .last_mut()
        .and_then(|(_, content)| content.last_mut())
    {
        block["cache_control"] = json!({"type": "ephemeral"});
    }
    let messages: Vec<Value> = messages
        .into_iter()
        .map(|(role, content)| json!({"role": role, "content": content}))
        .collect();
    let mut body = json!({
        "model": req.model,
        "max_tokens": req.options.max_output_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
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

/// A text block, or none for empty text.
fn text_block(text: &str) -> Vec<Value> {
    if text.is_empty() {
        Vec::new()
    } else {
        vec![json!({"type": "text", "text": text})]
    }
}

#[derive(Debug, Default)]
struct PartialCall {
    id: String,
    name: String,
    arguments: String,
}

/// Turns Messages stream events into [`ProviderEvent`]s. Tool calls are buffered by content block
/// and emitted whole, with the usage, when the message stops.
#[derive(Debug, Default)]
pub struct MessagesStreamParser {
    calls: BTreeMap<u64, PartialCall>,
    usage: Option<Usage>,
    finish: Option<FinishReason>,
    done: bool,
}

impl MessagesStreamParser {
    pub fn push(&mut self, data: &str) -> Result<Vec<ProviderEvent>, ProviderError> {
        let event: Value = serde_json::from_str(data)
            .map_err(|e| ProviderError::Protocol(format!("{e} in event: {data}")))?;
        let mut out = Vec::new();
        match event["type"].as_str().unwrap_or_default() {
            "message_start" => {
                let usage = &event["message"]["usage"];
                let count = |key: &str| usage[key].as_u64().unwrap_or(0);
                // Input is everything sent: uncached, written to the cache, and read from it.
                self.usage = Some(Usage {
                    input_tokens: count("input_tokens")
                        + count("cache_creation_input_tokens")
                        + count("cache_read_input_tokens"),
                    output_tokens: count("output_tokens"),
                    cached_tokens: count("cache_read_input_tokens"),
                });
            }
            "content_block_start" => {
                let block = &event["content_block"];
                if block["type"] == "tool_use" {
                    let index = event["index"].as_u64().unwrap_or(0);
                    let call = self.calls.entry(index).or_default();
                    call.id = block["id"].as_str().unwrap_or_default().to_string();
                    call.name = block["name"].as_str().unwrap_or_default().to_string();
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
                        "max_tokens" => FinishReason::Length,
                        other => FinishReason::Other(other.to_string()),
                    });
                }
                if let (Some(usage), Some(output)) = (
                    self.usage.as_mut(),
                    event["usage"]["output_tokens"].as_u64(),
                ) {
                    usage.output_tokens = output;
                }
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
            .usage
            .take()
            .map(ProviderEvent::Usage)
            .into_iter()
            .collect();
        out.extend(std::mem::take(&mut self.calls).into_values().map(|call| {
            ProviderEvent::ToolCall(ToolCall {
                id: call.id,
                name: call.name,
                arguments: if call.arguments.trim().is_empty() {
                    "{}".into()
                } else {
                    call.arguments
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
    ProviderError::Http {
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
            client: reqwest::Client::new(),
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
        sse::events(sse::send(http), MessagesStreamParser::default())
    }
}
