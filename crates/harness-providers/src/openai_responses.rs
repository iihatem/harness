//! The OpenAI Responses protocol (`POST /responses`, server-sent events): OpenAI API keys, and
//! ChatGPT sign-in. Requests are stateless: each carries the whole conversation with
//! `store: false`, so nothing on the server has to outlive a model switch.

use std::{collections::BTreeMap, time::Duration};

use harness_core::{
    message::{ChatRequest, Message, ToolCall, Usage},
    provider::{FinishReason, Provider, ProviderError, ProviderEvent, ProviderStream},
};
use serde_json::{Value, json};

use crate::sse::{self, EventParser};

/// Builds a streaming Responses request body. With `summaries`, OpenAI's reasoning models are
/// asked for reasoning summaries even when no profile sets an effort; without it, only a model
/// whose profile sets one is.
pub fn request_body(req: &ChatRequest, summaries: bool) -> Value {
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
    // Summaries stream as reasoning deltas; without a profile's effort, the API's default is kept.
    if let Some(effort) = &req.options.reasoning_effort {
        body["reasoning"] = json!({"effort": effort, "summary": "auto"});
    } else if summaries && reasons(&req.model) {
        body["reasoning"] = json!({"summary": "auto"});
    }
    body
}

/// Whether `model` is one of OpenAI's reasoning families, `gpt-5*` (but not its non-reasoning
/// `gpt-5-chat*`) and `o1`, `o3`, `o4-mini` and the like, which take reasoning settings.
fn reasons(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    let o_series = model
        .strip_prefix('o')
        .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()));
    o_series || (model.starts_with("gpt-5") && !model.starts_with("gpt-5-chat"))
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
    /// Whether `OutputStarted` has already been emitted for this reply.
    started: bool,
}

impl ResponsesStreamParser {
    /// Emits `OutputStarted` the first time this reply produces any content: a text, refusal or
    /// reasoning delta, or a function call starting or streaming its first argument fragment. A
    /// function call itself is buffered and arrives whole only once its output item is done, so
    /// this is the only early signal such a reply gives.
    fn mark_started(&mut self, out: &mut Vec<ProviderEvent>) {
        if !self.started {
            self.started = true;
            out.push(ProviderEvent::OutputStarted);
        }
    }

