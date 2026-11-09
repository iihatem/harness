//! The events a delegated run produces. Each variant is shaped to map directly onto the
//! `AgentEvent` of the same name (see the plan's table); the turn's start is the caller's own.

use harness_core::{
    event::{ErrorKind, TurnEndReason},
    message::Usage,
    meter::WindowSnapshot,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What a tool call changed in a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileChange {
    Add,
    Update,
    Delete,
}

/// How a subscription's limit stands, as the child says it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitStatus {
    Allowed,
    /// Near the limit.
    Warning,
    /// At the limit: requests are refused until the window resets.
    Rejected,
}

/// What the child wants to do.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DelegateApprovalKind {
    Command {
        command: String,
        cwd: Option<String>,
    },
    FileChange {
        paths: Vec<String>,
    },
    /// Any other tool call: its name and `arguments` say what it does.
    Tool,
}

/// An approval the child waits for. Answer it with [`crate::RunControl::answer`] using `id`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DelegateApproval {
    /// The id to answer with.
    pub id: String,
    /// The tool call it is about.
    pub call_id: String,
    pub tool: String,
    /// The call's arguments as the child sent them, **not redacted**: what an approver shows of
    /// the change is worked out from them, and only what it shows is redacted. (This is how
    /// `harness_core::ApprovalRequest::arguments` works too.)
    pub arguments: Value,
    pub kind: DelegateApprovalKind,
    pub reason: String,
    /// Whether [`ApprovalAnswer::AllowForRun`] is available for it.
    pub can_remember: bool,
}

/// The answer to an approval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalAnswer {
    AllowOnce,
    /// Allow it, and the same kind of action, for the rest of this run.
    AllowForRun,
    Deny {
        reason: Option<String>,
    },
    /// Deny it and stop the turn.
    Cancel,
}

/// Everything observable about a delegated run. The stream ends with exactly one
/// [`Finished`](DelegateEvent::Finished), after an [`Error`](DelegateEvent::Error) when the run
/// failed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DelegateEvent {
    /// The child is up: its session id (for resume), its model and its version.
    Started {
        session_id: String,
        model: Option<String>,
        child_version: String,
    },
    TextDelta {
        text: String,
    },
    ReasoningDelta {
        text: String,
    },
    AssistantMessage {
        content: String,
        model: Option<String>,
    },
    ToolCallRequested {
        id: String,
        name: String,
        arguments: String,
    },
    ApprovalNeeded(DelegateApproval),
    ToolCallFinished {
        id: String,
        output: String,
        is_error: bool,
    },
    /// A tool call changed a file (it succeeded).
    FileChanged {
        call_id: String,
        path: String,
        change: FileChange,
    },
    Usage {
        model: String,
        usage: Usage,
    },
    /// What the run cost, **estimated** by the child: never a bill. `session_total_usd` is the
    /// child's own running total for the session; `estimated_usd` is what this run added to it.
    Cost {
        estimated_usd: f64,
        session_total_usd: f64,
    },
    /// Where the account's usage windows stand. `status` is what the child says about the limit
    /// itself when it says anything; `plan` is the account's plan name where it is reported.
    RateLimits {
        snapshot: WindowSnapshot,
        status: Option<LimitStatus>,
        plan: Option<String>,
    },
    Retrying {
        attempt: u32,
        reason: String,
        delay_ms: u64,
    },
    Warning {
        message: String,
    },
    Finished {
        reason: TurnEndReason,
        detail: Option<String>,
    },
    Error {
        kind: ErrorKind,
        message: String,
    },
}
