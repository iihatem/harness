//! Where git keeps its metadata in a workspace. Platform-neutral: the macOS
//! Seatbelt profile uses `linked_gitdirs` for a symlinked or gitfile `.git`,
//! and the Linux sandbox uses [`discover`] for the guard and the mounts, with
//! the [`IgnoreRules`] it reads once per session ([`read_ignore_rules`]).

mod index;
mod linked;
mod read;

pub use index::{GitIndex, IgnoreRules, discover, read_ignore_rules};
#[cfg(target_os = "macos")]
pub(crate) use linked::{LinkedGitdirs, linked_gitdirs};

/// The entries in every gitdir that decide where git loads config and hooks
/// from, or that hold them: the set the macOS profile protects
/// (`GITDIR_FILES` in `macos/profile.rs`).
pub const GITDIR_PROTECTED: [&str; 6] = [
    "config",
    "config.worktree",
    "commondir",
    "hooks",
    "gitweb",
    "pid",
];

/// The entries at the top of the workspace that are protected: harness's
/// project settings, and a `HEAD` that would make the workspace look like a
/// bare repository to git.
pub const WORKSPACE_PROTECTED: [&str; 2] = [".harness", "HEAD"];
