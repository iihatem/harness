//! JSON-RPC over `codex app-server` (stdio, one JSON object per line, no `jsonrpc` field), as a
//! state machine. Method names, field names and decision values come from the schema generated
//! by `codex app-server generate-json-schema` for codex-cli 0.154.0 (`schema/codex-0.154.0/`),
//! which the tests validate every message against.
//!
//! The exchange: `initialize`, `initialized`, then `account/rateLimits/read` and `thread/start`
//! (or `thread/resume`) together; when the thread is up, `turn/start`. Approvals are requests of
//! the server (`item/commandExecution/requestApproval`, `item/fileChange/requestApproval`)
//! answered with a `decision`; every other request of the server is answered with an error,
//! never with an acceptance.

use std::collections::{HashMap, HashSet};

use harness_core::{
    event::{ErrorKind, TurnEndReason},
    message::Usage,
    meter::{Window, WindowSnapshot, WindowSource},
    permission::Mode,
};
use serde_json::{Value, json};

use crate::{
    DelegateError,
    driver::{Out, Protocol},
    event::{
        ApprovalAnswer, DelegateApproval, DelegateApprovalKind, DelegateEvent, FileChange,
        LimitStatus,
    },
};

/// The sandbox and approval policy a harness mode maps to (the table in the plan).
#[derive(Debug, Clone, PartialEq)]
pub struct Policy {
    /// `AskForApproval`.
    pub approval: &'static str,
    /// `SandboxMode`, for `thread/start`.
    pub sandbox: &'static str,
    /// `SandboxPolicy`, for `turn/start`.
    pub sandbox_policy: Value,
}

/// The policy for `mode`. `full-access` needs the caller's confirmation. Network is off in every
/// sandbox; `extra_dirs` are writable roots where the sandbox is `workspaceWrite`.
pub fn policy(
    mode: Mode,
    full_access_confirmed: bool,
    extra_dirs: &[String],
) -> Result<Policy, DelegateError> {
    Ok(match mode {
        Mode::Plan => Policy {
            approval: "on-request",
            sandbox: "read-only",
            sandbox_policy: json!({"type": "readOnly", "networkAccess": false}),
        },
        Mode::ReadOnly => Policy {
            approval: "untrusted",
            sandbox: "read-only",
            sandbox_policy: json!({"type": "readOnly", "networkAccess": false}),
        },
        Mode::Ask | Mode::Auto => {
            let mut sandbox_policy = json!({"type": "workspaceWrite", "networkAccess": false});
            if !extra_dirs.is_empty() {
                sandbox_policy["writableRoots"] = json!(extra_dirs);
            }
            Policy {
                approval: if mode == Mode::Ask {
                    "untrusted"
                } else {
                    "on-request"
                },
                sandbox: "workspace-write",
                sandbox_policy,
            }
        }
        Mode::FullAccess => {
            if !full_access_confirmed {
                return Err(DelegateError::FullAccessUnconfirmed);
            }
            Policy {
                approval: "never",
                sandbox: "danger-full-access",
                sandbox_policy: json!({"type": "dangerFullAccess"}),
            }
        }
    })
}

#[derive(Debug, Clone)]
pub struct CodexParams {
    pub task: String,
    /// The working directory of the thread and the turn.
    pub cwd: String,
    /// The version `--version` printed.
    pub child_version: String,
    pub mode: Mode,
    pub full_access_confirmed: bool,
    pub model: Option<String>,
    /// A thread id to continue.
    pub resume: Option<String>,
    pub extra_dirs: Vec<String>,
    /// Seconds since the Unix epoch, for the time a window was observed.
    pub clock: fn() -> u64,
}

