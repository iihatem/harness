use serde::{Deserialize, Serialize};

use crate::{
    message::Usage,
    meter::{RequestCost, WindowSnapshot},
};

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
    /// What a model request cost, right after it ended: billed, estimated at list price, and
    /// (with a baseline named) avoided.
    Metered {
        model: String,
        cost: RequestCost,
    },
    /// Where a subscription's usage windows stand, as the provider just said.
    RateLimits {
        snapshot: WindowSnapshot,
    },
    Retrying {
        attempt: u32,
        reason: String,
        delay_ms: u64,
    },
    /// Input the user sent while the turn ran, given to the model with the tool results just
    /// sent.
    Steered {
        text: String,
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
    /// How the turn went, just before it finishes, when it called the model: the model that
    /// answered last, how long its first output took, and the tokens the provider reported.
    TurnStats {
        model: String,
        /// From the request to the first output, of the turn's first reply that had any.
        time_to_first_token_ms: Option<u64>,
        /// Time spent streaming output: from each reply's first output to its end.
        generation_ms: u64,
        input_tokens: u64,
        output_tokens: u64,
        cached_tokens: u64,
    },
    TurnFinished {
        reason: TurnEndReason,
    },
    Error {
        kind: ErrorKind,
        message: String,
    },
}