    pub fn push(&mut self, data: &str) -> Result<Vec<ProviderEvent>, ProviderError> {
        let event: Value = serde_json::from_str(data)
            .map_err(|e| ProviderError::Protocol(format!("{e} in event: {data}")))?;
        let mut out = Vec::new();
        let delta = || event["delta"].as_str().filter(|d| !d.is_empty());
        match event["type"].as_str().unwrap_or_default() {
            // A refusal is the model's answer, and is shown as one.
            "response.output_text.delta" | "response.refusal.delta" => {
                if let Some(text) = delta() {
                    self.mark_started(&mut out);
                    out.push(ProviderEvent::TextDelta(text.to_string()));
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some(text) = delta() {
                    self.mark_started(&mut out);
                    out.push(ProviderEvent::ReasoningDelta(text.to_string()));
                }
            }
            // Each part of a summary after the first starts a new paragraph.
            "response.reasoning_summary_part.added" => {
                if event["summary_index"].as_u64().is_some_and(|i| i > 0) {
                    self.mark_started(&mut out);
                    out.push(ProviderEvent::ReasoningDelta("\n\n".into()));
                }
            }
            "response.output_item.added" | "response.output_item.done"
                if event["item"]["type"] == "function_call" =>
            {
                self.mark_started(&mut out);
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
                    if !fragment.is_empty() {
                        self.mark_started(&mut out);
                    }
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
            // The details are at the top level, or nested under `error` (where Codex reads them).
            "error" => {
                let error = event.get("error").filter(|e| e.is_object());
                return Err(stream_error(error.unwrap_or(&event)));
            }
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

/// The error an `error` event or a failed response reports, classified by its code as Codex
/// classifies it (`codex-api/src/sse/responses_error.rs`):
/// - an exhausted quota is a 429 whose body names it, which is not retried and says when the
///   quota resets;
/// - a context overflow, and a prompt or content refused, are final;
/// - an overloaded server is retried like a 503, and a rate limit or a request to slow down like a
///   429, waiting as long as the message asks ("try again in 11.054s");
/// - any other code, or none, is retried like a server error.
fn stream_error(error: &Value) -> ProviderError {
    let code = error["code"].as_str().unwrap_or_default();
    let message = error["message"].as_str().unwrap_or_default();
    let text = match (code, message) {
        ("", "") if error.is_object() => error.to_string(),
        ("", "") => "the response failed without saying why".to_string(),
        ("", message) => message.to_string(),
        (code, "") => code.to_string(),
        (code, message) => format!("{code}: {message}"),
    };
    let status = match code {
        "insufficient_quota"
        | "usage_limit_reached"
        | "usage_not_included"
        | "credit_balance_exhausted"
        | "organization_spend_limit_exceeded"
        | "project_spend_limit_exceeded" => {
            // As the HTTP form's body, so that it reads as an exhausted quota, with its reset time.
            return ProviderError::Reported {
                status: 429,
                body: json!({ "error": error }).to_string(),
                retry_after: None,
            };
        }
        "context_length_exceeded"
        | "invalid_prompt"
        | "cyber_policy"
        | "bio_policy"
        | "misalignment_policy_violation" => return ProviderError::InStream(text),
        "server_is_overloaded" => 503,
        "rate_limit_exceeded" | "slow_down" => 429,
        _ => 500,
    };
    ProviderError::Reported {
        status,
        retry_after: requested_wait(message),
        body: text,
    }
}

/// The wait a message asks for, as in "Please try again in 11.054s", "in 28ms" or "in 35
/// seconds".
fn requested_wait(message: &str) -> Option<Duration> {
    const PHRASE: &str = "try again in";
    let start = message.to_ascii_lowercase().find(PHRASE)? + PHRASE.len();
    let rest = message[start..].trim_start();
    let number = rest
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(rest.len());
    let (whole, fraction) = rest[..number]
        .split_once('.')
        .unwrap_or((&rest[..number], ""));
    if whole.is_empty() || fraction.contains('.') {
        return None;
    }
    // In nanoseconds, from the digits, so that 11.054 is exactly 11,054 ms.
    let digits: String = format!("{whole}{fraction:0<9.9}");
    let nanos: u128 = digits.parse().ok()?;
    let unit = rest[number..].trim_start().to_ascii_lowercase();
    let nanos = if unit.starts_with("ms") {
        nanos / 1_000
    } else if unit.starts_with('s') {
        nanos
    } else {
        return None;
    };
    Some(Duration::from_nanos(u64::try_from(nanos).ok()?))
}

/// Whether `error` is the API refusing reasoning summaries, as it does to an organization it has
/// not verified ("Your organization must be verified to generate reasoning summaries").
fn refuses_summaries(error: &ProviderError) -> bool {
    let ProviderError::Http {
        status: 400, body, ..
    } = error
    else {
        return false;
    };
    serde_json::from_str::<Value>(body)
        .is_ok_and(|value| value["error"]["param"] == "reasoning.summary")
}

/// Leaves reasoning summaries out of a request `body`, and `reasoning` too when nothing is left.
fn drop_summary(body: &mut Value) {
    if let Some(reasoning) = body.get_mut("reasoning").and_then(Value::as_object_mut) {
        reasoning.remove("summary");
        if reasoning.is_empty()
            && let Some(body) = body.as_object_mut()
        {
            body.remove("reasoning");
        }
    }
}

/// How requests are authorized.
enum Auth {
    /// An API key, when the endpoint needs one.
    Key(Option<String>),
    /// A signed-in ChatGPT account.
    #[cfg(feature = "chatgpt-login")]
    ChatGpt(std::sync::Arc<crate::chatgpt::auth::ChatGptAuth>),
}

/// A provider speaking the Responses protocol, with an API key or a ChatGPT account.
pub struct OpenAiResponses {
    client: reqwest::Client,
    base_url: String,
    auth: Auth,
    /// Whether reasoning models are asked for summaries without a profile's effort: only from
    /// OpenAI's own API, whose refusal harness recognises, and ChatGPT's backend.
    summaries: bool,
    /// Set once the API refused reasoning summaries (to an organization it has not verified):
    /// later requests do not ask for them.
    no_summaries: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl OpenAiResponses {
    pub fn new(base_url: impl Into<String>, api_key: Option<String>) -> Self {
        OpenAiResponses {
            client: crate::http::client()
                .build()
                .expect("an HTTP client builds"),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            auth: Auth::Key(api_key),
            summaries: false,
            no_summaries: Default::default(),
        }
    }

    /// Asks reasoning models for summaries even when no profile sets an effort, as OpenAI's own
    /// API is asked (the built-in `openai` provider).
    pub fn with_default_summaries(mut self) -> Self {
        self.summaries = true;
        self
    }

    /// ChatGPT's backend, as the signed-in account `auth`.
    #[cfg(feature = "chatgpt-login")]
    pub fn chatgpt(
        base_url: impl Into<String>,
        auth: std::sync::Arc<crate::chatgpt::auth::ChatGptAuth>,
    ) -> Self {
        OpenAiResponses {
            client: crate::http::client()
                .build()
                .expect("an HTTP client builds"),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            auth: Auth::ChatGpt(auth),
            summaries: true,
            no_summaries: Default::default(),
        }
    }
}

impl Provider for OpenAiResponses {
    fn stream(&self, request: ChatRequest) -> ProviderStream {
        let url = format!("{}/responses", self.base_url);
        match &self.auth {
            Auth::Key(key) => {
                use std::sync::atomic::Ordering::Relaxed;
                let mut body = request_body(&request, self.summaries);
                let no_summaries = self.no_summaries.clone();
                if no_summaries.load(Relaxed) {
                    drop_summary(&mut body);
                }
                let (client, key) = (self.client.clone(), key.clone());
                let local = request.options.local;
                let send = move |body: &Value| {
                    let mut http = client.post(&url).json(body);
                    if let Some(key) = &key {
                        http = http.bearer_auth(key);
                    }
                    sse::send(http, local)
                };
                // Summaries refused to an organization OpenAI has not verified: sent again
                // without them, and not asked for again.
                let response = async move {
                    let first = send(&body).await?;
                    if first.status() != reqwest::StatusCode::BAD_REQUEST
                        || body["reasoning"].get("summary").is_none()
                    {
                        return Ok(first);
                    }
                    let error = sse::http_error(first).await;
                    if !refuses_summaries(&error) {
                        return Err(error);
                    }
                    no_summaries.store(true, Relaxed);
                    drop_summary(&mut body);
                    send(&body).await
                };
                sse::events(response, ResponsesStreamParser::default(), local)
            }
            #[cfg(feature = "chatgpt-login")]
            Auth::ChatGpt(auth) => {
                let mut body = request_body(&request, self.summaries);
                // ChatGPT's backend takes no output limit (Codex never sends one). Every model it
                // serves reasons, and streams its summaries.
                if let Some(object) = body.as_object_mut() {
                    object.remove("max_output_tokens");
                    object
                        .entry("reasoning")
                        .or_insert_with(|| json!({"summary": "auto"}));
                }
                let (client, auth) = (self.client.clone(), auth.clone());
                // A 401 renews the tokens once, and the request is sent once more.
                let response = async move {
                    let tokens = auth.current().await?;
                    let first =
                        sse::send(chatgpt_request(&client, &url, &body, &tokens), false).await?;
                    if first.status() != reqwest::StatusCode::UNAUTHORIZED {
                        return Ok(first);
                    }
                    let tokens = auth.after_unauthorized(&tokens.access_token).await?;
                    let second =
                        sse::send(chatgpt_request(&client, &url, &body, &tokens), false).await?;
                    if second.status() == reqwest::StatusCode::UNAUTHORIZED {
                        return Err(auth.still_refused(sse::http_error(second).await));
                    }
                    Ok(second)
                };
                // ChatGPT's backend is hosted.
                sse::events(response, ResponsesStreamParser::default(), false)
            }
        }
    }
}

/// A request to ChatGPT's backend: the access token, and the account it belongs to.
#[cfg(feature = "chatgpt-login")]
fn chatgpt_request(
    client: &reqwest::Client,
    url: &str,
    body: &Value,
    tokens: &crate::chatgpt::oauth::Tokens,
) -> reqwest::RequestBuilder {
    let mut request = client
        .post(url)
        .bearer_auth(&tokens.access_token)
        .header("originator", crate::chatgpt::oauth::ORIGINATOR)
        .json(body);
    if let Some(account) = &tokens.account_id {
        request = request.header("ChatGPT-Account-ID", account);
    }
    request
}