impl CodexParams {
    pub fn new(
        task: impl Into<String>,
        cwd: impl Into<String>,
        child_version: impl Into<String>,
    ) -> CodexParams {
        CodexParams {
            task: task.into(),
            cwd: cwd.into(),
            child_version: child_version.into(),
            mode: Mode::Ask,
            full_access_confirmed: false,
            model: None,
            resume: None,
            extra_dirs: Vec::new(),
            clock: unix_now,
        }
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// What a request of ours was, by id, so its answer is understood.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Asked {
    Initialize,
    RateLimits,
    Thread,
    Turn,
    Interrupt,
}

struct PendingApproval {
    /// The JSON-RPC id to answer with, as the server sent it (a number or a string).
    id: Value,
    can_remember: bool,
}

pub struct CodexProtocol {
    params: CodexParams,
    policy: Policy,
    next_id: u64,
    asked: HashMap<u64, Asked>,
    thread_id: Option<String>,
    turn_id: Option<String>,
    model: Option<String>,
    interrupted: bool,
    interrupt_sent: bool,
    pending: HashMap<String, PendingApproval>,
    withdrawn: HashSet<String>,
    /// File changes announced by item id, for the approval that names no paths.
    changes: HashMap<String, Vec<(String, FileChange)>>,
    streamed_text: HashSet<String>,
    streamed_reasoning: HashSet<String>,
    usage: Option<Usage>,
    last_total: Option<u64>,
    retries: u32,
    last_error: Option<String>,
}

fn warning(message: String) -> DelegateEvent {
    DelegateEvent::Warning { message }
}

fn events(events: Vec<DelegateEvent>) -> Out {
    Out {
        events,
        ..Out::default()
    }
}

fn text(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_string()
}

/// A JSON-RPC id as the key an approval is answered by.
fn key_of(id: &Value) -> String {
    id.as_str().map_or_else(|| id.to_string(), String::from)
}

impl CodexProtocol {
    pub fn new(params: CodexParams) -> Result<CodexProtocol, DelegateError> {
        let policy = policy(
            params.mode,
            params.full_access_confirmed,
            &params.extra_dirs,
        )?;
        Ok(CodexProtocol {
            params,
            policy,
            next_id: 0,
            asked: HashMap::new(),
            thread_id: None,
            turn_id: None,
            model: None,
            interrupted: false,
            interrupt_sent: false,
            pending: HashMap::new(),
            withdrawn: HashSet::new(),
            changes: HashMap::new(),
            streamed_text: HashSet::new(),
            streamed_reasoning: HashSet::new(),
            usage: None,
            last_total: None,
            retries: 0,
            last_error: None,
        })
    }

    fn request(&mut self, method: &str, params: Option<Value>, what: Asked) -> String {
        self.next_id += 1;
        self.asked.insert(self.next_id, what);
        let mut message = json!({"id": self.next_id, "method": method});
        if let Some(params) = params {
            message["params"] = params;
        }
        message.to_string()
    }

    fn fail(&self, message: String) -> Out {
        Out {
            events: vec![
                DelegateEvent::Error {
                    kind: ErrorKind::Provider,
                    message,
                },
                DelegateEvent::Finished {
                    reason: TurnEndReason::Error,
                    detail: None,
                },
            ],
            finished: true,
            stop: true,
            close_stdin: true,
            ..Out::default()
        }
    }

    fn interrupt_line(&mut self) -> Option<String> {
        let (thread, turn) = (self.thread_id.clone()?, self.turn_id.clone()?);
        self.interrupt_sent = true;
        Some(self.request(
            "turn/interrupt",
            Some(json!({"threadId": thread, "turnId": turn})),
            Asked::Interrupt,
        ))
    }

    // --- responses to our requests ----------------------------------------------------------

    fn response(&mut self, id: u64, v: &Value) -> Out {
        let Some(what) = self.asked.remove(&id) else {
            return Out::default();
        };
        if let Some(error) = v.get("error") {
            let message = error["message"].as_str().unwrap_or("no message");
            return match what {
                Asked::RateLimits => Out::default(),
                Asked::Interrupt => events(vec![warning(format!(
                    "codex refused the interrupt: {message}"
                ))]),
                _ => self.fail(format!("codex refused a request: {message}")),
            };
        }
        let result = &v["result"];
        match what {
            Asked::Initialize => {
                let rate = self.request("account/rateLimits/read", None, Asked::RateLimits);
                let start = self.thread_request();
                Out {
                    writes: vec![json!({"method": "initialized"}).to_string(), rate, start],
                    ..Out::default()
                }
            }
            Asked::RateLimits => match result.get("rateLimits") {
                Some(snapshot) if snapshot.is_object() => {
                    events(vec![self.rate_event(snapshot, WindowSource::Poll)])
                }
                _ => Out::default(),
            },
            Asked::Thread => self.thread_started(result),
            Asked::Turn => {
                self.turn_id = result["turn"]["id"].as_str().map(String::from);
                let mut out = Out::default();
                if self.interrupted
                    && !self.interrupt_sent
                    && let Some(line) = self.interrupt_line()
                {
                    out.writes.push(line);
                }
                out
            }
            Asked::Interrupt => Out::default(),
        }
    }

