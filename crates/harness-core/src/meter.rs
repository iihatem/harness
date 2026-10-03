//! What the runtime tells a usage meter: one record for every model request, however it ended.
//! The meter (`harness-usage`) keeps the ledger; the runtime knows nothing of where it goes.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::message::Usage;

/// The role every request runs for until model roles exist.
pub const MAIN_ROLE: &str = "main";

/// How a request is paid for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountKind {
    /// An API key: billed per token at the provider's price.
    ApiKey,
    /// A subscription (ChatGPT): no per-token bill.
    Subscription,
    /// A server of the user's own: free.
    Local,
}

impl AccountKind {
    /// The account that pays for `model_id` (`<provider>/<model>`), `local` saying whether it
    /// runs on a server of the user's own.
    pub fn of(model_id: &str, local: bool) -> AccountKind {
        if local {
            AccountKind::Local
        } else if model_id.split('/').next() == Some("chatgpt") {
            AccountKind::Subscription
        } else {
            AccountKind::ApiKey
        }
    }

    /// The name the ledger writes.
    pub fn as_str(self) -> &'static str {
        match self {
            AccountKind::ApiKey => "api_key",
            AccountKind::Subscription => "subscription",
            AccountKind::Local => "local",
        }
    }
}

/// One model request that ended: with a reply, or with an error, or because the user stopped it.
#[derive(Debug, Clone, PartialEq)]
pub struct RequestRecord {
    /// The session the request was made in.
    pub session: String,
    /// The role it ran for.
    pub role: String,
    /// `<provider>/<model>`.
    pub model: String,
    /// Whether the model runs on a server of the user's own.
    pub local: bool,
    /// What the provider reported; all zero for a request that ended without a report.
    pub usage: Usage,
    /// From sending the request to its end, retries and the waits between them included.
    pub duration: Duration,
    /// `ok`, or `error:<kind>` (see `ProviderError::kind`).
    pub outcome: String,
}

/// What the tokens avoided against a named baseline model, when there is one.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Avoided {
    /// Nothing to show: no baseline is named, or the request was paid for with an API key.
    NotApplicable,
    /// The baseline has no known price: shown as "price unknown", never as $0.
    Unknown,
    /// What the baseline would have charged for the tokens, in USD.
    Usd(f64),
}

/// What one request cost, in the three figures that are never added together.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RequestCost {
    pub account: AccountKind,
    /// What was billed: the table price on an API-key account, 0 on a subscription or a local
    /// one; `None` when the model has no price.
    pub billed_usd: Option<f64>,
    /// What the same tokens cost at the table's price (an estimate), 0 for a local model;
    /// `None` when the model has no price.
    pub list_usd: Option<f64>,
    pub avoided: Avoided,
}

/// Where the runtime reports what each model request took. Implemented by `harness-usage`.
pub trait Meter: Send + Sync {
    /// Records `request`, and says what it cost.
    fn record_request(&self, request: &RequestRecord) -> RequestCost;

    /// What went wrong keeping the record, such as a ledger that cannot be written, each given
    /// once; the runtime shows them as warnings.
    fn take_warnings(&self) -> Vec<String> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_account_follows_the_provider_and_whether_it_is_local() {
        assert_eq!(AccountKind::of("openai/gpt-5", false), AccountKind::ApiKey);
        assert_eq!(
            AccountKind::of("chatgpt/gpt-5", false),
            AccountKind::Subscription
        );
        assert_eq!(
            AccountKind::of("ollama/qwen3-coder", true),
            AccountKind::Local
        );
        assert_eq!(AccountKind::of("mine/x", true), AccountKind::Local);
    }
}
