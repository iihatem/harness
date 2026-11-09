//! `claude-code/<model>` and `codex/<model>`: models that run in the stock `claude` and `codex`
//! programs, driven by `harness-delegate`.
//!
//! [`registry::resolve`](crate::registry::resolve) returns a [`DelegatedProvider`] for them. It is
//! a [`Provider`](harness_core::provider::Provider) that sends no requests: the agent sees
//! [`Provider::delegated`](harness_core::provider::Provider::delegated) and runs the turn through
//! it, so every door a model id comes in by (`--model`, config, `/model`, roles, commands) works
//! without knowing about it. What it does:
//!
//! - [`events`]: what the child does, as the agent's own events;
//! - [`approve`]: what the child asks leave to do, put to harness's permission engine;
//! - [`optin`]: which account a `claude-code` run uses (the user's Claude subscription only after
//!   an opt-in, else their Anthropic API key), and the disclosure shown for the opt-in.

pub mod approve;
pub mod optin;
mod provider;

use std::{path::PathBuf, sync::Arc};

pub use provider::DelegatedProvider;

/// The provider names that run in another program.
pub const CLAUDE_CODE: &str = "claude-code";
pub const CODEX: &str = "codex";

/// Whether `provider` is one of them.
pub fn is_delegated(provider: &str) -> bool {
    provider == CLAUDE_CODE || provider == CODEX
}

/// Where the answer to the Claude subscription question is kept.
pub trait OptInStore: Send + Sync {
    /// What the user answered, if they did.
    fn get(&self) -> Option<bool>;
    /// Keeps the answer for every later session.
    fn set(&self, yes: bool) -> Result<(), String>;
}

/// What every model of a program shares while a question about it is asked.
#[derive(Default)]
pub struct OptInShared {
    /// Held while the opt-in is asked: children on any model of the program that ask at once ask
    /// it once, the others take the answer.
    pub(crate) asking: tokio::sync::Mutex<()>,
    /// What was answered this session, kept or not.
    pub(crate) answer: std::sync::Mutex<Option<bool>>,
}

/// What the registry needs to make a delegated provider, from the configuration and the
/// credential store.
#[derive(Clone, Default)]
pub struct DelegateSettings {
    /// `[delegate.claude] use_subscription`, or what the user answered the first time (the
    /// configuration wins): `None` until either says.
    pub claude_subscription: Option<bool>,
    /// Where an answer given at the first use is kept.
    pub store: Option<Arc<dyn OptInStore>>,
    /// `[delegate] allow_untested`: run a version outside the tested range.
    pub allow_untested: bool,
    /// The program to run instead of the one on `PATH`: for tests only (debug builds).
    pub claude_binary: Option<PathBuf>,
    pub codex_binary: Option<PathBuf>,
    /// The question's state, shared by every provider made from these settings (clones share it).
    pub shared: Arc<OptInShared>,
}

impl std::fmt::Debug for DelegateSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DelegateSettings")
            .field("claude_subscription", &self.claude_subscription)
            .field("allow_untested", &self.allow_untested)
            .finish()
    }
}
