//! Git older than 2.31 ignores the `GIT_CONFIG_COUNT` settings every harness git command relies
//! on to switch a repository's hooks and drivers off, so worktrees are refused there. This file
//! holds one test: it changes `PATH` for its whole process, before git is first run.

use std::os::unix::fs::PermissionsExt;

use harness_core::worktree::{WorktreeError, Worktrees};

#[test]
fn git_before_2_31_refuses_worktrees_and_says_which_version() {
    let bin = tempfile::tempdir().unwrap();
    let fake = bin.path().join("git");
    std::fs::write(&fake, "#!/bin/sh\necho 'git version 2.30.2'\n").unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let old = std::env::var_os("PATH").unwrap_or_default();
    let mut path = std::ffi::OsString::from(bin.path());
    path.push(":");
    path.push(old);
    // SAFETY: the only test of this process, nothing else reads the environment meanwhile.
    unsafe { std::env::set_var("PATH", path) };

    let repo = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let err = Worktrees::new(repo.path(), home.path().join("p"))
        .err()
        .expect("refused");
    assert!(matches!(err, WorktreeError::Refused(_)), "{err:?}");
    let text = err.to_string();
    assert!(text.contains("2.30.2") && text.contains("2.31"), "{text}");
}
