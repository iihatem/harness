//! The environment a child gets: the caller's, minus every credential that is not the one the
//! run was asked to use. Subscription mode and API-key mode are explicit and exclusive, so a key
//! in the environment never silently bills an account the user believes is a subscription (or
//! the reverse), and no other provider's key reaches a program that talks to a different one.

use std::{
    ffi::{OsStr, OsString},
    fmt,
};

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

const CLAUDE_OWN: [&str; 6] = [
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
];

const CODEX_OWN: [&str; 3] = ["OPENAI_API_KEY", "CODEX_API_KEY", "OPENAI_BASE_URL"];

/// The variables `kind`'s child must not inherit: its own credential and routing variables (set
/// again from `auth` where the mode says so) and the other agent's.
fn removed(kind: BackendKind) -> impl Iterator<Item = &'static str> {
    let (own, other): (&[&str], &[&str]) = match kind {
        BackendKind::Claude => (&CLAUDE_OWN, &CODEX_OWN),
        BackendKind::Codex => (&CODEX_OWN, &CLAUDE_OWN),
    };
    own.iter().chain(other).chain(FOREIGN.iter()).copied()
}

/// The environment for `kind`'s child: `parent` without the variables above, plus the key for
/// [`AuthMode::ApiKey`] (`ANTHROPIC_API_KEY` for Claude, `OPENAI_API_KEY` for Codex: unverified
/// for Codex, see the plan).
pub fn child_env(
    kind: BackendKind,
    auth: &AuthMode,
    parent: impl IntoIterator<Item = (OsString, OsString)>,
) -> Vec<(OsString, OsString)> {
    let gone: Vec<&str> = removed(kind).collect();
    let mut env: Vec<(OsString, OsString)> = parent
        .into_iter()
        .filter(|(name, _)| !gone.iter().any(|g| OsStr::new(g) == name.as_os_str()))
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
