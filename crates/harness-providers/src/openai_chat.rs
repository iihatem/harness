use std::collections::BTreeMap;

use harness_core::{
    message::{ChatRequest, Message, ToolCall, Usage},
    provider::{FinishReason, Provider, ProviderError, ProviderEvent, ProviderStream},
};
use serde_json::{Value, json};

use crate::sse::{self, EventParser};

/// Builds a streaming Chat Completions request body.
pub fn request_body(req: &ChatRequest) -> Value {
    let mut messages = vec![json!({"role": "system", "content": req.system})];
    for message in &req.messages {
        messages.push(match message {
            Message::User { content } => json!({"role": "user", "content": content}),
            Message::Assistant { content, tool_calls, .. } => {
                let text = if content.is_empty() && !tool_calls.is_empty() { Value::Null } else { json!(content) };
                let mut value = json!({"role": "assistant", "content": text});
                if !tool_calls.is_empty() {
                    value["tool_calls"] = Value::Array(
                        tool_calls
                            .iter()
                            .map(|c| json!({"id": c.id, "type": "function", "function": {"name": c.name, "arguments": c.arguments}}))
                            .collect(),
                    );
                }
                value
            }
            Message::Tool { call_id, content, .. } => json!({"role": "tool", "tool_call_id": call_id, "content": content}),
        });
    }
    let mut body = json!({
        "model": req.model,
        "stream": true,
        "stream_options": {"include_usage": true},
        "messages": messages,
    });
    if !req.tools.is_empty() {
        body["tools"] = Value::Array(
            req.tools
                .iter()
                .map(|t| json!({"type": "function", "function": {"name": t.name, "description": t.description, "parameters": t.parameters}}))
                .collect(),
        );
    }
    body
}

#[derive(Debug, Default)]
struct PartialCall {
    id: Option<String>,
    name: String,
    arguments: String,
}

/// Turns SSE `data:` payloads into [`ProviderEvent`]s. Tool calls are buffered and emitted whole at the end.
#[derive(Debug, Default)]
pub struct ChatStreamParser {
    calls: BTreeMap<u64, PartialCall>,
    finish: Option<FinishReason>,
    done: bool,
}

impl ChatStreamParser {
    pub fn push(&mut self, data: &str) -> Result<Vec<ProviderEvent>, ProviderError> {
        if data.trim() == "[DONE]" {
            return Ok(self.finish());
        }
        let chunk: Value = serde_json::from_str(data)
            .map_err(|e| ProviderError::Protocol(format!("{e} in chunk: {data}")))?;
        if let Some(error) = chunk.get("error") {
            return Err(ProviderError::InStream(error.to_string()));
        }
        let mut out = Vec::new();
        if let Some(choice) = chunk["choices"].get(0) {
            let delta = &choice["delta"];
            if let Some(text) = delta["content"].as_str().filter(|t| !t.is_empty()) {
                out.push(ProviderEvent::TextDelta(text.to_string()));
            }
            for key in ["reasoning_content", "reasoning"] {
                if let Some(text) = delta[key].as_str().filter(|t| !t.is_empty()) {
                    out.push(ProviderEvent::ReasoningDelta(text.to_string()));
                }
            }
            if let Some(calls) = delta["tool_calls"].as_array() {
                for call in calls {
                    // An empty-string id or function name is not a real value; treat it as absent
                    // like a provider that omitted the field entirely.
                    let call_id = call["id"].as_str().filter(|s| !s.is_empty());
                    let func_name = call["function"]["name"].as_str().filter(|s| !s.is_empty());
                    let starts_new = call_id.is_some() || func_name.is_some();
                    let index = match call["index"].as_u64() {
                        Some(index) => index,
                        None if starts_new => self.calls.len() as u64,
                        None => self.calls.keys().last().copied().unwrap_or(0),
                    };
                    let entry = self.calls.entry(index).or_default();
                    if let Some(id) = call_id {
                        entry.id = Some(id.to_string());
                    }
                    if let Some(name) = func_name {
                        entry.name.push_str(name);
                    }
                    match &call["function"]["arguments"] {
                        Value::String(fragment) => entry.arguments.push_str(fragment),
                        Value::Null => {}
                        other => entry.arguments.push_str(&other.to_string()),
                    }
                }
            }
            if let Some(reason) = choice["finish_reason"].as_str() {
                self.finish = Some(match reason {
                    "stop" => FinishReason::Stop,
                    "tool_calls" => FinishReason::ToolCalls,
                    "length" => FinishReason::Length,
                    other => FinishReason::Other(other.to_string()),
                });
            }
        }
        if let Some(usage) = chunk.get("usage").filter(|u| u.is_object()) {
            out.push(ProviderEvent::Usage(Usage {
                input_tokens: usage["prompt_tokens"].as_u64().unwrap_or(0),
                output_tokens: usage["completion_tokens"].as_u64().unwrap_or(0),
                cached_tokens: usage["prompt_tokens_details"]["cached_tokens"]
                    .as_u64()
                    .unwrap_or(0),
            }));
        }
        Ok(out)
    }

    /// Emits buffered tool calls followed by `Finished`. Calling it again yields nothing.
    pub fn finish(&mut self) -> Vec<ProviderEvent> {
        if self.done {
            return Vec::new();
        }
        self.done = true;
        let mut out: Vec<ProviderEvent> = std::mem::take(&mut self.calls)
            .into_iter()
            .map(|(index, call)| {
                ProviderEvent::ToolCall(ToolCall {
                    id: call.id.unwrap_or_else(|| format!("call_{index}")),
                    name: call.name,
                    arguments: if call.arguments.trim().is_empty() {
                        "{}".into()
                    } else {
                        call.arguments
                    },
                })
            })
            .collect();
        out.push(ProviderEvent::Finished(
            self.finish.take().unwrap_or(FinishReason::Stop),
        ));
        out
    }

    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Whether a chunk with a `finish_reason` has been seen, even if `[DONE]` never arrived.
    pub fn saw_finish_reason(&self) -> bool {
        self.finish.is_some()
    }
}

/// A provider speaking the OpenAI Chat Completions protocol (Ollama, LM Studio, llama.cpp, OpenRouter, ...).
pub struct OpenAiChat {
    client: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
}

impl OpenAiChat {
    pub fn new(base_url: impl Into<String>, api_key: Option<String>) -> Self {
        OpenAiChat {
            client: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key,
        }
    }
}

impl EventParser for ChatStreamParser {
    fn push(&mut self, data: &str) -> Result<Vec<ProviderEvent>, ProviderError> {
        ChatStreamParser::push(self, data)
    }

    fn finish(&mut self) -> Vec<ProviderEvent> {
        ChatStreamParser::finish(self)
    }

    fn is_done(&self) -> bool {
        ChatStreamParser::is_done(self)
    }

    /// Some servers omit the trailing `[DONE]`: a `finish_reason` already ends the reply.
    fn may_end(&self) -> bool {
        self.is_done() || self.saw_finish_reason()
    }
}

impl Provider for OpenAiChat {
    fn stream(&self, request: ChatRequest) -> ProviderStream {
        let mut http = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .json(&request_body(&request));
        if let Some(key) = &self.api_key {
            http = http.bearer_auth(key);
        }
        sse::events(sse::send(http), ChatStreamParser::default())
    }
}
