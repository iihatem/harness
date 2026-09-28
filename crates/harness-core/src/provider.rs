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
    /// An error the provider reported inside a response stream.
    #[error("provider error: {0}")]
    InStream(String),
}

impl ProviderError {
    /// Network errors, HTTP 429, and HTTP 5xx are worth retrying.
    pub fn is_retryable(&self) -> bool {
        match self {
            ProviderError::Network(_) => true,
            ProviderError::Http { status, .. } => *status == 429 || (500..600).contains(status),
            ProviderError::Protocol(_) | ProviderError::InStream(_) => false,
        }
    }

    /// Whether the provider rejected the request as longer than the model's context window.
    /// Providers say so in different words, so this looks for the usual phrases, in an error
    /// response (HTTP 400, 413 or 422) or an error the provider reported inside the stream. Never
    /// in a response harness could not parse: that quotes what it received, which can be the
    /// model's own text.
    pub fn is_context_overflow(&self) -> bool {
        let text = match self {
            ProviderError::Http {
                status: 400 | 413 | 422,
                body,
                ..
            } => body,
            ProviderError::InStream(message) => message,
            _ => return false,
        };
        let text = text.to_lowercase();
        [
            "context_length_exceeded",
            "maximum context length",
            "context length",
            "context window",
            "exceeds the available context",
            "prompt is too long",
            "too many tokens",
            // xAI
            "maximum prompt length",
            // Gemini
            "exceeds the maximum number of tokens allowed",
            // Text Generation Inference, Together
            "`inputs` tokens + `max_new_tokens` must be",
            // Bedrock
            "input is too long",
            // Anthropic
            "exceed context limit",
        ]
        .iter()
        .any(|phrase| text.contains(phrase))
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