    fn thread_request(&mut self) -> String {
        let p = &self.params;
        let mut params = json!({
            "cwd": p.cwd,
            "approvalPolicy": self.policy.approval,
            "sandbox": self.policy.sandbox,
        });
        if let Some(model) = &p.model {
            params["model"] = json!(model);
        }
        let method = match &p.resume {
            Some(thread) => {
                params["threadId"] = json!(thread);
                "thread/resume"
            }
            None => "thread/start",
        };
        self.request(method, Some(params), Asked::Thread)
    }

    fn thread_started(&mut self, result: &Value) -> Out {
        let Some(thread) = result["thread"]["id"].as_str() else {
            return self.fail("codex started a thread without an id".into());
        };
        self.thread_id = Some(thread.to_string());
        self.model = result["model"].as_str().map(String::from);
        let started = DelegateEvent::Started {
            session_id: thread.to_string(),
            model: self.model.clone(),
            child_version: self.params.child_version.clone(),
        };
        if self.interrupted {
            // Interrupted before there was a turn: there is nothing to stop.
            return Out {
                events: vec![
                    started,
                    DelegateEvent::Finished {
                        reason: TurnEndReason::Interrupted,
                        detail: None,
                    },
                ],
                finished: true,
                close_stdin: true,
                ..Out::default()
            };
        }
        let p = &self.params;
        let mut params = json!({
            "threadId": thread,
            "input": [{"type": "text", "text": p.task}],
            "cwd": p.cwd,
            "approvalPolicy": self.policy.approval,
            "sandboxPolicy": self.policy.sandbox_policy,
        });
        if let Some(model) = &p.model {
            params["model"] = json!(model);
        }
        let line = self.request("turn/start", Some(params), Asked::Turn);
        Out {
            events: vec![started],
            writes: vec![line],
            ..Out::default()
        }
    }

    // --- requests of the server ---------------------------------------------------------------

    fn server_request(&mut self, id: &Value, method: &str, p: &Value) -> Out {
        match method {
            "item/commandExecution/requestApproval" => {
                let command = text(&p["command"]);
                let cwd = p["cwd"].as_str().map(String::from);
                let reason = match (
                    p["reason"].as_str(),
                    p["networkApprovalContext"].is_object(),
                ) {
                    (Some(reason), _) if !reason.is_empty() => reason.to_string(),
                    (_, true) => "codex wants network access".to_string(),
                    _ => "codex wants to run a command".to_string(),
                };
                let offered = p["availableDecisions"].as_array();
                let can_remember =
                    offered.is_none_or(|d| d.iter().any(|x| x == "acceptForSession"));
                self.approval(
                    id,
                    DelegateApproval {
                        id: key_of(id),
                        call_id: text(&p["itemId"]),
                        tool: "command".into(),
                        arguments: json!({"command": command, "cwd": cwd, "reason": p["reason"]}),
                        kind: DelegateApprovalKind::Command { command, cwd },
                        reason,
                        can_remember,
                    },
                )
            }
            "item/fileChange/requestApproval" => {
                let item = text(&p["itemId"]);
                let changes = self.changes.get(&item).cloned().unwrap_or_default();
                let paths: Vec<String> = changes.iter().map(|(path, _)| path.clone()).collect();
                let reason = p["reason"]
                    .as_str()
                    .filter(|r| !r.is_empty())
                    .map_or_else(|| "codex wants to change files".to_string(), String::from);
                self.approval(
                    id,
                    DelegateApproval {
                        id: key_of(id),
                        call_id: item,
                        tool: "file_change".into(),
                        arguments: json!({"paths": paths, "grantRoot": p["grantRoot"]}),
                        kind: DelegateApprovalKind::FileChange { paths },
                        reason,
                        can_remember: true,
                    },
                )
            }
            other => {
                let advice = if other == "account/chatgptAuthTokens/refresh" {
                    "codex asked harness for fresh sign-in tokens, which harness never holds: run `codex login` in a terminal".to_string()
                } else {
                    format!("codex sent a `{other}` request, which harness does not handle")
                };
                Out {
                    events: vec![warning(advice)],
                    writes: vec![
                        json!({"id": id, "error": {"code": -32601,
                            "message": format!("harness does not handle {other}")}})
                        .to_string(),
                    ],
                    ..Out::default()
                }
            }
        }
    }

