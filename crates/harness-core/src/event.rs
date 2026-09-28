use serde::{Deserialize, Serialize};

use crate::message::Usage;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnEndReason {
    Completed,
    StepLimit,
    Interrupted,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    Provider,
    Internal,
}

/// Everything observable about a turn. Frontends render these; `harness ask --json` prints one per line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    TurnStarted,
    TextDelta {
        text: String,
    },
    ReasoningDelta {
        text: String,
    },
    AssistantMessage {
        content: String,
        model: String,
    },
    ToolCallRequested {
        id: String,
        name: String,
        arguments: String,
    },
    ApprovalNeeded {
        id: String,
        reason: String,
    },
    ActionBlocked {
        id: String,
        reason: String,
    },
    ToolCallFinished {
        id: String,
        output: String,
        is_error: bool,
    },
    Usage {
        model: String,
        usage: Usage,
    },
    Retrying {
        attempt: u32,
        reason: String,
        delay_ms: u64,
    },
    /// Something the user should know that did not stop the turn.
    Warning {
        message: String,
    },
    /// The workspace was snapshotted before the turn's first change.
    CheckpointCreated {
        commit: String,
    },
    /// The older part of the conversation was replaced by `summary`.
    Compacted {
        summary: String,
        tokens_before: u64,
        tokens_after: u64,
    },
    TurnFinished {
        reason: TurnEndReason,
    },
    Error {
        kind: ErrorKind,
        message: String,
    },
}
