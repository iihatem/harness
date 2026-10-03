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

/// How long after it was observed a window snapshot is shown as stale, in seconds.
pub const STALE_AFTER_SECS: u64 = 15 * 60;

/// Where a window's figures were read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowSource {
    /// The response headers of a request.
    Header,
    /// An event in the reply's stream.
    Stream,
    /// A call to the usage endpoint.
    Poll,
}

/// One usage window of a subscription. Every field is optional: a provider says what it says.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Window {
    pub window_minutes: Option<u64>,
    /// 0 to 100.
    pub used_percent: Option<f64>,
    /// When it resets, in seconds since the Unix epoch.
    pub resets_at: Option<u64>,
    pub source: WindowSource,
}

impl Window {
    /// The window's length as a name, never its position: `5h`, `7d`, `1d`, `90m`; `window` when
    /// the length is not known.
    pub fn label(&self) -> String {
        match self.window_minutes {
            Some(m) if m >= 1_440 && m % 1_440 == 0 => format!("{}d", m / 1_440),
            Some(m) if m >= 60 && m % 60 == 0 => format!("{}h", m / 60),
            Some(m) => format!("{m}m"),
            None => "window".to_string(),
        }
    }
}

/// The windows a provider reported at one time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowSnapshot {
    pub windows: Vec<Window>,
    /// When it was observed, in seconds since the Unix epoch.
    pub observed_at: u64,
}

impl WindowSnapshot {
    /// Whether it was observed more than 15 minutes before `now`.
    pub fn is_stale(&self, now: u64) -> bool {
        now.saturating_sub(self.observed_at) > STALE_AFTER_SECS
    }

    /// The window with the highest used percentage; a window with no percentage is not a
    /// candidate, since an unknown window is never shown as 0%.
    pub fn most_used(&self) -> Option<&Window> {
        self.windows
            .iter()
            .filter(|w| w.used_percent.is_some())
            .max_by(|a, b| {
                a.used_percent
                    .partial_cmp(&b.used_percent)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    }

    /// A name for this snapshot that follows its windows, not when they were seen, so a snapshot
    /// that repeats unchanged is written once: the ledger refers to it by this.
    pub fn id(&self) -> String {
        use sha2::{Digest, Sha256};
        let json = serde_json::to_string(&self.windows).unwrap_or_default();
        let digest = Sha256::digest(json.as_bytes());
        format!("w{}", hex::encode(&digest[..8]))
    }
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

/// Which money budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetKind {
    Session,
    Daily,
    Monthly,
}

impl BudgetKind {
    /// The word for it: `session`, `daily` or `monthly`.
    pub fn name(self) -> &'static str {
        match self {
            BudgetKind::Session => "session",
            BudgetKind::Daily => "daily",
            BudgetKind::Monthly => "monthly",
        }
    }

    /// The `[budgets]` key that sets it.
    pub fn config_key(self) -> &'static str {
        match self {
            BudgetKind::Session => "session_usd",
            BudgetKind::Daily => "daily_usd",
            BudgetKind::Monthly => "monthly_usd",
        }
    }
}

/// A budget, what it allows and what has been spent against it (billed cost only).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BudgetNotice {
    pub budget: BudgetKind,
    pub spent_usd: f64,
    pub limit_usd: f64,
}

impl BudgetNotice {
    /// How to raise the budget.
    fn raise_hint(&self) -> String {
        match self.budget {
            BudgetKind::Session => format!(
                "raise it with /budget <usd> for this session, or set budgets.{} in the configuration",
                self.budget.config_key()
            ),
            _ => format!(
                "raise it by setting budgets.{} in the configuration",
                self.budget.config_key()
            ),
        }
    }

    /// What to tell the user when 80% of the budget is spent.
    pub fn warning_message(&self) -> String {
        let percent = (self.spent_usd / self.limit_usd * 100.0 + 1e-9).floor();
        format!(
            "{percent:.0}% of the {} budget of ${:.2} is spent (${:.2}); requests go on until it is reached",
            self.budget.name(),
            self.limit_usd,
            self.spent_usd
        )
    }

