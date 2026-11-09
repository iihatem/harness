//! Real git repositories for tests, made with the system `git` (no network). Always compiled,
//! like [`crate::testing`], so tests of other crates can use it too.

use std::{path::Path, process::Command};

/// Runs `git args` in `dir` with a fixed identity and no system or global configuration, and
/// returns what it printed. Panics when git fails.
pub fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Test User")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test User")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// A repository in `dir` (which exists) on branch `main`, with one commit holding `README.md`
/// and `src/lib.rs`, and the identity `Test User <test@example.com>` in its own configuration.
pub fn init_repo(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.name", "Test User"]);
    git(dir, &["config", "user.email", "test@example.com"]);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("README.md"), "hello\n").unwrap();
    std::fs::write(dir.join("src/lib.rs"), "pub fn one() -> u32 {\n    1\n}\n").unwrap();
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", "initial"]);
}

/// Writes `text` to `path` below `dir`, making the directories.
pub fn write(dir: &Path, path: &str, text: &str) {
    let file = dir.join(path);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(file, text).unwrap();
}

/// An executable shell script at `path` that creates the file `marker` when it runs.
#[cfg(unix)]
pub fn marking_script(path: &Path, marker: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        path,
        format!("#!/bin/sh\ntouch '{}'\ncat\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}
