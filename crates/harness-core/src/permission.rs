use std::{
    collections::VecDeque,
    fmt,
    path::{Component, Path, PathBuf},
    str::FromStr,
};

use serde::{Deserialize, Serialize};

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
    /// Modes that grant no more than `ask` does. Only these may come from an untrusted project config.
    pub fn is_narrow(self) -> bool {
        matches!(self, Mode::Plan | Mode::ReadOnly | Mode::Ask)
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
}

/// The P1 policy: mode-based decisions with no OS sandbox. Because no sandbox exists yet, every shell
/// command needs approval outside `full-access` (no silent unsandboxed fallback). P2 replaces this.
#[derive(Debug, Clone)]
pub struct BaselinePolicy {
    mode: Mode,
    workspace: PathBuf,
    read_dirs: Vec<PathBuf>,
}

impl BaselinePolicy {
    pub fn new(mode: Mode, workspace: &Path, read_dirs: Vec<PathBuf>) -> Self {
        let canonical = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        BaselinePolicy {
            mode,
            workspace: canonical(workspace),
            read_dirs: read_dirs.iter().map(|d| canonical(d)).collect(),
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }
}

impl PermissionPolicy for BaselinePolicy {
    fn check(&self, action: &Action) -> Decision {
        if self.mode == Mode::FullAccess {
            return Decision::Allow;
        }
        match action {
            Action::Read(path) => {
                let target = resolve_path(&self.workspace, path);
                if target.starts_with(&self.workspace)
                    || self.read_dirs.iter().any(|d| target.starts_with(d))
                {
                    Decision::Allow
                } else {
                    Decision::Ask(format!("read outside the workspace: {}", target.display()))
                }
            }
            Action::Write(path) => {
                if matches!(self.mode, Mode::Plan | Mode::ReadOnly) {
                    return Decision::Deny(format!(
                        "file writes are not allowed in {} mode",
                        self.mode
                    ));
                }
                let target = resolve_path(&self.workspace, path);
                if !target.starts_with(&self.workspace) {
                    Decision::Ask(format!("write outside the workspace: {}", target.display()))
                } else if self.mode == Mode::Auto {
                    Decision::Allow
                } else {
                    Decision::Ask(format!("write {}", target.display()))
                }
            }
            Action::Bash(command) => {
                Decision::Ask(format!("run `{command}` (no sandbox is available yet)"))
            }
        }
    }
}
