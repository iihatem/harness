use std::{path::Path, process::Command};

use harness_context::environment::{self, GitState};

/// Runs git in `dir` with the user's own configuration ignored.
fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .unwrap()
        .status;
    assert!(status.success(), "git {args:?}");
}

fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(dir.path().join("a.txt"), "a\n").unwrap();
    git(dir.path(), &["add", "a.txt"]);
    git(dir.path(), &["commit", "-q", "-m", "first"]);
    dir
}

#[test]
fn a_dirty_repository_reports_its_branch_and_uncommitted_changes() {
    let dir = repo();
    std::fs::write(dir.path().join("a.txt"), "changed\n").unwrap();
    let env = environment::capture(dir.path(), "2026-09-27");
    assert_eq!(
        env.git,
        Some(GitState {
            head: Some("main".into()),
            dirty: Some(true),
        })
    );
    let text = env.render();
    assert!(text.contains("Git branch: main\n"), "{text}");
    assert!(text.contains("Uncommitted changes: yes\n"), "{text}");
}

#[test]
fn a_clean_repository_says_there_are_no_uncommitted_changes() {
    let dir = repo();
    let text = environment::capture(dir.path(), "2026-09-27").render();
    assert!(text.contains("Uncommitted changes: no\n"), "{text}");
}

#[test]
fn an_untracked_file_counts_as_an_uncommitted_change() {
    let dir = repo();
    std::fs::write(dir.path().join("new.txt"), "n\n").unwrap();
    let env = environment::capture(dir.path(), "2026-09-27");
    assert_eq!(env.git.unwrap().dirty, Some(true));
}

#[test]
fn a_detached_head_is_named_by_its_commit() {
    let dir = repo();
    git(dir.path(), &["checkout", "-q", "--detach"]);
    let head = environment::capture(dir.path(), "2026-09-27")
        .git
        .unwrap()
        .head
        .unwrap();
    assert!(head.starts_with("detached HEAD at "), "{head}");
}

#[test]
fn outside_a_repository_there_is_no_git_line() {
    let dir = tempfile::tempdir().unwrap();
    let env = environment::capture(dir.path(), "2026-09-27");
    assert_eq!(env.git, None);
    let text = env.render();
    assert!(!text.contains("Git"), "{text}");
    assert!(text.contains(&format!("Working directory: {}\n", dir.path().display())));
    assert!(text.contains(&format!("Operating system: {}\n", std::env::consts::OS)));
    assert!(text.contains("Date: 2026-09-27\n"));
}

#[test]
fn on_macos_the_environment_says_how_to_commit_a_multi_line_message() {
    let environment = |os: &str| harness_context::environment::Environment {
        cwd: "/work".into(),
        os: os.into(),
        date: "2026-09-27".into(),
        git: None,
    };
    let macos = environment("macos").render();
    assert!(macos.contains("git commit -F - <<'EOF'"), "{macos}");
    assert!(macos.contains("bash 3.2"), "{macos}");
    let linux = environment("linux").render();
    assert!(!linux.contains("git commit"), "{linux}");
}
