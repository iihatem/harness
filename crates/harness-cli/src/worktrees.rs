//! The CLI's side of worktrees: where they live and how a session opens them.

use std::sync::Arc;

use harness_context::project::{project_key, project_root};
use harness_core::worktree::Worktrees;

use crate::setup::Setup;

/// The worktrees of the session's project, kept in `<data>/worktrees/<project>`; `None` when
/// the workspace is not in a git repository (agents then cannot be isolated).
pub fn open(setup: &Setup) -> Option<Arc<Worktrees>> {
    let home = setup
        .paths
        .data_dir
        .join("worktrees")
        .join(project_key(&project_root(&setup.workspace)));
    Worktrees::new(&setup.workspace, home).ok().map(Arc::new)
}
