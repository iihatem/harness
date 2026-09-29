//! The OpenAI Responses protocol (`POST /responses`, server-sent events): OpenAI API keys, and
//! ChatGPT sign-in. Requests are stateless: each carries the whole conversation with
//! `store: false`, so nothing on the server has to outlive a model switch.

use std::collections::BTreeMap;

use harness_core::{
    message::{ChatRequest, Message, ToolCall, Usage},
    provider::{FinishReason, Provider, ProviderError, ProviderEvent, ProviderStream},
};
use serde_json::{Value, json};

use crate::sse::{self, EventParser};

/// Builds a streaming Responses request body.
pub fn request_body(req: &ChatRequest) -> Value {
    let mut input = Vec::new();
    for message in &req.messages {
        match message {
            Message::User { content } => input.push(json!({
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": content}],
            })),
            Message::Assistant {
                content,
                tool_calls,
                ..
            } => {
                if !content.is_empty() {
                    input.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": content}],
                    }));
                }
                for call in tool_calls {
                    input.push(json!({
                        "type": "function_call",
                        "call_id": call.id,
                        "name": call.name,
                        "arguments": call.arguments,
                    }));
                }
            }
            Message::Tool {
                call_id, content, ..
            } => input.push(json!({
                "type": "function_call_output",
                "call_id": call_id,
                "output": content,
            })),
        }
    }
    let mut body = json!({
        "model": req.model,
        "instructions": req.system,
        "input": input,
        "stream": true,
        "store": false,
    });
    if !req.tools.is_empty() {
        // Strict schemas would have to list every property as required.
        body["tools"] = Value::Array(
            req.tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                        "strict": false,
                    })
                })
                .collect(),
        );
        body["tool_choice"] = json!("auto");
    }
    if let Some(tokens) = req.options.max_output_tokens {
        body["max_output_tokens"] = json!(tokens);
    }
    if let Some(temperature) = req.options.temperature {
        body["temperature"] = json!(temperature);
    }
    if let Some(effort) = &req.options.reasoning_effort {
        body["reasoning"] = json!({"effort": effort, "summary": "auto"});
    }
    body
}

#[derive(Debug, Default)]
struct PartialCall {
    call_id: String,
    name: String,
    arguments: String,
}

/// Turns Responses stream events into [`ProviderEvent`]s. Function calls are buffered by output
/// index and emitted whole, in output order, when the response ends.
#[derive(Debug, Default)]
pub struct ResponsesStreamParser {
    calls: BTreeMap<u64, PartialCall>,
    finish: Option<FinishReason>,
    done: bool,
}

