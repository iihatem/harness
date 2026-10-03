use std::time::Duration;

use futures::{future::BoxFuture, stream::BoxStream};
use serde::{Deserialize, Serialize};

use crate::{
    message::{ChatRequest, ToolCall, Usage},
    meter::WindowSnapshot,
};

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
    /// The reply's first byte of any kind arrived: text, reasoning, or a tool call's name or
    /// arguments starting to stream. Emitted at most once per reply, before whatever caused it.
    /// A tool call itself is buffered and arrives whole only once the stream ends, so this is the
    /// only signal of when a tool-call reply actually started generating.
    OutputStarted,
    TextDelta(String),
    ReasoningDelta(String),
    ToolCall(ToolCall),
    Usage(Usage),
    /// The provider said where a subscription's usage windows stand (response headers, or an event
    /// in the stream).
    RateLimits(WindowSnapshot),
    Finished(FinishReason),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProviderError {
    #[error("network error: {0}")]
    Network(String),
    /// No data came within the wait for a reply's first data: 300 s for a hosted provider, 30
    /// minutes for a local server (`local`), which may load the model and read a long prompt on a
    /// CPU first.
    #[error("{message}")]
    NoStart { message: String, local: bool },
    #[error("HTTP {status}: {body}")]
    Http {
        status: u16,
        body: String,
        retry_after: Option<Duration>,
    },
    #[error("invalid provider response: {0}")]
    Protocol(String),
    /// An error the provider reported inside a response stream that stands for an HTTP error:
    /// it is treated as `status`, retried or not as that status would be, though no such status
    /// was received.
    #[error("the provider reported {}: {body}", reported(*.status, .body))]
    Reported {
        status: u16,
        body: String,
        retry_after: Option<Duration>,
    },
    /// An API key the provider refused (HTTP 401 or 403), with which key that was (the
    /// variable, or the stored profile) and how to replace it.
    #[error("HTTP {status}: {body}; {hint}")]
    KeyRefused {
        status: u16,
        body: String,
        hint: String,
    },
    /// An error the provider reported inside a response stream.
    #[error("provider error: {0}")]
    InStream(String),
}

impl ProviderError {
    /// Network errors, HTTP 429, and HTTP 5xx are worth retrying; a 429 that reports an
    /// exhausted quota or plan limit is not, since waiting seconds does not end it. Nor is a local
    /// server that did not start its reply within its wait: a retry would start over what it was
    /// doing (loading the model, reading the prompt), and wait as long again.
    pub fn is_retryable(&self) -> bool {
        match self {
            ProviderError::Network(_) => true,
            ProviderError::NoStart { local, .. } => !local,
            ProviderError::Http { status: 429, .. }
            | ProviderError::Reported { status: 429, .. } => {
                !self.is_quota_exhausted() && !self.is_spend_cap()
            }
            ProviderError::Http { status, .. } | ProviderError::Reported { status, .. } => {
                (500..600).contains(status)
            }
            ProviderError::Protocol(_)
            | ProviderError::InStream(_)
            | ProviderError::KeyRefused { .. } => false,
        }
    }

    /// Whether this is a 429 that reports an exhausted quota or plan limit: ChatGPT's
    /// `usage_limit_reached` and `usage_not_included`, or OpenAI's `insufficient_quota`, and the
    /// credit and spend limits Codex counts as quotas too.
    pub fn is_quota_exhausted(&self) -> bool {
        match self {
            ProviderError::Http {
                status: 429, body, ..
            }
            | ProviderError::Reported {
                status: 429, body, ..
            } => reports_quota(body),
            _ => false,
        }
    }

    /// Whether this is a 429 that reports a spend cap the account set (Anthropic's
    /// `enforced_spend_limit_reached`, which comes without `retry-after`): waiting does not end
    /// it, and it is not the subscription limit that a fallback or a resume answers.
    pub fn is_spend_cap(&self) -> bool {
        matches!(
            self,
            ProviderError::Http {
                status: 429, body, ..
            } | ProviderError::Reported {
                status: 429, body, ..
            } if body.contains("enforced_spend_limit_reached")
        )
    }

    /// When an exhausted limit resets, in seconds since the Unix epoch, if the provider said:
    /// `resets_at`, or `resets_in_seconds` from now.
    pub fn resets_at(&self) -> Option<u64> {
        let (ProviderError::Http { body, .. } | ProviderError::Reported { body, .. }) = self else {
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

    /// What kind of failure this is, in the word the usage ledger records after `error:`:
    /// `unavailable` (a 5xx, or a network error), `rate_limited`, `quota`, `spend_cap`, `auth`,
    /// `context_overflow`, `rejected` (another 4xx) or `protocol`.
    /// Whether a configured fallback chain may answer this failure: a rate limit, an exhausted
    /// quota or window, an overload, or an unavailable provider (the retries are over by the time
    /// this is asked). Never an authentication error, another 4xx, a context overflow or a spend
    /// cap.
    pub fn is_fallback_trigger(&self) -> bool {
        match self.kind() {
            "quota" | "rate_limited" | "unavailable" => true,
            _ => {
                matches!(self, ProviderError::InStream(message) if message.to_lowercase().contains("overload"))
            }
        }
    }

    pub fn kind(&self) -> &'static str {
        if self.is_spend_cap() {
            return "spend_cap";
        }
        if self.is_quota_exhausted() {
            return "quota";
        }
        if self.is_context_overflow() {
            return "context_overflow";
        }
        match self {
            ProviderError::Network(_) | ProviderError::NoStart { .. } => "unavailable",
            ProviderError::Http { status, .. } | ProviderError::Reported { status, .. } => {
                match status {
                    429 => "rate_limited",
                    401 | 403 => "auth",
                    500..=599 => "unavailable",
                    _ => "rejected",
                }
            }
            ProviderError::KeyRefused { .. } => "auth",
            ProviderError::Protocol(_) | ProviderError::InStream(_) => "protocol",
        }
    }

    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            ProviderError::Http { retry_after, .. }
            | ProviderError::Reported { retry_after, .. } => *retry_after,
            _ => None,
        }
    }
}

/// Whether an error response's `body` reports an exhausted quota or plan limit.
fn reports_quota(body: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return false;
    };
    let error = &value["error"];
    [&error["type"], &error["code"]].iter().any(|v| {
        matches!(
            v.as_str(),
            Some(
                "usage_limit_reached"
                    | "usage_not_included"
                    | "insufficient_quota"
                    | "credit_balance_exhausted"
                    | "organization_spend_limit_exceeded"
                    | "project_spend_limit_exceeded"
            )
        )
    })
}

/// What an error reported in a stream and treated as HTTP `status`, with `body`, stands for.
fn reported(status: u16, body: &str) -> &'static str {
    match status {
        429 if reports_quota(body) => "a usage limit",
        429 => "a rate limit",
        503 | 529 => "an overload",
        _ => "a server error",
    }
}

pub type ProviderStream = BoxStream<'static, Result<ProviderEvent, ProviderError>>;

/// A model backend: translates a [`ChatRequest`] to a wire protocol and streams events back.
pub trait Provider: Send + Sync {
    fn stream(&self, request: ChatRequest) -> ProviderStream;

    /// Asks the provider where the account's usage windows stand, when it has any (a ChatGPT
    /// subscription's); `None` for a provider with none. Called on demand, never on a timer.
    fn windows(&self) -> Option<BoxFuture<'static, Result<WindowSnapshot, String>>> {
        None
    }
}
