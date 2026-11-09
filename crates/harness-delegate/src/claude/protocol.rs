//! What `claude -p --input-format stream-json --output-format stream-json
//! --permission-prompt-tool stdio` writes and reads, as a state machine.
//!
//! Wire shapes: the control messages (`control_request` with `can_use_tool` and `interrupt`,
//! `control_response`, `control_cancel_request`) and `rate_limit_event` are the SDK's, read from
//! the schema text embedded in the stock 2.1.312 binary; the assistant/user/stream-event bodies
//! are the Anthropic Messages shapes. Nothing here was checked against a live run (see the
//! plan). An unknown message type is ignored, so a newer CLI that adds events keeps working.

use std::collections::{HashMap, HashSet};

use harness_core::{
    event::{ErrorKind, TurnEndReason},
    message::Usage,
    meter::{Window, WindowSnapshot, WindowSource},
};
use nix::sys::signal::Signal;
use serde_json::{Value, json};

use crate::{
    DelegateError,
    driver::{Out, Protocol},
    event::{
        ApprovalAnswer, DelegateApproval, DelegateApprovalKind, DelegateEvent, FileChange,
        LimitStatus,
    },
};

/// Which credential the run was asked to use, checked against `system/init.apiKeySource`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthKind {
    Subscription,
    ApiKey,
}

#[derive(Debug, Clone)]
pub struct ClaudeParams {
    pub task: String,
    /// The version `--version` printed; used until `system/init` says its own.
    pub child_version: String,
    pub auth: AuthKind,
    /// The session's cost total before this run (a resumed session reports the whole total).
    pub cost_baseline_usd: f64,
    /// Seconds since the Unix epoch, for the time a window was observed.
    pub clock: fn() -> u64,
}

