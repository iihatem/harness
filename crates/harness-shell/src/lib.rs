//! Decides whether a model-requested shell command
//! may run without a prompt, needs approval, or is denied.
//!
//! The command is parsed with brush-parser and decomposed into the simple commands
//! it runs — across `;`, newlines, `&`, `&&`, `||`, pipes, subshells, brace groups
//! and command substitutions, and through wrappers such as `env`, `sudo`, `xargs`
//! or `bash -c` — before any rule is evaluated. Deny rules are matched against
//! every decomposed form; allow requires every sub-command to match; anything that
//! cannot be fully decomposed must be approved (after a best-effort deny scan).

mod argv;
mod bash32;
mod destructive;
mod fallback;
mod git;
mod matching;
mod parse;
mod paths;
mod verdict;
mod wrappers;

pub use matching::glob_match;
pub use verdict::{Rules, Verdict, evaluate, session_prefixes};
