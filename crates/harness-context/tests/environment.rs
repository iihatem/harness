use std::{path::Path, process::Command, sync::OnceLock};

use harness_context::environment::{self, GitState};

/// Makes the git that `environment::capture` runs ignore this machine's global and system
/// configuration, as the fixtures' git does. Its global configuration is instead a file of the
/// test's that defines a filter driver of the user's own, as git-lfs's `git lfs install` does: a
/// driver there is the user's program, which their own `git status` runs too, so it must not
/// keep harness from saying whether a work tree is dirty.
fn isolate() {
    static GLOBAL: OnceLock<tempfile::TempDir> = OnceLock::new();
    GLOBAL.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("gitconfig");
        std::fs::write(
            &config,
            "[filter \"users\"]\n\tclean = cat\n\tsmudge = cat\n",
        )
        .unwrap();
        // SAFETY: nothing in this binary reads the environment except through `std`, which
        // serializes it with starting processes.
        unsafe {
            std::env::set_var("GIT_CONFIG_GLOBAL", &config);
            std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
        }
        dir
    });
}

/// Runs git in `dir` with the user's own configuration ignored.
fn git(dir: &Path, args: &[&str]) {
    isolate();
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

/// An executable script at `dir/name` that creates `dir/<name>.ran` and passes its input through.
fn marker_script(dir: &Path, name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let script = dir.join(name);
    let marker = dir.join(format!("{name}.ran"));
    std::fs::write(
        &script,
        format!("#!/bin/sh\ntouch '{}'\ncat\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    (script, marker)
}

// Final review, important 1 (probe p3): the repository's own configuration names a file-system
// monitor. Capturing the environment runs before any approval and outside the sandbox, so it
// must not run it.
#[test]
fn a_file_system_monitor_the_repository_names_never_runs() {
    let dir = repo();
    let tools = tempfile::tempdir().unwrap();
    let (script, marker) = marker_script(tools.path(), "fsmonitor");
    git(
        dir.path(),
        &["config", "core.fsmonitor", script.to_str().unwrap()],
    );
    std::fs::write(dir.path().join("a.txt"), "b\n").unwrap();
    let env = environment::capture(dir.path(), "2026-09-27");
    assert!(!marker.exists(), "the fsmonitor hook ran");
    assert_eq!(env.git.unwrap().head.as_deref(), Some("main"));
}

// A clean filter runs for a file whose stat changed; with a filter configured, whether the work
// tree is dirty is left unsaid rather than asked of git.
#[test]
fn a_clean_filter_the_repository_assigns_never_runs() {
    let dir = repo();
    let tools = tempfile::tempdir().unwrap();
    let (script, marker) = marker_script(tools.path(), "clean");
    git(
        dir.path(),
        &["config", "filter.x.clean", script.to_str().unwrap()],
    );
    std::fs::write(dir.path().join(".gitattributes"), "* filter=x\n").unwrap();
    // Same size, new content: git has to compare it, through the filter.
    std::fs::write(dir.path().join("a.txt"), "b\n").unwrap();
    let env = environment::capture(dir.path(), "2026-09-27");
    assert!(!marker.exists(), "the clean filter ran");
    let git_state = env.git.clone().unwrap();
    assert_eq!(git_state.head.as_deref(), Some("main"));
    assert_eq!(git_state.dirty, None);
    let text = env.render();
    assert!(text.contains("Git branch: main\n"), "{text}");
    assert!(!text.contains("Uncommitted changes"), "{text}");
}

// Ruling on the final review's important 1: only the repository's own configuration is untrusted.
// A filter driver in the user's global configuration (see [`isolate`]) keeps the line.
#[test]
fn a_filter_driver_in_the_users_own_configuration_keeps_the_line() {
    isolate();
    let output = Command::new("git")
        .args(["config", "--global", "--get", "filter.users.clean"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&output.stdout), "cat\n");
    let dir = repo();
    std::fs::write(dir.path().join("a.txt"), "b\n").unwrap();
    let env = environment::capture(dir.path(), "2026-09-27");
    assert_eq!(env.git.clone().unwrap().dirty, Some(true));
    assert!(env.render().contains("Uncommitted changes: yes\n"));
}

// A filter driver the repository's configuration pulls in from another file, or sets in its
// worktree configuration, is the repository's too.
#[test]
fn a_filter_driver_included_by_the_repository_or_in_its_worktree_configuration_never_runs() {
    for how in ["include.path", "includeIf", "worktree"] {
        let dir = repo();
        let tools = tempfile::tempdir().unwrap();
        let (script, marker) = marker_script(tools.path(), "clean");
        let definition = format!("[filter \"x\"]\n\tclean = {}\n", script.display());
        match how {
            "worktree" => {
                git(dir.path(), &["config", "extensions.worktreeConfig", "true"]);
                git(
                    dir.path(),
                    &[
                        "config",
                        "--worktree",
                        "filter.x.clean",
                        script.to_str().unwrap(),
                    ],
                );
            }
            include => {
                let included = tools.path().join("included");
                std::fs::write(&included, definition).unwrap();
                let key = if include == "includeIf" {
                    "includeIf.onbranch:main.path"
                } else {
                    "include.path"
                };
                git(dir.path(), &["config", key, included.to_str().unwrap()]);
            }
        }
        std::fs::write(dir.path().join(".gitattributes"), "* filter=x\n").unwrap();
        std::fs::write(dir.path().join("a.txt"), "b\n").unwrap();
        let env = environment::capture(dir.path(), "2026-09-27");
        assert!(!marker.exists(), "{how}: the clean filter ran");
        assert_eq!(env.git.unwrap().dirty, None, "{how}");
    }
}

/// A repository whose own configuration makes it a partial clone, with a staged rename whose old
/// blob is missing: `git status` must read that blob to find the rename, and would fetch it from
/// the promisor remote through `transport` (`sshCommand` or `ext`), which runs `script`.
fn partial_clone_missing_a_blob(transport: &str, script: &Path) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    git(ws, &["init", "-q", "-b", "main"]);
    let lines: String = (1..=50).map(|n| format!("{n}\n")).collect();
    std::fs::write(ws.join("a.txt"), lines).unwrap();
    git(ws, &["add", "a.txt"]);
    git(ws, &["commit", "-q", "-m", "first"]);
    let blob = Command::new("git")
        .arg("-C")
        .arg(ws)
        .args(["rev-parse", "HEAD:a.txt"])
        .output()
        .unwrap();
    let blob = String::from_utf8(blob.stdout).unwrap();
    let blob = blob.trim();
    git(ws, &["mv", "a.txt", "b.txt"]);
    let mut changed = std::fs::read_to_string(ws.join("b.txt")).unwrap();
    changed.push_str("extra\n");
    std::fs::write(ws.join("b.txt"), changed).unwrap();
    git(ws, &["add", "b.txt"]);
    std::fs::remove_file(ws.join(".git/objects").join(&blob[..2]).join(&blob[2..])).unwrap();
    git(ws, &["config", "extensions.partialClone", "origin"]);
    git(ws, &["config", "remote.origin.promisor", "true"]);
    let script = script.to_str().unwrap();
    match transport {
        "sshCommand" => {
            git(
                ws,
                &["config", "remote.origin.url", "ssh://example.invalid/x"],
            );
            git(ws, &["config", "core.sshCommand", script]);
        }
        _ => {
            git(
                ws,
                &["config", "remote.origin.url", &format!("ext::{script}")],
            );
            git(ws, &["config", "protocol.ext.allow", "always"]);
        }
    }
    dir
}

// Re-review of fix wave 4, important 1: in a partial clone, `git status` fetches a missing object
// from the promisor remote, running the transport program the repository's configuration names.
#[test]
fn a_partial_clones_transport_never_runs() {
    for transport in ["sshCommand", "ext"] {
        let tools = tempfile::tempdir().unwrap();
        let (script, marker) = marker_script(tools.path(), "transport");
        let dir = partial_clone_missing_a_blob(transport, &script);
        let env = environment::capture(dir.path(), "2026-09-27");
        assert!(!marker.exists(), "{transport}: the transport ran");
        let git_state = env.git.clone().unwrap();
        assert_eq!(git_state.head.as_deref(), Some("main"), "{transport}");
        assert_eq!(git_state.dirty, None, "{transport}");
        assert!(!env.render().contains("Uncommitted changes"), "{transport}");
    }
}

// `git status` runs `git status` in each submodule, with the submodule's own configuration.
#[test]
fn a_clean_filter_in_a_submodule_never_runs() {
    let dir = repo();
    let sub = dir.path().join("sub");
    std::fs::create_dir(&sub).unwrap();
    git(&sub, &["init", "-q", "-b", "main"]);
    std::fs::write(sub.join("s.txt"), "s\n").unwrap();
    git(&sub, &["add", "s.txt"]);
    git(&sub, &["commit", "-q", "-m", "sub"]);
    let head = Command::new("git")
        .arg("-C")
        .arg(&sub)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    let head = String::from_utf8(head.stdout).unwrap();
    git(
        dir.path(),
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{},sub", head.trim()),
        ],
    );
    git(dir.path(), &["commit", "-q", "-m", "add sub"]);
    let tools = tempfile::tempdir().unwrap();
    let (script, marker) = marker_script(tools.path(), "clean");
    git(
        &sub,
        &["config", "filter.x.clean", script.to_str().unwrap()],
    );
    std::fs::write(sub.join(".gitattributes"), "* filter=x\n").unwrap();
    std::fs::write(sub.join("s.txt"), "t\n").unwrap();
    let env = environment::capture(dir.path(), "2026-09-27");
    assert!(!marker.exists(), "the submodule's clean filter ran");
    assert_eq!(env.git.unwrap().head.as_deref(), Some("main"));
}

// The branch is read from `HEAD`, which a linked worktree keeps in its own git directory.
#[test]
fn a_linked_worktree_reports_its_own_branch() {
    let dir = repo();
    let linked = tempfile::tempdir().unwrap();
    let path = linked.path().join("wt");
    git(
        dir.path(),
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "topic",
            path.to_str().unwrap(),
        ],
    );
    let env = environment::capture(&path, "2026-09-27");
    assert_eq!(
        env.git,
        Some(GitState {
            head: Some("topic".into()),
            dirty: Some(false),
        })
    );
}
