//! Which account a `claude-code` run uses, and the question asked before the user's Claude
//! subscription is used. Codex needs none of it: it uses its own sign-in (C13).

use harness_delegate::env::ApiKey;

/// The question shown the first time a `claude-code` model would run on a subscription: short
/// and plain, and the choice is the user's (U7).
pub const DISCLOSURE: &str = "Use your Claude subscription for claude-code models? harness would start the claude program you signed in to, with your own login; it never reads that program's tokens. Anthropic allows signing in to its official claude program. Whether another tool driving it counts as allowed is not settled, so use of a subscription this way could be limited or stopped by Anthropic. Answer no to use your Anthropic API key instead, billed per token. The answer is kept for every session; [delegate.claude] use_subscription in the global config.toml overrides it.";

/// What a run is allowed to authenticate with.
#[derive(Debug, Clone)]
pub enum Auth {
    /// The program's own sign-in (Codex always; Claude after an opt-in).
    Own,
    /// The user's Anthropic API key, billed per token.
    Key(ApiKey),
    /// Not answered yet: asked at the first turn. The key is what an answer of "no" falls back to.
    Undecided { key: Option<ApiKey> },
    /// Cannot be used, and why.
    Unusable(String),
}

/// Why `claude-code` cannot run with neither an opt-in nor a key.
pub const NO_ACCOUNT: &str = "claude-code needs an account to run on: set [delegate.claude] use_subscription = true in the global config.toml to use your Claude subscription (through the claude program you signed in to), or add an Anthropic API key with `harness auth add anthropic` (or set ANTHROPIC_API_KEY)";

/// Why it cannot run where the opt-in was refused and there is no key.
pub const REFUSED_NO_KEY: &str = "claude-code is not allowed to use your Claude subscription (the opt-in was declined) and there is no Anthropic API key: add one with `harness auth add anthropic` (or set ANTHROPIC_API_KEY), or set [delegate.claude] use_subscription = true in the global config.toml";

/// The account for `claude-code` from the opt-in, if it was answered, and the API key, if there
/// is one.
pub fn claude_auth(opt_in: Option<bool>, key: Option<ApiKey>) -> Auth {
    match (opt_in, key) {
        (Some(true), _) => Auth::Own,
        (Some(false), Some(key)) => Auth::Key(key),
        (Some(false), None) => Auth::Unusable(REFUSED_NO_KEY.into()),
        (None, key) => Auth::Undecided { key },
    }
}
