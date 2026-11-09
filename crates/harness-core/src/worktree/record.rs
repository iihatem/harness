//! What harness keeps about each worktree it made, in a JSON file next to it
//! (`<data>/worktrees/<project>/<name>.json`): enough to list it after a restart, review it, and
//! to check that cleanup is about one of harness's own.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::review::Outcome;
use crate::agents::AgentStatus;

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
    /// How its agent is doing; kept up to date while it runs, so that after a restart the
    /// listing says how it ended.
    #[serde(default = "working")]
    pub status: AgentStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The agent's own session, with its whole transcript.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default)]
    pub tool_calls: u32,
    /// It was started to run in the background.
    #[serde(default)]
    pub background: bool,
    /// What it left, committed on its branch; `None` while it runs or when it changed nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<Outcome>,
}

fn working() -> AgentStatus {
    AgentStatus::Working
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
            status: AgentStatus::Working,
            summary: None,
            error: None,
            session: None,
            model: None,
            tool_calls: 0,
            background: false,
            outcome: None,
        }
    }
}
