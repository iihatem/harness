use std::{
    collections::VecDeque,
    fmt,
    path::{Component, Path, PathBuf},
    str::FromStr,
};

use serde::{Deserialize, Serialize};

use crate::engine::RuleSet;

/// Approval mode. See the permissions-sandbox spec for the exact semantics of each.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    Plan,
    ReadOnly,
    Ask,
    Auto,
    FullAccess,
}

impl Mode {
    /// Whether this mode lets the agent do no more than `other` does. Plan and read-only rank
    /// lowest (and equal), then ask, auto, and full-access.
    pub fn grants_at_most(self, other: Mode) -> bool {
        self.rank() <= other.rank()
    }

    fn rank(self) -> u8 {
        match self {
            Mode::Plan | Mode::ReadOnly => 0,
            Mode::Ask => 1,
            Mode::Auto => 2,
            Mode::FullAccess => 3,
        }
    }

    /// What a sandboxed shell command may write in this mode.
    pub fn fs_access(self) -> FsAccess {
        match self {
            Mode::Plan | Mode::ReadOnly => FsAccess::ReadOnly,
            Mode::Ask | Mode::Auto | Mode::FullAccess => FsAccess::WorkspaceWrite,
        }
    }
}

impl FromStr for Mode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "plan" => Ok(Mode::Plan),
            "read-only" => Ok(Mode::ReadOnly),
            "ask" => Ok(Mode::Ask),
            "auto" => Ok(Mode::Auto),
            "full-access" => Ok(Mode::FullAccess),
            other => Err(format!(
                "unknown mode `{other}` (expected plan, read-only, ask, auto, or full-access)"
            )),
        }
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Mode::Plan => "plan",
            Mode::ReadOnly => "read-only",
            Mode::Ask => "ask",
            Mode::Auto => "auto",
            Mode::FullAccess => "full-access",
        })
    }
}

/// What a tool call is about to do, as seen by the permission policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Read(PathBuf),
    Write(PathBuf),
    Bash(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Ask(String),
    Deny(String),
}

/// Filesystem access given to a sandboxed shell command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsAccess {
    ReadOnly,
    WorkspaceWrite,
}

/// Resolves `path` (absolute, or relative to `workspace`) to an absolute path. Symlinks are followed
/// component by component (including dangling symlinks and chains), so the result always reflects where
/// the OS would navigate to (including `link/..` following the real target). Symlink loops are detected
/// with a hop limit of 40; components beyond that are applied lexically.
pub fn resolve_path(workspace: &Path, path: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace.join(path)
    };

    let mut out = PathBuf::new();
    let mut pending: VecDeque<PathBuf> = joined
        .components()
        .map(|c| {
            let mut p = PathBuf::new();
            p.push(c.as_os_str());
            p
        })
        .collect();
    let mut hop_count = 0;
    const HOP_LIMIT: usize = 40;

    while let Some(component_path) = pending.pop_front() {
        let component = component_path.components().next().unwrap();
        match component {
            Component::Prefix(_) | Component::RootDir => {
                out.push(component.as_os_str());
            }
            Component::CurDir => {
                // . is a no-op
            }
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(name) => {
                out.push(name);

                // Check if this is a symlink; if so, follow it
                if hop_count < HOP_LIMIT
                    && let Ok(metadata) = std::fs::symlink_metadata(&out)
                    && metadata.file_type().is_symlink()
                {
                    hop_count += 1;
                    if let Ok(target) = std::fs::read_link(&out) {
                        out.pop();
                        if target.is_absolute() {
                            out.clear();
                        }
                        // Create target component paths and push to front of queue in reverse order
                        let target_comps: Vec<_> = target
                            .components()
                            .map(|c| {
                                let mut p = PathBuf::new();
                                p.push(c.as_os_str());
                                p
                            })
                            .collect();
                        for comp in target_comps.iter().rev() {
                            pending.push_front(comp.clone());
                        }
                    }
                }
            }
        }
    }

    out
}

/// Decides whether an action may run, must be approved, or is refused.
pub trait PermissionPolicy: Send + Sync {
    fn check(&self, action: &Action) -> Decision;

    /// Records that the user approved `action` for the rest of the session. Returns whether anything
    /// was remembered (destructive commands never are).
    fn remember(&self, _action: &Action) -> bool {
        false
    }

    /// Whether [`remember`](Self::remember) would remember `action` now, without remembering it.
    fn can_remember(&self, _action: &Action) -> bool {
        false
    }

    /// Switches the approval mode for later checks. Policies without modes ignore it.
    ///
    /// Internal to the agent: frontends call `Agent::set_mode`, which also gives shell commands
    /// the new mode's sandbox access and records the change in the conversation. Calling this
    /// directly would leave commands running with the old mode's access.
    fn set_mode(&self, _mode: Mode) {}

    /// Whether shell commands run in an OS sandbox from now on. Internal to the agent, like
    /// [`set_mode`](Self::set_mode), which it goes with.
    fn set_sandbox_available(&self, _available: bool) {}

    /// Adds rules for the current turn only, or with `None` removes them. They never override
    /// deny rules, destructive-command confirmation or the sandbox.
    fn set_turn_rules(&self, _rules: Option<RuleSet>) {}
}