    fn approval(&mut self, id: &Value, approval: DelegateApproval) -> Out {
        self.pending.insert(
            key_of(id),
            PendingApproval {
                id: id.clone(),
                can_remember: approval.can_remember,
            },
        );
        events(vec![DelegateEvent::ApprovalNeeded(approval)])
    }

    // --- notifications --------------------------------------------------------------------------

    fn notification(&mut self, method: &str, p: &Value) -> Out {
        match method {
            "item/agentMessage/delta" => {
                self.streamed_text.insert(text(&p["itemId"]));
                events(vec![DelegateEvent::TextDelta {
                    text: text(&p["delta"]),
                }])
            }
            "item/reasoning/textDelta" | "item/reasoning/summaryTextDelta" => {
                self.streamed_reasoning.insert(text(&p["itemId"]));
                events(vec![DelegateEvent::ReasoningDelta {
                    text: text(&p["delta"]),
                }])
            }
            "item/started" => self.item_started(&p["item"]),
            "item/completed" => self.item_completed(&p["item"]),
            "item/fileChange/patchUpdated" => {
                self.changes
                    .insert(text(&p["itemId"]), file_changes(&p["changes"]));
                Out::default()
            }
            "thread/tokenUsage/updated" => {
                self.token_usage(&p["tokenUsage"]);
                Out::default()
            }
            "account/rateLimits/updated" => events(vec![
                self.rate_event(&p["rateLimits"], WindowSource::Stream),
            ]),
            "serverRequest/resolved" => {
                let key = key_of(&p["requestId"]);
                if self.pending.remove(&key).is_some() {
                    self.withdrawn.insert(key.clone());
                    events(vec![DelegateEvent::ApprovalWithdrawn { id: key }])
                } else {
                    Out::default()
                }
            }
            "turn/completed" => self.turn_completed(&p["turn"]),
            "error" => {
                let message = text(&p["error"]["message"]);
                if p["willRetry"].as_bool().unwrap_or(false) {
                    self.retries += 1;
                    events(vec![DelegateEvent::Retrying {
                        attempt: self.retries,
                        reason: message,
                        delay_ms: 0,
                    }])
                } else {
                    self.last_error = Some(message);
                    Out::default()
                }
            }
            "warning" => events(vec![warning(text(&p["message"]))]),
            "model/rerouted" => events(vec![warning(format!(
                "codex switched from {} to {}",
                text(&p["fromModel"]),
                text(&p["toModel"])
            ))]),
            _ => Out::default(),
        }
    }

    fn item_started(&mut self, item: &Value) -> Out {
        let id = text(&item["id"]);
        let (name, arguments) = match item["type"].as_str() {
            Some("commandExecution") => (
                "command".to_string(),
                json!({"command": item["command"], "cwd": item["cwd"]}).to_string(),
            ),
            Some("fileChange") => {
                let changes = file_changes(&item["changes"]);
                let shown: Vec<Value> = changes
                    .iter()
                    .map(|(path, kind)| json!({"path": path, "kind": kind}))
                    .collect();
                self.changes.insert(id.clone(), changes);
                (
                    "file_change".to_string(),
                    json!({"changes": shown}).to_string(),
                )
            }
            Some("mcpToolCall") => (
                format!("{}/{}", text(&item["server"]), text(&item["tool"])),
                item["arguments"].to_string(),
            ),
            Some("dynamicToolCall") => (text(&item["tool"]), item["arguments"].to_string()),
            Some("webSearch") => (
                "web_search".to_string(),
                json!({"query": item["query"]}).to_string(),
            ),
            _ => return Out::default(),
        };
        events(vec![DelegateEvent::ToolCallRequested {
            id,
            name,
            arguments,
        }])
    }

