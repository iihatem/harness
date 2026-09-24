use std::time::Duration;

use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};

use crate::message::{ChatRequest, ToolCall, Usage};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    Stop,
    ToolCalls,
    Length,
    Other(String),
}

/// One item of a provider's streamed reply. Tool calls arrive complete, never as fragments.
#[derive(Debug, Clone, PartialEq)]
pub enum ProviderEvent {
    TextDelta(String),
    ReasoningDelta(String),
    ToolCall(ToolCall),
    Usage(Usage),
    Finished(FinishReason),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProviderError {
    #[error("network error: {0}")]
    Network(String),
    #[error("HTTP {status}: {body}")]
    Http {
        status: u16,
        body: String,
        retry_after: Option<Duration>,
    },
    #[error("invalid provider response: {0}")]
    Protocol(String),
}

impl ProviderError {
    /// Network errors, HTTP 429, and HTTP 5xx are worth retrying.
    pub fn is_retryable(&self) -> bool {
        match self {
            ProviderError::Network(_) => true,
            ProviderError::Http { status, .. } => *status == 429 || (500..600).contains(status),
            ProviderError::Protocol(_) => false,
        }
    }

    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            ProviderError::Http { retry_after, .. } => *retry_after,
            _ => None,
        }
    }
}

pub type ProviderStream = BoxStream<'static, Result<ProviderEvent, ProviderError>>;

/// A model backend: translates a [`ChatRequest`] to a wire protocol and streams events back.
pub trait Provider: Send + Sync {
    fn stream(&self, request: ChatRequest) -> ProviderStream;
}
