//! The environment a child gets: the caller's, minus every credential that is not the one the
//! run was asked to use. Subscription mode and API-key mode are explicit and exclusive, so a key
//! in the environment never silently bills an account the user believes is a subscription (or
//! the reverse), and no other provider's key reaches a program that talks to a different one.

use std::{ffi::OsString, fmt};

use crate::BackendKind;

/// A key held in memory for a child's environment. Never shown by `Debug`.
#[derive(Clone)]
pub struct ApiKey(String);

impl ApiKey {
    pub fn new(key: impl Into<String>) -> ApiKey {
        ApiKey(key.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiKey(..)")
    }
}

/// How the child authenticates.
#[derive(Debug, Clone)]
pub enum AuthMode {
    /// The user's own login with the child (its subscription): no key variable reaches it.
    Subscription,
    /// The caller's API key, passed in the child's environment only (never on a command line).
    ApiKey(ApiKey),
}

/// Variables of other providers, removed whatever the mode: a delegate talks to one provider.
const FOREIGN: [&str; 14] = [
    "OPENROUTER_API_KEY",
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "MISTRAL_API_KEY",
    "GROQ_API_KEY",
    "XAI_API_KEY",
    "DEEPSEEK_API_KEY",
    "AZURE_OPENAI_API_KEY",
    "TOGETHER_API_KEY",
    "FIREWORKS_API_KEY",
    "COHERE_API_KEY",
    "PERPLEXITY_API_KEY",
    "CEREBRAS_API_KEY",
    "HF_TOKEN",
];

/// Whether `kind`'s child must not inherit `name`: a credential, a route to another provider or
/// account, or a variable of the other agent. Matched by prefix as well as by name, so a variable
/// a newer release reads is covered before this list knows it (a deny-list by pattern, not an
/// allow-list: a child needs the user's proxy, certificate, `git` and `ssh` settings, which no
/// fixed list could name). harness may itself run inside Claude Code, whose environment holds
/// `ANTHROPIC_BASE_URL`, `CLAUDECODE` and many `CLAUDE_CODE_*` names: none of them may steer a
/// delegate.
fn is_removed(name: &str) -> bool {
    if FOREIGN.contains(&name) {
        return true;
    }
    let claude_side = name.starts_with("ANTHROPIC_")
        || name.starts_with("CLAUDE_CODE_")
        || name == "CLAUDECODE"
        || name == "AWS_BEARER_TOKEN_BEDROCK";
    // The user's own Codex settings (where its home is, a certificate), not credentials.
    let codex_settings = ["CODEX_HOME", "CODEX_SQLITE_HOME", "CODEX_CA_CERTIFICATE"];
    let codex_side = name.starts_with("OPENAI_")
        || (name.starts_with("CODEX_") && !codex_settings.contains(&name));
    // Both agents' variables go from both children: each child loses its own credentials (set
    // again from `auth` where the mode says so) and the other agent's.
    claude_side || codex_side
}

/// The environment for `kind`'s child: `parent` without the variables above, plus the key for
/// [`AuthMode::ApiKey`] (`ANTHROPIC_API_KEY` for Claude, `OPENAI_API_KEY` for Codex: unverified
/// for Codex, see the plan).
pub fn child_env(
    kind: BackendKind,
    auth: &AuthMode,
    parent: impl IntoIterator<Item = (OsString, OsString)>,
) -> Vec<(OsString, OsString)> {
    let mut env: Vec<(OsString, OsString)> = parent
        .into_iter()
        .filter(|(name, _)| !name.to_str().is_some_and(is_removed))
        .collect();
    if let AuthMode::ApiKey(key) = auth {
        let name = match kind {
            BackendKind::Claude => "ANTHROPIC_API_KEY",
            BackendKind::Codex => "OPENAI_API_KEY",
        };
        env.push((name.into(), key.expose().into()));
    }
    env
}
