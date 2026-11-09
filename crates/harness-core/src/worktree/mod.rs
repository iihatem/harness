//! Git worktrees for agents that edit: made, locked, committed, reviewed and removed by harness.
//!
//! Every git command here is run by harness itself, outside any sandbox, with the repository's
//! own code switched off ([`git`]).

pub mod git;
pub mod testing;