    /// What to tell the user when the budget is reached: which one, and how to raise it.
    pub fn reached_message(&self) -> String {
        let raise = self.raise_hint();
        format!(
            "the {} budget of ${:.2} is reached (${:.2} spent), so the request was not sent; {raise}",
            self.budget.name(),
            self.limit_usd,
            self.spent_usd
        )
    }
}

impl BudgetNotice {
    /// What to tell the user when the budget is reached but the request is not billed: paid
    /// models are paused, and this one goes on.
    pub fn paused_message(&self) -> String {
        format!(
            "the {} budget of ${:.2} is reached (${:.2} spent): requests on an API key are paused, \
             while ChatGPT plans and local models go on; {}",
            self.budget.name(),
            self.limit_usd,
            self.spent_usd,
            self.raise_hint()
        )
    }
}

/// What the budgets say before a request: the ones at 80% (each said once), and the one that is
/// reached, if any. A reached budget refuses only a request that would add billed cost: for any
/// other account it is `paused` instead of `stop`, and the request goes on.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BudgetStatus {
    pub warnings: Vec<BudgetNotice>,
    /// Reached, and the request is on an API key: it is not sent.
    pub stop: Option<BudgetNotice>,
    /// Reached, but the request is on a subscription or a local model: it is sent.
    pub paused: Option<BudgetNotice>,
}

/// How the gates of a turn ended (counts only).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateCounts {
    pub passed: u32,
    pub failed: u32,
    pub skipped: u32,
}

/// How a Build turn got the conversation, for its outcome record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffRecord {
    /// `same_model`, `history` or `plan_only`.
    pub kind: String,
    /// `[roles.handoff] mode` chose it.
    pub forced: bool,
}

/// One finished turn: how it went, as counts and ids; never the prompt, the reply, paths or
/// commands.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnRecord {
    pub session: String,
    /// The turn's user message, as the entry that holds it in the session.
    pub turn: String,
    pub role: String,
    /// The model that answered last.
    pub model: String,
    /// What chose the model: `config`, `user`, `fallback` or `escalation`.
    pub selected_by: String,
    /// The tokens the provider reported for the turn's requests.
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// From the request to the first output, of the turn's first reply that had any.
    pub first_token_ms: Option<u64>,
    pub duration_ms: u64,
    pub tool_calls: u32,
    pub invalid_calls: u32,
    pub retries: u32,
    /// `completed`, `step_limit`, `interrupted`, `error`, `budget` or `gate_failed`.
    pub finish_reason: String,
    /// When the turn started and ended, in seconds since the Unix epoch: where the turn's
    /// requests are in the ledger.
    pub started_at: u64,
    pub ended_at: u64,
    pub gates: GateCounts,
    /// For a Build turn, how it got the conversation; `None` for any other turn.
    pub handoff: Option<HandoffRecord>,
}

/// Where the runtime reports what each model request took. Implemented by `harness-usage`.
pub trait Meter: Send + Sync {
    /// Records `request`, and says what it cost.
    fn record_request(&self, request: &RequestRecord) -> RequestCost;

    /// Asked before each model request in `session`, which `account` pays for: whether a money
    /// budget allows it. A reached budget refuses only an `ApiKey` request. The meter says each
    /// 80% warning once.
    fn check_budget(&self, _session: &str, _account: AccountKind) -> BudgetStatus {
        BudgetStatus::default()
    }

    /// A window snapshot the provider reported during a request that has not ended yet: the
    /// meter keeps it, and names it in that request's record.
    fn record_window(&self, _snapshot: &WindowSnapshot) {}

    /// Records a finished turn.
    fn record_turn(&self, _turn: &TurnRecord) {}

    /// The user rewound the conversation to before these turns of `session` (each named by its
    /// user message's entry).
    fn turns_rewound(&self, _session: &str, _turns: &[String]) {}

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
