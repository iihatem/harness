//! Driving the stock `claude` and `codex` binaries as child processes, and turning what they
//! write into one harness-neutral stream of [`event::DelegateEvent`]s.
//!
//! Nothing here touches the agent loop, the terminal or the CLI: a caller starts a run with a
//! [`backend::DelegateBackend`], reads the events, answers approvals through the run's
//! [`backend::RunControl`], and maps the events onto its own (see the table in the crate's plan).
//! harness never reads the children's credentials and never runs them with a login; the children
//! sign in on their own (`claude /login`, `codex login`).

pub mod backend;
pub mod env;
pub mod event;
pub mod redact;

pub use backend::{
    BackendKind, DelegateBackend, DelegateError, DelegateRun, RunControl, StartRequest,
};
