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
    /// Network errors, HTTP 429, and HTTP 5xx are worth retrying; a 429 that reports an
    /// exhausted quota or plan limit is not, since waiting seconds does not end it.
    pub fn is_retryable(&self) -> bool {
        match self {
            ProviderError::Network(_) => true,
            ProviderError::Http { status: 429, .. } => !self.is_quota_exhausted(),
            ProviderError::Http { status, .. } => (500..600).contains(status),
            ProviderError::Protocol(_) | ProviderError::InStream(_) => false,
        }
    }

    /// Whether this is a 429 that reports an exhausted quota or plan limit: ChatGPT's
    /// `usage_limit_reached` and `usage_not_included`, or OpenAI's `insufficient_quota`.
    pub fn is_quota_exhausted(&self) -> bool {
        let ProviderError::Http {
            status: 429, body, ..
        } = self
        else {
            return false;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
            return false;
        };
        let error = &value["error"];
        [&error["type"], &error["code"]].iter().any(|v| {
            matches!(
                v.as_str(),
                Some("usage_limit_reached" | "usage_not_included" | "insufficient_quota")
            )
        })
    }

    /// When an exhausted limit resets, in seconds since the Unix epoch, if the provider said:
    /// `resets_at`, or `resets_in_seconds` from now.
    pub fn resets_at(&self) -> Option<u64> {
        let ProviderError::Http { body, .. } = self else {
            return None;
        };
        let value: serde_json::Value = serde_json::from_str(body).ok()?;
        let error = &value["error"];
        error["resets_at"].as_u64().or_else(|| {
            error["resets_in_seconds"]
                .as_u64()
                .map(|secs| crate::time::now_unix().saturating_add(secs))
        })
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
