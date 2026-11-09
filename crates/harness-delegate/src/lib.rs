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
//!   mode ([`codex::protocol::policy`]) and the user's own MCP servers switched off.
//!   `full-access` needs `full_access_confirmed`.
//! - Project instructions: with no setting sources Claude Code likely does not read CLAUDE.md
//!   itself (static evidence from 2.1.312), so the caller passes the instructions harness
//!   already loads in [`StartRequest::instructions`]; they go to a private temporary file the
//!   run owns and removes (`--append-system-prompt-file`). If a later live check shows Claude
//!   does read them, they appear twice (tokens only). Codex reads its own AGENTS.md.
//! - The environment is scrubbed by [`env::child_env`] for subscription or API-key mode: by
//!   pattern (`ANTHROPIC_*`, `CLAUDE_CODE_*`, `OPENAI_*`, `CODEX_*` but the user's own
//!   `CODEX_HOME`-style settings), so a variable a newer release reads is covered too.
//! - Versions outside [`detect::tested_range`] are refused unless the caller allows them.
//!
//! # What was not verified against a live run
//!
//! See the plan: the Claude stdio permission handshake (no `initialize` request is sent), the
//! shapes of Claude's assistant and stream events (hand-written fixtures), CLAUDE.md loading
//! with `--setting-sources=` (and whether `apiProvider` appears in `system/init`), Claude's
//! `rate_limit_event` fields, the resume cost baseline, the Codex API-key variable, and whether
//! a user's own Codex execution-policy rules and hooks can act before harness is asked. The
//! Codex messages are validated against the schema generated from codex-cli 0.154.0, and one
//! real Codex run (`tests/fixtures/codex/ask-0.154.0.jsonl`) is replayed through the protocol.

pub mod backend;
pub mod claude;
pub mod codex;
pub mod detect;
pub mod driver;
pub mod env;
pub mod event;
pub mod models;
pub mod redact;

pub use backend::{
    BackendKind, DelegateBackend, DelegateError, DelegateRun, RunControl, StartRequest,
};
pub use claude::ClaudeBackend;
pub use codex::CodexBackend;