impl ResponsesStreamParser {
    pub fn push(&mut self, data: &str) -> Result<Vec<ProviderEvent>, ProviderError> {
        let event: Value = serde_json::from_str(data)
            .map_err(|e| ProviderError::Protocol(format!("{e} in event: {data}")))?;
        let mut out = Vec::new();
        let delta = || event["delta"].as_str().filter(|d| !d.is_empty());
        match event["type"].as_str().unwrap_or_default() {
            "response.output_text.delta" => {
                if let Some(text) = delta() {
                    out.push(ProviderEvent::TextDelta(text.to_string()));
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some(text) = delta() {
                    out.push(ProviderEvent::ReasoningDelta(text.to_string()));
                }
            }
            "response.output_item.added" | "response.output_item.done"
                if event["item"]["type"] == "function_call" =>
            {
                let item = &event["item"];
                let call = self.call(&event);
                if let Some(id) = item["call_id"].as_str() {
                    call.call_id = id.to_string();
                }
                if let Some(name) = item["name"].as_str() {
                    call.name = name.to_string();
                }
                // `done` carries the complete arguments; `added` usually none yet.
                if let Some(arguments) = item["arguments"].as_str().filter(|a| !a.is_empty()) {
                    call.arguments = arguments.to_string();
                }
            }
            "response.function_call_arguments.delta" => {
                if let Some(fragment) = event["delta"].as_str() {
                    self.call(&event).arguments.push_str(fragment);
                }
            }
            "response.function_call_arguments.done" => {
                if let Some(arguments) = event["arguments"].as_str() {
                    self.call(&event).arguments = arguments.to_string();
                }
            }
            "response.completed" | "response.incomplete" => {
                let response = &event["response"];
                if let Some(usage) = response.get("usage").filter(|u| u.is_object()) {
                    out.push(ProviderEvent::Usage(Usage {
                        input_tokens: usage["input_tokens"].as_u64().unwrap_or(0),
                        output_tokens: usage["output_tokens"].as_u64().unwrap_or(0),
                        cached_tokens: usage["input_tokens_details"]["cached_tokens"]
                            .as_u64()
                            .unwrap_or(0),
                    }));
                }
                self.finish = Some(match response["incomplete_details"]["reason"].as_str() {
                    Some("max_output_tokens") => FinishReason::Length,
                    Some(other) => FinishReason::Other(other.to_string()),
                    None if !self.calls.is_empty() => FinishReason::ToolCalls,
                    None => FinishReason::Stop,
                });
                out.extend(self.finish());
            }
            "response.failed" => return Err(stream_error(&event["response"]["error"])),
            "error" => return Err(stream_error(&event)),
            _ => {}
        }
        Ok(out)
    }

    /// The call at the event's output index.
    fn call(&mut self, event: &Value) -> &mut PartialCall {
        let index = event["output_index"]
            .as_u64()
            .unwrap_or_else(|| self.calls.keys().last().copied().unwrap_or(0));
        self.calls.entry(index).or_default()
    }

    /// Emits buffered calls followed by `Finished`. Calling it again yields nothing.
    pub fn finish(&mut self) -> Vec<ProviderEvent> {
        if self.done {
            return Vec::new();
        }
        self.done = true;
        let mut out: Vec<ProviderEvent> = std::mem::take(&mut self.calls)
            .into_iter()
            .map(|(index, call)| {
                ProviderEvent::ToolCall(ToolCall {
                    id: if call.call_id.is_empty() {
                        format!("call_{index}")
                    } else {
                        call.call_id
                    },
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
}

impl EventParser for ResponsesStreamParser {
    fn push(&mut self, data: &str) -> Result<Vec<ProviderEvent>, ProviderError> {
        ResponsesStreamParser::push(self, data)
    }

    fn finish(&mut self) -> Vec<ProviderEvent> {
        ResponsesStreamParser::finish(self)
    }

    fn is_done(&self) -> bool {
        ResponsesStreamParser::is_done(self)
    }
}

/// The error an `error` event or a failed response reports. Server errors and rate limits are
/// worth retrying, as their HTTP forms are.
fn stream_error(error: &Value) -> ProviderError {
    let code = error["code"].as_str().unwrap_or_default();
    let message = error["message"].as_str().unwrap_or_default();
    let text = match (code, message) {
        ("", "") => error.to_string(),
        ("", message) => message.to_string(),
        (code, "") => code.to_string(),
        (code, message) => format!("{code}: {message}"),
    };
    let status = match code {
        "server_error" => 500,
        "rate_limit_exceeded" => 429,
        _ => return ProviderError::InStream(text),
    };
    ProviderError::Http {
        status,
        body: text,
        retry_after: None,
    }
}

/// A provider speaking the Responses protocol with an optional API key.
pub struct OpenAiResponses {
    client: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
}

impl OpenAiResponses {
    pub fn new(base_url: impl Into<String>, api_key: Option<String>) -> Self {
        OpenAiResponses {
            client: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key,
        }
    }
}

impl Provider for OpenAiResponses {
    fn stream(&self, request: ChatRequest) -> ProviderStream {
        let mut http = self
            .client
            .post(format!("{}/responses", self.base_url))
            .json(&request_body(&request));
        if let Some(key) = &self.api_key {
            http = http.bearer_auth(key);
        }
        sse::events(sse::send(http), ResponsesStreamParser::default())
    }
}