impl ClaudeParams {
    pub fn new(task: impl Into<String>, child_version: impl Into<String>) -> ClaudeParams {
        ClaudeParams {
            task: task.into(),
            child_version: child_version.into(),
            auth: AuthKind::Subscription,
            cost_baseline_usd: 0.0,
            clock: unix_now,
        }
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

struct Pending {
    tool: String,
    input: Value,
    suggestions: Vec<Value>,
}

pub struct ClaudeProtocol {
    params: ClaudeParams,
    session_id: Option<String>,
    model: Option<String>,
    /// Text and thinking streamed as deltas and not yet matched by a block of an assistant
    /// message: a block that equals the front of it was already shown.
    text_streamed: String,
    thinking_streamed: String,
    /// Tool calls seen and not yet finished: id to (name, input).
    calls: HashMap<String, (String, Value)>,
    pending: HashMap<String, Pending>,
    withdrawn: HashSet<String>,
    interrupted: bool,
    requests: u32,
}

const FILE_TOOLS: [&str; 4] = ["Edit", "Write", "MultiEdit", "NotebookEdit"];

fn warning(message: String) -> DelegateEvent {
    DelegateEvent::Warning { message }
}

fn line(value: Value) -> String {
    serde_json::to_string(&value).expect("a JSON value serializes")
}

fn events(events: Vec<DelegateEvent>) -> Out {
    Out {
        events,
        ..Out::default()
    }
}

/// Whether `block` was already streamed: it is the front of what was, which is then used up.
/// When it is not, whatever was streamed is dropped (it belonged to a block that never came).
fn take_streamed(streamed: &mut String, block: &str) -> bool {
    match streamed.strip_prefix(block) {
        Some(rest) => {
            *streamed = rest.to_string();
            true
        }
        None => {
            streamed.clear();
            false
        }
    }
}

impl ClaudeProtocol {
    pub fn new(params: ClaudeParams) -> ClaudeProtocol {
        ClaudeProtocol {
            params,
            session_id: None,
            model: None,
            text_streamed: String::new(),
            thinking_streamed: String::new(),
            calls: HashMap::new(),
            pending: HashMap::new(),
            withdrawn: HashSet::new(),
            interrupted: false,
            requests: 0,
        }
    }

    fn subagent(v: &Value) -> bool {
        v.get("parent_tool_use_id").is_some_and(|p| !p.is_null())
    }

    fn system(&mut self, v: &Value) -> Out {
        match v["subtype"].as_str() {
            Some("init") => self.init(v),
            Some("api_retry") => {
                let error = v["error"].as_str().unwrap_or("error");
                let reason = match v["error_status"].as_u64() {
                    Some(status) => format!("{error} ({status})"),
                    None => error.to_string(),
                };
                events(vec![DelegateEvent::Retrying {
                    attempt: u32::try_from(v["attempt"].as_u64().unwrap_or(1)).unwrap_or(1),
                    reason,
                    delay_ms: v["retry_delay_ms"].as_u64().unwrap_or(0),
                }])
            }
            Some("model_fallback") => events(vec![warning(format!(
                "claude switched from {} to {} ({})",
                v["original_model"].as_str().unwrap_or("its model"),
                v["fallback_model"].as_str().unwrap_or("another"),
                v["trigger"].as_str().unwrap_or("it failed"),
            ))]),
            _ => Out::default(),
        }
    }

    fn init(&mut self, v: &Value) -> Out {
        self.session_id = v["session_id"].as_str().map(String::from);
        self.model = v["model"].as_str().map(String::from);
        let version = v["claude_code_version"]
            .as_str()
            .unwrap_or(&self.params.child_version)
            .to_string();
        let started = DelegateEvent::Started {
            session_id: self.session_id.clone().unwrap_or_default(),
            model: self.model.clone(),
            child_version: version,
        };
        // Anything but `none` is a key of some kind (the environment's, a helper's, a managed
        // one, or one from settings): in subscription mode none may be in use.
        let source = v["apiKeySource"].as_str().unwrap_or("none");
        let uses_key = source != "none";
        let provider = v["apiProvider"]
            .as_str()
            .or_else(|| v["account"]["apiProvider"].as_str());
        let wrong = match (self.params.auth, provider) {
            (AuthKind::Subscription, Some(provider)) if provider != "firstParty" => Some(format!(
                "claude reports it uses the `{provider}` provider but this run is in subscription mode: the run was stopped"
            )),
            (AuthKind::Subscription, _) if uses_key => Some(format!(
                "claude reports it uses an API key ({source}) but this run is in subscription mode: the run was stopped"
            )),
            (AuthKind::ApiKey, _) if !uses_key => Some(
                "claude is not using the API key harness passed (apiKeySource: none): the run was stopped"
                    .to_string(),
            ),
            _ => None,
        };
        let mut out = events(vec![started]);
        if let Some(message) = wrong {
            out.events.push(DelegateEvent::Error {
                kind: ErrorKind::Internal,
                message,
            });
            out.events.push(DelegateEvent::Finished {
                reason: TurnEndReason::Error,
                detail: None,
            });
            out.finished = true;
            out.stop = true;
            out.close_stdin = true;
        }
        out
    }

    fn stream_event(&mut self, v: &Value) -> Out {
        if Self::subagent(v) {
            return Out::default();
        }
        let delta = &v["event"]["delta"];
        if v["event"]["type"] == "message_start" {
            // Whatever an earlier message streamed and never delivered is not this one's.
            self.text_streamed.clear();
            self.thinking_streamed.clear();
        }
        if v["event"]["type"] != "content_block_delta" {
            return Out::default();
        }
        match delta["type"].as_str() {
            Some("text_delta") => {
                let text = delta["text"].as_str().unwrap_or_default();
                self.text_streamed.push_str(text);
                events(vec![DelegateEvent::TextDelta {
                    text: text.to_string(),
                }])
            }
            Some("thinking_delta") => {
                let text = delta["thinking"].as_str().unwrap_or_default();
                self.thinking_streamed.push_str(text);
                events(vec![DelegateEvent::ReasoningDelta {
                    text: text.to_string(),
                }])
            }
            _ => Out::default(),
        }
    }

    fn assistant(&mut self, v: &Value) -> Out {
        if Self::subagent(v) {
            return Out::default();
        }
        let message = &v["message"];
        if let Some(model) = message["model"].as_str() {
            self.model = Some(model.to_string());
        }
        let mut out = Vec::new();
        for block in message["content"].as_array().into_iter().flatten() {
            match block["type"].as_str() {
                Some("text") => {
                    let text = block["text"].as_str().unwrap_or_default();
                    if !take_streamed(&mut self.text_streamed, text) {
                        out.push(DelegateEvent::TextDelta {
                            text: text.to_string(),
                        });
                    }
                    out.push(DelegateEvent::AssistantMessage {
                        content: text.to_string(),
                        model: self.model.clone(),
                    });
                }
                Some("thinking") => {
                    let thinking = block["thinking"].as_str().unwrap_or_default();
                    if !take_streamed(&mut self.thinking_streamed, thinking) {
                        out.push(DelegateEvent::ReasoningDelta {
                            text: thinking.to_string(),
                        });
                    }
                }
                Some("tool_use") => {
                    let id = block["id"].as_str().unwrap_or_default().to_string();
                    let name = block["name"].as_str().unwrap_or_default().to_string();
                    let input = block["input"].clone();
                    self.calls.insert(id.clone(), (name.clone(), input.clone()));
                    out.push(DelegateEvent::ToolCallRequested {
                        id,
                        name,
                        arguments: input.to_string(),
                    });
                }
                _ => {}
            }
        }
        events(out)
    }

    fn user(&mut self, v: &Value) -> Out {
        if Self::subagent(v) {
            return Out::default();
        }
        let mut out = Vec::new();
        for block in v["message"]["content"].as_array().into_iter().flatten() {
            if block["type"] != "tool_result" {
                continue;
            }
            let id = block["tool_use_id"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let is_error = block["is_error"].as_bool().unwrap_or(false);
            out.push(DelegateEvent::ToolCallFinished {
                id: id.clone(),
                output: tool_output(&block["content"]),
                is_error,
            });
            let call = self.calls.remove(&id);
            if let (false, Some((name, input))) = (is_error, call)
                && FILE_TOOLS.contains(&name.as_str())
                && let Some(path) = ["file_path", "notebook_path", "path"]
                    .iter()
                    .find_map(|k| input[*k].as_str())
            {
                let change = if v["tool_use_result"]["type"] == "create" {
                    FileChange::Add
                } else {
                    FileChange::Update
                };
                out.push(DelegateEvent::FileChanged {
                    call_id: id,
                    path: path.to_string(),
                    change,
                });
            }
        }
        events(out)
    }

    fn control_request(&mut self, v: &Value) -> Out {
        let Some(id) = v["request_id"].as_str().filter(|id| !id.is_empty()) else {
            // Nothing could match an answer to it, and nothing may be approved by default.
            return Out {
                events: vec![warning(
                    "claude sent a control request without a request_id: it was refused".into(),
                )],
                writes: vec![error_response("", "a control request needs a request_id")],
                ..Out::default()
            };
        };
        let id = id.to_string();
        let request = &v["request"];
        let subtype = request["subtype"].as_str().unwrap_or_default();
        if subtype != "can_use_tool" {
            // A request harness cannot serve: answer it, so the child does not wait for ever.
            return Out {
                events: vec![warning(format!(
                    "claude sent a `{subtype}` request, which harness does not handle"
                ))],
                writes: vec![error_response(
                    &id,
                    &format!("harness does not handle {subtype}"),
                )],
                ..Out::default()
            };
        }
        let tool = request["tool_name"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let input = request["input"].clone();
        if tool == "AskUserQuestion" {
            // The answer would have to come back inside the tool's input: harness cannot relay it.
            return Out {
                events: vec![warning(
                    "claude asked the user a question, which cannot be relayed: it was told to carry on"
                        .into(),
                )],
                writes: vec![permission_response(
                    &id,
                    json!({"behavior": "deny", "message":
                        "Questions to the user cannot be relayed from here: make a reasonable assumption and continue."}),
                )],
                ..Out::default()
            };
        }
        let suggestions = request["permission_suggestions"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let kind = match tool.as_str() {
            "Bash" => DelegateApprovalKind::Command {
                command: input["command"].as_str().unwrap_or_default().to_string(),
                cwd: None,
            },
            t if FILE_TOOLS.contains(&t) => DelegateApprovalKind::FileChange {
                paths: ["file_path", "notebook_path", "path"]
                    .iter()
                    .find_map(|k| input[*k].as_str())
                    .map(|p| vec![p.to_string()])
                    .unwrap_or_default(),
            },
            _ => DelegateApprovalKind::Tool,
        };
        let reason = request["decision_reason"]
            .as_str()
            .filter(|r| !r.is_empty())
            .map_or_else(|| format!("{tool} needs approval"), String::from);
        let approval = DelegateApproval {
            id: id.clone(),
            call_id: request["tool_use_id"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            tool: tool.clone(),
            arguments: input.clone(),
            kind,
            reason,
            can_remember: true,
        };
        self.pending.insert(
            id,
            Pending {
                tool,
                input,
                suggestions,
            },
        );
        events(vec![DelegateEvent::ApprovalNeeded(approval)])
    }

    fn result(&mut self, v: &Value) -> Out {
        let mut out = Vec::new();
        let denials = v["permission_denials"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if !denials.is_empty() {
            let mut names: Vec<&str> = denials
                .iter()
                .filter_map(|d| d["tool_name"].as_str())
                .collect();
            names.sort_unstable();
            names.dedup();
            out.push(warning(format!(
                "claude reports {} denied tool call(s): {}",
                denials.len(),
                names.join(", ")
            )));
        }
        out.extend(self.usage_events(v));
        if let Some(total) = v["total_cost_usd"].as_f64() {
            out.push(DelegateEvent::Cost {
                estimated_usd: (total - self.params.cost_baseline_usd).max(0.0),
                session_total_usd: total,
            });
        }
        let subtype = v["subtype"].as_str().unwrap_or_default();
        let is_error = v["is_error"].as_bool().unwrap_or(false);
        let reason = if self.interrupted {
            TurnEndReason::Interrupted
        } else {
            match (subtype, is_error) {
                ("success", false) => TurnEndReason::Completed,
                ("error_max_turns", _) => TurnEndReason::StepLimit,
                ("error_max_budget_usd", _) => TurnEndReason::Budget,
                _ => TurnEndReason::Error,
            }
        };
        if reason == TurnEndReason::Error {
            let errors: Vec<&str> = v["errors"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            let message = if !errors.is_empty() {
                errors.join("; ")
            } else if let Some(text) = v["result"].as_str().filter(|t| !t.is_empty()) {
                text.to_string()
            } else {
                format!("claude ended the turn with `{subtype}`")
            };
            let from_provider = v.get("api_error").is_some_and(|a| !a.is_null())
                || (subtype == "success" && is_error);
            out.push(DelegateEvent::Error {
                kind: if from_provider {
                    ErrorKind::Provider
                } else {
                    ErrorKind::Internal
                },
                message,
            });
        }
        out.push(DelegateEvent::Finished {
            reason,
            detail: None,
        });
        Out {
            events: out,
            finished: true,
            close_stdin: true,
            ..Out::default()
        }
    }

    /// Per-model usage: what was since the last result when claude says, else the cumulative
    /// per-model figures, else the main loop's own `usage`.
    fn usage_events(&self, v: &Value) -> Vec<DelegateEvent> {
        let by_model = ["model_usage_since_last_result", "modelUsage"]
            .iter()
            .filter_map(|k| v[*k].as_object())
            .find(|m| !m.is_empty());
        if let Some(models) = by_model {
            return models
                .iter()
                .map(|(model, u)| DelegateEvent::Usage {
                    model: model.clone(),
                    usage: usage_of(
                        &u["inputTokens"],
                        &u["outputTokens"],
                        &u["cacheReadInputTokens"],
                        &u["cacheCreationInputTokens"],
                        &u["cacheCreationEphemeral1hInputTokens"],
                        &u["thinkingTokens"],
                    ),
                })
                .collect();
        }
        let u = &v["usage"];
        if u["input_tokens"].is_null() && u["output_tokens"].is_null() {
            return Vec::new();
        }
        vec![DelegateEvent::Usage {
            model: self.model.clone().unwrap_or_default(),
            usage: usage_of(
                &u["input_tokens"],
                &u["output_tokens"],
                &u["cache_read_input_tokens"],
                &u["cache_creation_input_tokens"],
                &Value::Null,
                &Value::Null,
            ),
        }]
    }

    fn rate_limit(&self, v: &Value) -> Out {
        let info = &v["rate_limit_info"];
        let observed_at = (self.params.clock)();
        let window = |minutes: u64, w: &Value, fallback_util: &Value| Window {
            window_minutes: Some(minutes),
            used_percent: w["utilization"]
                .as_f64()
                .or_else(|| fallback_util.as_f64())
                .map(percent),
            resets_at: w["resetsAt"].as_u64(),
            source: WindowSource::Stream,
        };
        let mut windows = Vec::new();
        if let Some(unified) = info["unifiedWindows"].as_object() {
            for (key, minutes) in [
                ("five_hour", 300),
                ("seven_day", 10_080),
                ("seven_day_overage_included", 10_080),
            ] {
                if let Some(w) = unified.get(key) {
                    windows.push(window(minutes, w, &Value::Null));
                }
            }
        }
        if windows.is_empty() {
            let minutes = match info["rateLimitType"].as_str() {
                Some("five_hour") => Some(300),
                Some(
                    "seven_day"
                    | "seven_day_opus"
                    | "seven_day_sonnet"
                    | "seven_day_overage_included",
                ) => Some(10_080),
                _ => None,
            };
            windows.push(Window {
                window_minutes: minutes,
                used_percent: info["utilization"].as_f64().map(percent),
                resets_at: info["resetsAt"].as_u64(),
                source: WindowSource::Stream,
            });
        }
        let status = match info["status"].as_str() {
            Some("allowed") => Some(LimitStatus::Allowed),
            Some("allowed_warning") => Some(LimitStatus::Warning),
            Some("rejected") => Some(LimitStatus::Rejected),
            _ => None,
        };
        events(vec![DelegateEvent::RateLimits {
            snapshot: WindowSnapshot {
                windows,
                observed_at,
            },
            status,
            plan: None,
        }])
    }

    fn next_request_id(&mut self) -> String {
        self.requests += 1;
        format!("harness_{}", self.requests)
    }
}

/// A fraction up to 1.0 is a share of the window; anything above is already a percentage.
fn percent(u: f64) -> f64 {
    if u <= 1.0 { u * 100.0 } else { u }
}

fn usage_of(
    input: &Value,
    output: &Value,
    cache_read: &Value,
    cache_write: &Value,
    cache_write_1h: &Value,
    thinking: &Value,
) -> Usage {
    let n = |v: &Value| v.as_u64().unwrap_or(0);
    Usage {
        // Everything the request sent: uncached, read from the cache, written to it.
        input_tokens: n(input) + n(cache_read) + n(cache_write),
        output_tokens: n(output),
        cached_tokens: n(cache_read),
        cache_write_tokens: n(cache_write),
        cache_write_1h_tokens: n(cache_write_1h),
        reasoning_tokens: n(thinking),
    }
}

fn tool_output(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .map(|b| match b["type"].as_str() {
                Some("text") => b["text"].as_str().unwrap_or_default().to_string(),
                Some(other) => format!("[{other}]"),
                None => String::new(),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn permission_response(request_id: &str, result: Value) -> String {
    line(json!({"type": "control_response", "response":
        {"subtype": "success", "request_id": request_id, "response": result}}))
}

fn error_response(request_id: &str, error: &str) -> String {
    line(json!({"type": "control_response", "response":
        {"subtype": "error", "request_id": request_id, "error": error}}))
}

impl Protocol for ClaudeProtocol {
    fn start(&mut self) -> Out {
        Out {
            writes: vec![line(json!({
                "type": "user",
                "session_id": "",
                "message": {"role": "user", "content": [{"type": "text", "text": self.params.task}]},
                "parent_tool_use_id": null,
            }))],
            ..Out::default()
        }
    }

    fn on_line(&mut self, text: &str) -> Out {
        let Ok(v) = serde_json::from_str::<Value>(text) else {
            return malformed(text);
        };
        if !v.is_object() {
            return malformed(text);
        }
        if self.session_id.is_none()
            && let Some(id) = v["session_id"].as_str().filter(|s| !s.is_empty())
        {
            self.session_id = Some(id.to_string());
        }
        match v["type"].as_str() {
            Some("system") => self.system(&v),
            Some("stream_event") => self.stream_event(&v),
            Some("assistant") => self.assistant(&v),
            Some("user") => self.user(&v),
            Some("control_request") => self.control_request(&v),
            Some("control_cancel_request") => {
                let id = v["request_id"].as_str().unwrap_or_default().to_string();
                if self.pending.remove(&id).is_some() {
                    self.withdrawn.insert(id.clone());
                    events(vec![DelegateEvent::ApprovalWithdrawn { id }])
                } else {
                    Out::default()
                }
            }
            Some("control_response") => {
                let response = &v["response"];
                if response["subtype"] == "error" {
                    events(vec![warning(format!(
                        "claude refused a request: {}",
                        response["error"].as_str().unwrap_or("no reason given")
                    ))])
                } else {
                    Out::default()
                }
            }
            Some("rate_limit_event") => self.rate_limit(&v),
            Some("result") => self.result(&v),
            _ => Out::default(),
        }
    }

    fn on_answer(&mut self, id: &str, answer: ApprovalAnswer) -> Result<Out, DelegateError> {
        if self.withdrawn.remove(id) {
            return Ok(Out::default());
        }
        let pending = self
            .pending
            .remove(id)
            .ok_or_else(|| DelegateError::UnknownApproval(id.to_string()))?;
        let mut out = Out::default();
        let result = match answer {
            ApprovalAnswer::AllowOnce => {
                json!({"behavior": "allow", "updatedInput": pending.input})
            }
            ApprovalAnswer::AllowForRun => {
                // Only what claude itself offers for this session: never a settings file.
                let session: Vec<&Value> = pending
                    .suggestions
                    .iter()
                    .filter(|s| s["destination"] == "session")
                    .collect();
                let mut allow = json!({"behavior": "allow", "updatedInput": pending.input});
                if !session.is_empty() {
                    allow["updatedPermissions"] = json!(session);
                }
                allow
            }
            ApprovalAnswer::Deny { reason } => json!({"behavior": "deny", "message":
                reason.unwrap_or_else(|| format!("The user denied this {} call.", pending.tool))}),
            ApprovalAnswer::Cancel => {
                self.interrupted = true;
                out.stop = true;
                json!({"behavior": "deny", "message": "The user cancelled the turn.", "interrupt": true})
            }
        };
        out.writes.push(permission_response(id, result));
        Ok(out)
    }

    fn on_interrupt(&mut self) -> Out {
        self.interrupted = true;
        let id = self.next_request_id();
        Out {
            writes: vec![line(json!({"type": "control_request", "request_id": id,
                "request": {"subtype": "interrupt"}}))],
            ..Out::default()
        }
    }

    fn session_id(&self) -> Option<String> {
        self.session_id.clone()
    }

    fn escalation(&self) -> &'static [Signal] {
        &[Signal::SIGINT, Signal::SIGTERM, Signal::SIGKILL]
    }
}

fn malformed(text: &str) -> Out {
    let shown: String = text.chars().take(120).collect();
    events(vec![warning(format!(
        "claude sent a line that is not JSON and it was ignored: {shown}"
    ))])
}
