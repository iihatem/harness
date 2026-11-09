//! What harness keeps about each worktree it made, in a JSON file next to it
//! (`<data>/worktrees/<project>/<name>.json`): enough to list it after a restart, review it, and
//! to check that cleanup is about one of harness's own.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One worktree harness made.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    /// The directory name, and the part of the branch after `harness/`.
    pub name: String,
    pub branch: String,
    /// The worktree's root.
    pub path: PathBuf,
    /// The commit it was made from (the local HEAD then).
    pub base: String,
    /// The agent's id in the session that made it.
    pub agent: String,
    pub label: String,
    pub task: String,
    #[serde(default)]
    pub port_offset: u16,
    /// The harness process that made it (and holds its lock while the agent runs).
    #[serde(default)]
    pub pid: u32,
    /// When it was made, in seconds since the epoch.
    #[serde(default)]
    pub created: u64,
}

impl Record {
    pub fn new(
        name: &str,
        branch: &str,
        path: &Path,
        base: &str,
        agent: &str,
        label: &str,
        task: &str,
    ) -> Record {
        Record {
            name: name.into(),
            branch: branch.into(),
            path: path.to_path_buf(),
            base: base.into(),
            agent: agent.into(),
            label: label.into(),
            task: task.into(),
            port_offset: 0,
            pid: std::process::id(),
            created: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
        }
    }
}
