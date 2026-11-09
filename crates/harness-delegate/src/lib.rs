//! Driving the stock `claude` and `codex` binaries as child processes, and turning what they
//! write into one harness-neutral stream of [`event::DelegateEvent`]s.
//!
//! Nothing here touches the agent loop, the terminal or the CLI: a caller starts a run with a
//! [`DelegateBackend`] ([`ClaudeBackend`] or [`CodexBackend`]), reads the events, answers
//! approvals through the run's [`RunControl`], and maps the events onto its own `AgentEvent`s.
//! harness never reads the children's credentials and never runs them with a login: they sign
//! in on their own (`claude /login`, `codex login`).
//!
//! # Contract
//!
//! - A run is one task. Its event stream ends after exactly one `Finished` (preceded by an
//!   `Error` when it failed), whatever happens to the child: a crash, a malformed line, an
//!   interrupt, a dropped consumer. Dropping the [`DelegateRun`] kills the child's process group.
//! - Every string a child relays passes the caller's `Redactor` ([`redact::EventFilter`]), except
//!   an approval's raw `arguments`, which the approver needs to work out what to show.
//! - harness's permission engine is the only approver: Claude is started with no settings
//!   sources and an empty rule set, Codex with the sandbox and approval policy of the harness
//!   mode ([`codex::protocol::policy`]). `full-access` needs `full_access_confirmed`.
//! - The environment is scrubbed by [`env::child_env`] for subscription or API-key mode.
//! - Versions outside [`detect::tested_range`] are refused unless the caller allows them.
//!
//! # What was not verified against a live run
//!
//! See the plan: the Claude stdio permission handshake (no `initialize` request is sent), the
//! shapes of Claude's assistant and stream events (hand-written fixtures), `--setting-sources=`
//! with an empty value, Claude's `rate_limit_event` fields, the Codex API-key variable, and
//! whether a user's own Codex execution-policy rules can approve a command before harness is
//! asked. The Codex messages are validated against the schema generated from codex-cli 0.154.0.

pub mod backend;
pub mod claude;
pub mod codex;
pub mod detect;
pub mod driver;
pub mod env;
pub mod event;
pub mod redact;

pub use backend::{
    BackendKind, DelegateBackend, DelegateError, DelegateRun, RunControl, StartRequest,
};
pub use claude::ClaudeBackend;
pub use codex::CodexBackend;
