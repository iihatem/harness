//! A restore that empties a workspace in a repository's subdirectory, where the process runs.
//! Its own test binary: it changes the process's working directory.

use std::process::Command;

use harness_core::checkpoint::Checkpoints;

// Re-review E, V2: git removes the workspace when it empties it; the directory comes back, and so
// does the process's working directory, which was the removed one.
#[test]
fn the_working_directory_survives_a_restore_that_empties_the_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let repo = base.join("repo");
    let ws = repo.join("a/b");
    std::fs::create_dir_all(&ws).unwrap();
    let init = Command::new("git")
        .args(["init", "-q"])
        .current_dir(&repo)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .unwrap();
    assert!(init.success());
    std::env::set_current_dir(&ws).unwrap();
    let checkpoints = Checkpoints::open(&base.join("data/p.git"), &ws, "s1").unwrap();
    let first = checkpoints.snapshot("turn 1").unwrap();
    std::fs::write(ws.join("new.txt"), "agent\n").unwrap();
    checkpoints.restore(&first).unwrap();
    assert_eq!(std::env::current_dir().unwrap(), ws);
    // Relative paths resolve there again.
    std::fs::write("relative.txt", "here\n").unwrap();
    assert!(ws.join("relative.txt").is_file());
    std::env::set_current_dir(&base).unwrap();
}