    fn item_completed(&mut self, item: &Value) -> Out {
        let id = text(&item["id"]);
        let status = item["status"].as_str().unwrap_or_default();
        let bad = matches!(status, "failed" | "declined");
        match item["type"].as_str() {
            Some("agentMessage") => {
                let content = text(&item["text"]);
                let mut out = Vec::new();
                if !self.streamed_text.remove(&id) {
                    out.push(DelegateEvent::TextDelta {
                        text: content.clone(),
                    });
                }
                out.push(DelegateEvent::AssistantMessage {
                    content,
                    model: self.model.clone(),
                });
                events(out)
            }
            Some("reasoning") => {
                if self.streamed_reasoning.remove(&id) {
                    return Out::default();
                }
                let parts: Vec<&str> = ["summary", "content"]
                    .iter()
                    .flat_map(|k| item[*k].as_array().into_iter().flatten())
                    .filter_map(Value::as_str)
                    .collect();
                if parts.is_empty() {
                    Out::default()
                } else {
                    events(vec![DelegateEvent::ReasoningDelta {
                        text: parts.join("\n"),
                    }])
                }
            }
            Some("commandExecution") => {
                let failed = bad || item["exitCode"].as_i64().is_some_and(|c| c != 0);
                events(vec![DelegateEvent::ToolCallFinished {
                    id,
                    output: item["aggregatedOutput"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                    is_error: failed,
                }])
            }
            Some("fileChange") => {
                let changes = self
                    .changes
                    .remove(&id)
                    .filter(|c| !c.is_empty())
                    .unwrap_or_else(|| file_changes(&item["changes"]));
                let mut out = vec![DelegateEvent::ToolCallFinished {
                    id: id.clone(),
                    output: format!("{status}: {} file(s)", changes.len()),
                    is_error: bad,
                }];
                if status == "completed" {
                    out.extend(changes.into_iter().map(|(path, change)| {
                        DelegateEvent::FileChanged {
                            call_id: id.clone(),
                            path,
                            change,
                        }
                    }));
                }
                events(out)
            }
            Some("mcpToolCall") => {
                let output = match item["error"]["message"].as_str() {
                    Some(message) => message.to_string(),
                    None => item["result"]["content"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|b| b["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n"),
                };
                events(vec![DelegateEvent::ToolCallFinished {
                    id,
                    output,
                    is_error: bad,
                }])
            }
            Some("dynamicToolCall") => events(vec![DelegateEvent::ToolCallFinished {
                id,
                output: String::new(),
                is_error: bad || item["success"] == false,
            }]),
            Some("webSearch") => events(vec![DelegateEvent::ToolCallFinished {
                id,
                output: String::new(),
                is_error: false,
            }]),
            _ => Out::default(),
        }
    }

    /// Codex reports the thread's running total and the last request's figures; a repeated
    /// update (the total did not grow) is not a new request.
    fn token_usage(&mut self, usage: &Value) {
        let total = usage["total"]["totalTokens"].as_u64().unwrap_or(0);
        if self.last_total.is_some_and(|seen| total <= seen) {
            return;
        }
        self.last_total = Some(total);
        let last = &usage["last"];
        let n = |k: &str| last[k].as_u64().unwrap_or(0);
        let acc = self.usage.get_or_insert_with(Usage::default);
        acc.input_tokens += n("inputTokens");
        acc.cached_tokens += n("cachedInputTokens");
        acc.output_tokens += n("outputTokens");
        acc.reasoning_tokens += n("reasoningOutputTokens");
        acc.cache_write_tokens += n("cacheWriteInputTokens");
    }

    fn rate_event(&self, snapshot: &Value, source: WindowSource) -> DelegateEvent {
        let window = |w: &Value| Window {
            window_minutes: w["windowDurationMins"].as_u64(),
            used_percent: w["usedPercent"].as_f64(),
            resets_at: w["resetsAt"].as_u64(),
            source,
        };
        let windows: Vec<Window> = ["primary", "secondary"]
            .iter()
            .filter(|k| snapshot[**k].is_object())
            .map(|k| window(&snapshot[*k]))
            .collect();
        let reached =
            !snapshot["rateLimitReachedType"].is_null() || snapshot["spendControlReached"] == true;
        DelegateEvent::RateLimits {
            snapshot: WindowSnapshot {
                windows,
                observed_at: (self.params.clock)(),
            },
            status: Some(if reached {
                LimitStatus::Rejected
            } else {
                LimitStatus::Allowed
            }),
            plan: snapshot["planType"].as_str().map(String::from),
        }
    }

    fn turn_completed(&mut self, turn: &Value) -> Out {
        if let (Some(ours), Some(theirs)) = (&self.turn_id, turn["id"].as_str())
            && ours != theirs
        {
            return Out::default();
        }
        let mut out = Vec::new();
        if let Some(usage) = self.usage.take() {
            out.push(DelegateEvent::Usage {
                model: self.model.clone().unwrap_or_default(),
                usage,
            });
        }
        let reason = match turn["status"].as_str() {
            Some("completed") => TurnEndReason::Completed,
            Some("interrupted") => TurnEndReason::Interrupted,
            _ => TurnEndReason::Error,
        };
        if reason == TurnEndReason::Error {
            let message = turn["error"]["message"]
                .as_str()
                .map(String::from)
                .or_else(|| self.last_error.take())
                .unwrap_or_else(|| "codex ended the turn with an error".to_string());
            out.push(DelegateEvent::Error {
                kind: ErrorKind::Provider,
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
}

fn file_changes(changes: &Value) -> Vec<(String, FileChange)> {
    changes
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| {
            let kind = match c["kind"]["type"].as_str() {
                Some("add") => FileChange::Add,
                Some("delete") => FileChange::Delete,
                _ => FileChange::Update,
            };
            (text(&c["path"]), kind)
        })
        .collect()
}

impl Protocol for CodexProtocol {
    fn start(&mut self) -> Out {
        let line = self.request(
            "initialize",
            Some(json!({
                "clientInfo": {"name": "harness", "title": "harness", "version": env!("CARGO_PKG_VERSION")},
                "capabilities": {"experimentalApi": false, "optOutNotificationMethods": [
                    "item/commandExecution/outputDelta",
                    "item/fileChange/outputDelta",
                    "item/mcpToolCall/progress",
                    "item/commandExecution/terminalInteraction",
                ]},
            })),
            Asked::Initialize,
        );
        Out {
            writes: vec![line],
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
        match (v.get("method").and_then(Value::as_str), v.get("id")) {
            (Some(method), Some(id)) => {
                let id = id.clone();
                self.server_request(&id, method, &v["params"])
            }
            (Some(method), None) => self.notification(method, &v["params"]),
            (None, Some(id)) => match id.as_u64() {
                Some(id) => self.response(id, &v),
                None => Out::default(),
            },
            (None, None) => Out::default(),
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
        let decision = match answer {
            ApprovalAnswer::AllowOnce => "accept",
            // Only when codex offered it; otherwise the caller's own rule store remembers.
            ApprovalAnswer::AllowForRun if pending.can_remember => "acceptForSession",
            ApprovalAnswer::AllowForRun => "accept",
            ApprovalAnswer::Deny { .. } => "decline",
            ApprovalAnswer::Cancel => "cancel",
        };
        out.writes
            .push(json!({"id": pending.id, "result": {"decision": decision}}).to_string());
        if decision == "cancel" {
            self.interrupted = true;
            out.stop = true;
            if !self.interrupt_sent
                && let Some(line) = self.interrupt_line()
            {
                out.writes.push(line);
            }
        }
        Ok(out)
    }

    fn on_interrupt(&mut self) -> Out {
        self.interrupted = true;
        let mut out = Out::default();
        if !self.interrupt_sent
            && let Some(line) = self.interrupt_line()
        {
            out.writes.push(line);
        }
        out
    }

    fn session_id(&self) -> Option<String> {
        self.thread_id.clone()
    }
}

fn malformed(text: &str) -> Out {
    let shown: String = text.chars().take(120).collect();
    events(vec![warning(format!(
        "codex sent a line that is not JSON and it was ignored: {shown}"
    ))])
}
