//! The git-metadata guard on real directory trees, simulating what a sandboxed command does
//! between `begin` and `finish`. Platform-neutral: runs on macOS and Linux.
//!
//! The workspace is hostile: every probe stays inside a temp dir, and one that could block (a
//! FIFO) runs on a helper thread that must finish within [`LIMIT`].

use std::ffi::OsStr;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use harness_sandbox::guard::GuardSession;

/// How long a guard over these small trees may take.
const LIMIT: Duration = Duration::from_secs(20);

struct Env {
    _dir: tempfile::TempDir,
    ws: PathBuf,
    quarantine: PathBuf,
    session: Arc<GuardSession>,
}

/// A workspace holding a repository with `config`, `HEAD` and one hook, and a quarantine
/// directory next to it.
fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let ws = base.join("ws");
    std::fs::create_dir_all(ws.join(".git/hooks")).unwrap();
    std::fs::create_dir_all(ws.join(".git/objects")).unwrap();
    std::fs::write(ws.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::write(ws.join(".git/config"), "[core]\n\tbare = false\n").unwrap();
    std::fs::write(ws.join(".git/hooks/pre-commit"), "exit 0\n").unwrap();
    let quarantine = base.join("quarantine");
    let session = GuardSession::new(&quarantine);
    Env {
        _dir: dir,
        ws,
        quarantine,
        session,
    }
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap()
}

fn gone(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_err()
}

/// The one entry the quarantine holds at `rel` (relative to the workspace).
fn quarantined(env: &Env, rel: &str) -> PathBuf {
    let mut found: Vec<PathBuf> = std::fs::read_dir(&env.quarantine)
        .unwrap()
        .map(|e| e.unwrap().path().join(rel))
        .filter(|p| std::fs::symlink_metadata(p).is_ok())
        .collect();
    assert_eq!(found.len(), 1, "{rel} in quarantine: {found:?}");
    found.remove(0)
}

fn mkfifo(path: &Path) {
    let status = Command::new("/usr/bin/mkfifo").arg(path).status().unwrap();
    assert!(status.success());
}

/// `run()`, on a thread that must finish within [`LIMIT`]. A thread that does not is left
/// behind, blocked; the test fails either way.
fn bounded<T: Send + 'static>(run: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run());
    });
    rx.recv_timeout(LIMIT)
        .unwrap_or_else(|_| panic!("the guard took longer than {LIMIT:?}"))
}

#[test]
fn a_command_that_leaves_git_metadata_alone_gets_no_report() {
    let env = env();
    let guard = env.session.begin(&env.ws, true, |_| {});
    // What git writes during commit, checkout and stash.
    std::fs::write(env.ws.join(".git/index.lock"), "x").unwrap();
    std::fs::rename(env.ws.join(".git/index.lock"), env.ws.join(".git/index")).unwrap();
    std::fs::write(env.ws.join(".git/COMMIT_EDITMSG"), "msg\n").unwrap();
    std::fs::create_dir_all(env.ws.join(".git/objects/ab")).unwrap();
    std::fs::write(env.ws.join(".git/objects/ab/cdef"), "blob").unwrap();
    std::fs::write(env.ws.join("src.txt"), "work").unwrap();
    assert_eq!(guard.finish(), None);
    assert!(!env.quarantine.exists());
}

#[test]
fn a_real_git_commit_gets_no_report() {
    let env = env();
    std::fs::remove_dir_all(env.ws.join(".git")).unwrap();
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .current_dir(&env.ws)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap()
    };
    assert!(git(&["init", "-q"]).status.success());
    let guard = env.session.begin(&env.ws, true, |_| {});
    std::fs::write(env.ws.join("a.txt"), "a").unwrap();
    assert!(git(&["add", "a.txt"]).status.success());
    let commit = git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit",
        "-q",
        "-m",
        "x",
    ]);
    assert!(commit.status.success(), "{commit:?}");
    assert!(git(&["checkout", "-q", "-b", "other"]).status.success());
    assert_eq!(guard.finish(), None);
}

#[test]
fn a_planted_hook_is_quarantined_and_blocks_the_command() {
    let env = env();
    let guard = env.session.begin(&env.ws, true, |_| {});
    std::fs::write(env.ws.join(".git/hooks/post-checkout"), "echo pwned\n").unwrap();
    let report = guard.finish().expect("a report");
    assert!(report.blocked);
    assert!(gone(&env.ws.join(".git/hooks/post-checkout")));
    let moved = quarantined(&env, ".git/hooks/post-checkout");
    assert_eq!(read(&moved), "echo pwned\n");
    assert!(
        report.message.contains(&format!(
            "- .git/hooks/post-checkout: new in a protected directory; moved to {}",
            moved.display()
        )),
        "{}",
        report.message
    );
}

#[test]
fn a_changed_config_is_restored_and_the_change_kept() {
    let env = env();
    let guard = env.session.begin(&env.ws, true, |_| {});
    std::fs::write(
        env.ws.join(".git/config"),
        "[core]\n\tfsmonitor = /tmp/evil\n",
    )
    .unwrap();
    let report = guard.finish().expect("a report");
    assert!(report.blocked);
    assert_eq!(
        read(&env.ws.join(".git/config")),
        "[core]\n\tbare = false\n"
    );
    assert_eq!(
        read(&quarantined(&env, ".git/config")),
        "[core]\n\tfsmonitor = /tmp/evil\n"
    );
    assert!(
        report
            .message
            .contains("- .git/config: changed; restored the earlier version")
    );
}

#[test]
fn a_deleted_hook_is_restored() {
    let env = env();
    let guard = env.session.begin(&env.ws, true, |_| {});
    std::fs::remove_file(env.ws.join(".git/hooks/pre-commit")).unwrap();
    let report = guard.finish().expect("a report");
    assert_eq!(read(&env.ws.join(".git/hooks/pre-commit")), "exit 0\n");
    assert!(
        report
            .message
            .contains("- .git/hooks/pre-commit: deleted; restored the earlier version")
    );
}

#[test]
fn without_saving_everything_only_new_names_are_undone() {
    // The Linux full tier: read-only mounts stop changes to existing entries, so they are not
    // saved; new names still appear, because mounts cannot cover what does not exist.
    let env = env();
    let guard = env.session.begin(&env.ws, false, |_| {});
    std::fs::write(
        env.ws.join(".git/config"),
        "changed through a mount that was not there\n",
    )
    .unwrap();
    std::fs::write(env.ws.join(".git/commondir"), "/tmp/evil\n").unwrap();
    let report = guard.finish().expect("a report");
    assert_eq!(
        read(&env.ws.join(".git/config")),
        "changed through a mount that was not there\n"
    );
    assert!(gone(&env.ws.join(".git/commondir")));
    assert_eq!(read(&quarantined(&env, ".git/commondir")), "/tmp/evil\n");
    assert!(
        !report.message.contains(".git/config"),
        "{}",
        report.message
    );
}

#[test]
fn a_repointed_hooks_symlink_is_put_back_even_without_saving_everything() {
    // Read-only mounts cannot cover a symlink, so the full tier relies on the guard for it.
    let env = env();
    std::fs::rename(env.ws.join(".git/hooks"), env.ws.join("tracked-hooks")).unwrap();
    symlink("../tracked-hooks", env.ws.join(".git/hooks")).unwrap();
    let guard = env.session.begin(&env.ws, false, |_| {});
    std::fs::remove_file(env.ws.join(".git/hooks")).unwrap();
    symlink("/tmp/evil-hooks", env.ws.join(".git/hooks")).unwrap();
    let report = guard.finish().expect("a report");
    assert!(report.blocked);
    assert_eq!(
        std::fs::read_link(env.ws.join(".git/hooks")).unwrap(),
        PathBuf::from("../tracked-hooks")
    );
    assert!(
        report
            .message
            .contains("- .git/hooks: changed; restored the earlier version"),
        "{}",
        report.message
    );
}

#[test]
fn a_hard_linked_config_is_restored_even_without_saving_everything() {
    let env = env();
    std::fs::hard_link(env.ws.join(".git/config"), env.ws.join("alias")).unwrap();
    let guard = env.session.begin(&env.ws, false, |_| {});
    std::fs::write(env.ws.join("alias"), "[core]\n\thooksPath = /tmp/evil\n").unwrap();
    let report = guard.finish().expect("a report");
    assert!(report.blocked);
    assert_eq!(
        read(&env.ws.join(".git/config")),
        "[core]\n\tbare = false\n"
    );
}

#[test]
fn new_protected_names_in_every_gitdir_are_quarantined() {
    let env = env();
    std::fs::create_dir_all(env.ws.join(".git/modules/sub")).unwrap();
    std::fs::write(env.ws.join(".git/modules/sub/HEAD"), "ref: x\n").unwrap();
    let guard = env.session.begin(&env.ws, true, |_| {});
    std::fs::write(env.ws.join(".git/modules/sub/commondir"), "/tmp/evil\n").unwrap();
    std::fs::write(env.ws.join(".git/config.worktree"), "[core]\n").unwrap();
    std::fs::create_dir(env.ws.join(".git/gitweb")).unwrap();
    std::fs::write(env.ws.join(".git/pid"), "1\n").unwrap();
    let report = guard.finish().expect("a report");
    for rel in [
        ".git/modules/sub/commondir",
        ".git/config.worktree",
        ".git/gitweb",
        ".git/pid",
    ] {
        assert!(gone(&env.ws.join(rel)), "{rel}");
        quarantined(&env, rel);
        assert!(
            report.message.contains(&format!("- {rel}: new; moved to ")),
            "{rel}"
        );
    }
}

#[test]
fn a_top_level_head_and_harness_dir_are_quarantined() {
    let env = env();
    let guard = env.session.begin(&env.ws, true, |_| {});
    std::fs::write(env.ws.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::create_dir(env.ws.join(".harness")).unwrap();
    std::fs::write(
        env.ws.join(".harness/config.toml"),
        "mode = \"full-access\"\n",
    )
    .unwrap();
    let report = guard.finish().expect("a report");
    assert!(report.blocked);
    assert!(gone(&env.ws.join("HEAD")) && gone(&env.ws.join(".harness")));
    assert_eq!(
        read(&quarantined(&env, ".harness").join("config.toml")),
        "mode = \"full-access\"\n"
    );
}

#[test]
fn new_repositories_worktrees_and_submodules_are_quarantined() {
    let env = env();
    let guard = env.session.begin(&env.ws, true, |_| {});
    std::fs::create_dir_all(env.ws.join("sub/.git/hooks")).unwrap();
    std::fs::write(env.ws.join("sub/file.txt"), "kept").unwrap();
    std::fs::create_dir_all(env.ws.join(".git/worktrees/wt")).unwrap();
    std::fs::write(env.ws.join(".git/worktrees/wt/commondir"), "../..\n").unwrap();
    std::fs::create_dir_all(env.ws.join(".git/modules/m")).unwrap();
    std::fs::write(env.ws.join(".git/modules/m/HEAD"), "ref: x\n").unwrap();
    let report = guard.finish().expect("a report");
    assert!(gone(&env.ws.join("sub/.git")));
    assert_eq!(read(&env.ws.join("sub/file.txt")), "kept");
    quarantined(&env, "sub/.git");
    quarantined(&env, ".git/worktrees/wt");
    quarantined(&env, ".git/modules/m");
    assert!(
        report
            .message
            .contains("- sub/.git: a new repository; moved to ")
    );
    assert!(
        report
            .message
            .contains("- .git/worktrees/wt: a new worktree or submodule gitdir; moved to ")
    );
}

#[test]
fn a_new_repository_in_an_ignored_directory_is_not_found() {
    // A known window (design.md): the walk skips git-ignored directories.
    let env = env();
    std::fs::write(env.ws.join(".gitignore"), "build/\n").unwrap();
    let guard = env.session.begin(&env.ws, true, |_| {});
    std::fs::create_dir_all(env.ws.join("build/.git")).unwrap();
    assert_eq!(guard.finish(), None);
    assert!(env.ws.join("build/.git").exists());
}

#[test]
fn a_replaced_dot_git_is_quarantined_and_reported() {
    let env = env();
    let guard = env.session.begin(&env.ws, true, |_| {});
    std::fs::rename(env.ws.join(".git"), env.ws.join("moved")).unwrap();
    std::fs::create_dir_all(env.ws.join(".git/hooks")).unwrap();
    std::fs::write(env.ws.join(".git/hooks/pre-commit"), "echo pwned\n").unwrap();
    let report = guard.finish().expect("a report");
    assert!(report.blocked);
    assert!(gone(&env.ws.join(".git")));
    assert_eq!(
        read(&quarantined(&env, ".git").join("hooks/pre-commit")),
        "echo pwned\n"
    );
    assert!(
        report
            .message
            .contains("- .git: moved or replaced; moved to "),
        "{}",
        report.message
    );
    // The repository the command moved away is left where it is.
    assert!(env.ws.join("moved/HEAD").exists());
}

#[test]
fn a_repointed_dot_git_symlink_is_put_back() {
    let env = env();
    std::fs::rename(env.ws.join(".git"), env.ws.join("real")).unwrap();
    symlink("real", env.ws.join(".git")).unwrap();
    let guard = env.session.begin(&env.ws, true, |_| {});
    std::fs::remove_file(env.ws.join(".git")).unwrap();
    symlink("/tmp", env.ws.join(".git")).unwrap();
    let report = guard.finish().expect("a report");
    assert_eq!(
        std::fs::read_link(env.ws.join(".git")).unwrap(),
        PathBuf::from("real")
    );
    assert_eq!(
        std::fs::read_link(quarantined(&env, ".git")).unwrap(),
        PathBuf::from("/tmp")
    );
    assert!(
        report
            .message
            .contains("- .git: moved or replaced; restored the earlier version")
    );
}

#[test]
fn a_rewritten_gitfile_is_restored() {
    let env = env();
    std::fs::create_dir_all(env.ws.join(".git/modules/sub")).unwrap();
    std::fs::write(env.ws.join(".git/modules/sub/HEAD"), "ref: x\n").unwrap();
    std::fs::create_dir_all(env.ws.join("sub")).unwrap();
    std::fs::write(env.ws.join("sub/.git"), "gitdir: ../.git/modules/sub\n").unwrap();
    let guard = env.session.begin(&env.ws, true, |_| {});
    std::fs::write(env.ws.join("sub/.git"), "gitdir: /tmp/evil\n").unwrap();
    let report = guard.finish().expect("a report");
    assert!(report.blocked);
    assert_eq!(
        read(&env.ws.join("sub/.git")),
        "gitdir: ../.git/modules/sub\n"
    );
}

#[test]
fn a_gitfile_replaced_by_a_new_file_is_restored() {
    let env = env();
    std::fs::create_dir_all(env.ws.join(".git/modules/sub")).unwrap();
    std::fs::write(env.ws.join(".git/modules/sub/HEAD"), "ref: x\n").unwrap();
    std::fs::create_dir_all(env.ws.join("sub")).unwrap();
    std::fs::write(env.ws.join("sub/.git"), "gitdir: ../.git/modules/sub\n").unwrap();
    let guard = env.session.begin(&env.ws, true, |_| {});
    std::fs::remove_file(env.ws.join("sub/.git")).unwrap();
    std::fs::write(env.ws.join("sub/.git"), "gitdir: /tmp/evil\n").unwrap();
    let report = guard.finish().expect("a report");
    assert!(report.blocked);
    assert_eq!(
        read(&env.ws.join("sub/.git")),
        "gitdir: ../.git/modules/sub\n"
    );
    assert_eq!(read(&quarantined(&env, "sub/.git")), "gitdir: /tmp/evil\n");
    assert!(
        report
            .message
            .contains("- sub/.git: moved or replaced; restored the earlier version"),
        "{}",
        report.message
    );
}

#[test]
fn names_planted_after_a_command_ends_are_caught_before_the_next() {
    let env = env();
    assert_eq!(env.session.begin(&env.ws, true, |_| {}).finish(), None);
    // A process the first command left running plants these after it ended.
    std::fs::write(env.ws.join(".git/commondir"), "/tmp/evil\n").unwrap();
    std::fs::write(env.ws.join("HEAD"), "ref: x\n").unwrap();
    let report = env
        .session
        .begin(&env.ws, true, |_| {})
        .finish()
        .expect("a report");
    assert!(!report.blocked, "the second command did nothing wrong");
    assert!(
        report.message.starts_with("[before this command ran"),
        "{}",
        report.message
    );
    assert!(gone(&env.ws.join(".git/commondir")) && gone(&env.ws.join("HEAD")));
}

#[test]
fn a_repository_created_between_commands_is_left_alone() {
    // The user may clone into the workspace while harness runs; only names inside gitdirs
    // that were already known are checked before a command.
    let env = env();
    assert_eq!(env.session.begin(&env.ws, true, |_| {}).finish(), None);
    std::fs::create_dir_all(env.ws.join("cloned/.git/hooks")).unwrap();
    std::fs::write(env.ws.join("cloned/.git/config"), "[core]\n").unwrap();
    assert_eq!(env.session.begin(&env.ws, true, |_| {}).finish(), None);
    assert!(env.ws.join("cloned/.git/config").exists());
}

#[test]
fn placeholders_run_before_existing_names_are_recorded() {
    let env = env();
    std::fs::remove_dir_all(env.ws.join(".git/hooks")).unwrap();
    let guard = env.session.begin(&env.ws, false, |index| {
        for gitdir in &index.gitdirs {
            std::fs::create_dir(gitdir.join("hooks")).unwrap();
        }
    });
    assert_eq!(guard.finish(), None);
    assert!(env.ws.join(".git/hooks").is_dir());
}

#[test]
fn the_quarantine_inside_the_workspace_is_not_walked() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().canonicalize().unwrap();
    std::fs::create_dir_all(ws.join(".git/hooks")).unwrap();
    let session = GuardSession::new(&ws.join(".quarantine"));
    let guard = session.begin(&ws, true, |_| {});
    std::fs::create_dir_all(ws.join("sub/.git")).unwrap();
    let report = guard.finish().expect("a report");
    assert_eq!(
        report.message.matches("\n- sub/.git:").count(),
        1,
        "{}",
        report.message
    );
    assert_eq!(session.begin(&ws, true, |_| {}).finish(), None);
}

#[test]
fn a_watcher_can_undo_changes_while_the_command_runs() {
    let env = env();
    let guard = env.session.begin(&env.ws, true, |_| {});
    let handle = guard.watch_handle();
    let dirs = handle.dirs();
    for dir in [&env.ws, &env.ws.join(".git"), &env.ws.join(".git/hooks")] {
        assert!(dirs.contains(dir), "{dir:?} not in {dirs:?}");
    }
    let git = env.ws.join(".git");
    assert!(handle.relevant(&env.ws, Some(OsStr::new(".git"))));
    assert!(handle.relevant(&env.ws, Some(OsStr::new("HEAD"))));
    assert!(!handle.relevant(&env.ws, Some(OsStr::new("src.txt"))));
    assert!(handle.relevant(&git, Some(OsStr::new("commondir"))));
    assert!(handle.relevant(&git, Some(OsStr::new("worktrees"))));
    assert!(!handle.relevant(&git, Some(OsStr::new("index.lock"))));
    assert!(handle.relevant(&git.join("hooks"), Some(OsStr::new("post-checkout"))));
    assert!(handle.relevant(&git, None));

    std::fs::write(git.join("commondir"), "/tmp/evil\n").unwrap();
    handle.check();
    assert!(
        gone(&git.join("commondir")),
        "moved while the command still runs"
    );
    handle.check();
    let report = guard.finish().expect("a report");
    assert_eq!(
        report.message.matches("\n- .git/commondir:").count(),
        1,
        "{}",
        report.message
    );
    // Once the guard has finished, its handle does nothing.
    std::fs::write(git.join("commondir"), "/tmp/evil\n").unwrap();
    handle.check();
    assert!(git.join("commondir").exists());
}

// Amendment A: the ignore rules are read once per session.

#[test]
fn the_ignore_rules_are_read_once_so_a_command_cannot_hide_its_repository() {
    let env = env();
    env.session.prime(&env.ws);
    let guard = env.session.begin(&env.ws, true, |_| {});
    // `echo sub/ >> .gitignore && git init sub`
    std::fs::write(env.ws.join(".gitignore"), "sub/\n").unwrap();
    std::fs::create_dir_all(env.ws.join("sub/.git/hooks")).unwrap();
    let report = guard.finish().expect("a report");
    assert!(report.blocked);
    assert!(gone(&env.ws.join("sub/.git")));
    quarantined(&env, "sub/.git");
}

#[test]
fn begin_uses_the_rules_read_when_the_session_was_primed() {
    let env = env();
    std::fs::write(env.ws.join(".gitignore"), "build/\n").unwrap();
    env.session.prime(&env.ws);
    std::fs::write(env.ws.join(".gitignore"), "").unwrap();
    let guard = env.session.begin(&env.ws, true, |_| {});
    std::fs::create_dir_all(env.ws.join("build/.git")).unwrap();
    assert_eq!(guard.finish(), None);
    assert!(env.ws.join("build/.git").exists());
}

// Amendment B: nothing is reached through a symlink.

#[test]
fn a_protected_directory_swapped_for_a_symlink_is_never_followed() {
    let env = env();
    let outside = env.ws.parent().unwrap().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("pre-commit"), "outside\n").unwrap();
    std::fs::write(outside.join("post-checkout"), "outside hook\n").unwrap();
    let guard = env.session.begin(&env.ws, true, |_| {});
    let hooks = env.ws.join(".git/hooks");
    std::fs::rename(&hooks, env.ws.join(".git/hooks-old")).unwrap();
    symlink(&outside, &hooks).unwrap();
    let report = guard.finish().expect("a report");
    assert!(report.blocked);
    let mut names: Vec<_> = std::fs::read_dir(&outside)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    names.sort();
    assert_eq!(names, ["post-checkout", "pre-commit"]);
    assert_eq!(read(&outside.join("pre-commit")), "outside\n");
    assert_eq!(read(&outside.join("post-checkout")), "outside hook\n");
    assert!(std::fs::symlink_metadata(&hooks).unwrap().is_dir());
    assert_eq!(read(&hooks.join("pre-commit")), "exit 0\n");
    assert_eq!(
        std::fs::read_link(quarantined(&env, ".git/hooks")).unwrap(),
        outside
    );
}

#[test]
fn metadata_made_unreachable_is_reported_and_blocks_the_command() {
    // A hook planted, then its gitdir (or the directory above a repository) made unreadable:
    // git would pick the hook up once the permissions are put back. As root, where
    // permissions do not stop harness, the hook is moved instead; blocked either way.
    use std::os::unix::fs::PermissionsExt;
    // SAFETY: no preconditions.
    let root = unsafe { libc::geteuid() } == 0;
    let env = env();
    std::fs::create_dir_all(env.ws.join("sub/.git/hooks")).unwrap();
    std::fs::write(env.ws.join("sub/.git/HEAD"), "ref: x\n").unwrap();
    for (locked, hook, unreachable) in [
        (
            ".git",
            ".git/hooks/post-checkout",
            "\n- .git/hooks: could not be checked; ",
        ),
        (
            "sub",
            "sub/.git/hooks/post-checkout",
            "\n- sub/.git: could not be checked; ",
        ),
    ] {
        let locked = env.ws.join(locked);
        let guard = env.session.begin(&env.ws, true, |_| {});
        std::fs::write(env.ws.join(hook), "echo pwned\n").unwrap();
        std::fs::set_permissions(&locked, PermissionsExt::from_mode(0o000)).unwrap();
        let report = guard.finish().expect("a report");
        std::fs::set_permissions(&locked, PermissionsExt::from_mode(0o755)).unwrap();
        assert!(report.blocked, "{locked:?}: {}", report.message);
        assert!(
            root || report.message.contains(unreachable),
            "{locked:?}: {}",
            report.message
        );
        let _ = std::fs::remove_file(env.ws.join(hook));
        // With the permissions back, what was there all along is not taken for new.
        let next = env.session.begin(&env.ws, true, |_| {}).finish();
        assert!(
            next.as_ref()
                .is_none_or(|next| !next.message.contains(": new;")),
            "{locked:?}: {next:?}"
        );
        assert_eq!(
            read(&env.ws.join(".git/config")),
            "[core]\n\tbare = false\n"
        );
    }
}

#[test]
fn a_fifo_swapped_in_for_config_never_blocks_the_guard() {
    let env = env();
    let guard = env.session.begin(&env.ws, true, |_| {});
    std::fs::remove_file(env.ws.join(".git/config")).unwrap();
    mkfifo(&env.ws.join(".git/config"));
    let report = bounded(move || guard.finish()).expect("a report");
    assert!(report.blocked);
    assert_eq!(
        read(&env.ws.join(".git/config")),
        "[core]\n\tbare = false\n"
    );
    let moved = quarantined(&env, ".git/config");
    assert!(
        !std::fs::symlink_metadata(moved)
            .unwrap()
            .file_type()
            .is_file()
    );
}

// Amendment C: a scan that could not cover the whole workspace.

/// Makes every scan of the workspace incomplete: an ignore file that is a symlink cannot be
/// used as git would use it.
fn unusable_gitignore(env: &Env) {
    symlink("elsewhere", env.ws.join(".gitignore")).unwrap();
}

#[test]
fn an_incomplete_scan_is_reported_once_per_session_without_blocking() {
    let env = env();
    unusable_gitignore(&env);
    env.session.prime(&env.ws);
    let report = env
        .session
        .begin(&env.ws, true, |_| {})
        .finish()
        .expect("a report");
    assert!(!report.blocked, "{}", report.message);
    assert!(
        report
            .message
            .contains("harness could not scan the whole workspace"),
        "{}",
        report.message
    );
    assert_eq!(
        env.session.begin(&env.ws, true, |_| {}).finish(),
        None,
        "once per session"
    );
}

#[test]
fn after_an_incomplete_scan_new_repositories_are_listed_and_left_in_place() {
    let env = env();
    unusable_gitignore(&env);
    env.session.prime(&env.ws);
    let _ = env.session.begin(&env.ws, true, |_| {}).finish();
    let guard = env.session.begin(&env.ws, true, |_| {});
    for i in 0..12 {
        std::fs::create_dir_all(env.ws.join(format!("r{i:02}/.git"))).unwrap();
    }
    let report = guard.finish().expect("a report");
    assert!(!report.blocked, "{}", report.message);
    assert!(
        report.message.contains(
            "found, not checked: harness could not scan the whole workspace, so it cannot tell whether these are new"
        ),
        "{}",
        report.message
    );
    assert!(
        report.message.contains("\n- r00/.git"),
        "{}",
        report.message
    );
    assert!(
        report.message.contains("\n- r09/.git"),
        "{}",
        report.message
    );
    assert!(!report.message.contains("r10/.git"), "{}", report.message);
    assert!(
        report.message.contains("\n- and 2 more"),
        "{}",
        report.message
    );
    for i in 0..12 {
        assert!(env.ws.join(format!("r{i:02}/.git")).is_dir());
    }
    assert!(!env.quarantine.exists());
}

#[test]
fn a_command_that_leaves_the_workspace_uncheckable_is_blocked() {
    let env = env();
    let guard = env.session.begin(&env.ws, true, |_| {});
    // A `modules/` tree deeper than the scan follows.
    let mut deep = env.ws.join(".git/modules");
    for _ in 0..70 {
        deep.push("m");
    }
    std::fs::create_dir_all(&deep).unwrap();
    std::fs::create_dir_all(env.ws.join("sub/.git")).unwrap();
    let report = guard.finish().expect("a report");
    assert!(report.blocked);
    assert!(
        report.message.contains("too large or unreadable"),
        "{}",
        report.message
    );
    assert!(
        report.message.contains(".gitignore rules"),
        "{}",
        report.message
    );
    // The scan before the command was complete, so what the scan after it found is new.
    assert!(gone(&env.ws.join("sub/.git")));
    quarantined(&env, "sub/.git");
}

#[test]
fn a_repository_gone_after_the_command_is_reported_without_blocking() {
    let env = env();
    std::fs::create_dir_all(env.ws.join("sub/.git/hooks")).unwrap();
    std::fs::write(env.ws.join("sub/.git/HEAD"), "ref: x\n").unwrap();
    let guard = env.session.begin(&env.ws, true, |_| {});
    std::fs::remove_dir_all(env.ws.join("sub")).unwrap();
    let report = guard.finish().expect("a report");
    assert!(!report.blocked, "{}", report.message);
    assert!(
        report.message.contains(
            "- sub/.git: no longer at this path: moved or deleted; harness does not protect it where it went"
        ),
        "{}",
        report.message
    );
    assert_eq!(
        report.message.matches("\n- ").count(),
        1,
        "{}",
        report.message
    );
}

// Amendment D (R9): changes by processes a command left running.

fn evil_config(env: &Env) {
    std::fs::write(
        env.ws.join(".git/config"),
        "[core]\n\tfsmonitor = /tmp/evil\n",
    )
    .unwrap();
}

#[test]
fn with_survivors_a_config_rewrite_after_a_command_is_undone_before_the_next() {
    let env = env();
    env.session.set_survivor_probe(Arc::new(|| true));
    assert_eq!(env.session.begin(&env.ws, true, |_| {}).finish(), None);
    evil_config(&env);
    std::fs::write(env.ws.join(".git/hooks/post-checkout"), "echo pwned\n").unwrap();
    let report = env
        .session
        .begin(&env.ws, true, |_| {})
        .finish()
        .expect("a report");
    assert!(!report.blocked, "{}", report.message);
    assert!(
        report.message.starts_with("[before this command ran"),
        "{}",
        report.message
    );
    assert!(
        report
            .message
            .contains("- .git/config: changed; restored the earlier version"),
        "{}",
        report.message
    );
    assert!(
        report
            .message
            .contains("- .git/hooks/post-checkout: new in a protected directory; moved to "),
        "{}",
        report.message
    );
    assert_eq!(
        read(&env.ws.join(".git/config")),
        "[core]\n\tbare = false\n"
    );
    assert_eq!(
        read(&quarantined(&env, ".git/config")),
        "[core]\n\tfsmonitor = /tmp/evil\n"
    );
    assert!(gone(&env.ws.join(".git/hooks/post-checkout")));
}

#[test]
fn without_survivors_a_config_change_between_commands_is_the_users() {
    let env = env();
    assert_eq!(env.session.begin(&env.ws, true, |_| {}).finish(), None);
    evil_config(&env);
    assert_eq!(env.session.begin(&env.ws, true, |_| {}).finish(), None);
    assert_eq!(
        read(&env.ws.join(".git/config")),
        "[core]\n\tfsmonitor = /tmp/evil\n"
    );
}

#[test]
fn survivors_that_exit_before_the_next_command_still_get_their_changes_undone() {
    let env = env();
    let alive = Arc::new(AtomicBool::new(true));
    // Alive when the first command finishes, gone by the time anything asks again.
    env.session
        .set_survivor_probe(Arc::new(move || alive.swap(false, Ordering::SeqCst)));
    assert_eq!(env.session.begin(&env.ws, true, |_| {}).finish(), None);
    evil_config(&env);
    let report = env
        .session
        .begin(&env.ws, true, |_| {})
        .finish()
        .expect("a report");
    assert!(!report.blocked, "{}", report.message);
    assert_eq!(
        read(&env.ws.join(".git/config")),
        "[core]\n\tbare = false\n"
    );
}

#[test]
fn in_the_full_tier_changes_between_commands_are_not_restored() {
    // Survivors stay under the read-only mounts there; only new names are checked.
    let env = env();
    env.session.set_survivor_probe(Arc::new(|| true));
    assert_eq!(env.session.begin(&env.ws, false, |_| {}).finish(), None);
    evil_config(&env);
    std::fs::write(env.ws.join(".git/commondir"), "/tmp/evil\n").unwrap();
    let report = env
        .session
        .begin(&env.ws, false, |_| {})
        .finish()
        .expect("a report");
    assert!(
        !report.message.contains(".git/config"),
        "{}",
        report.message
    );
    assert!(report.message.contains("- .git/commondir: new; moved to "));
    assert_eq!(
        read(&env.ws.join(".git/config")),
        "[core]\n\tfsmonitor = /tmp/evil\n"
    );
}

#[test]
fn a_watcher_can_undo_changes_between_commands_while_survivors_live() {
    let env = env();
    env.session.set_survivor_probe(Arc::new(|| true));
    assert_eq!(env.session.begin(&env.ws, true, |_| {}).finish(), None);
    let handle = env
        .session
        .between_commands(&env.ws)
        .expect("survivors to watch for");
    let git = env.ws.join(".git");
    let dirs = handle.dirs();
    for dir in [&env.ws, &git, &git.join("hooks")] {
        assert!(dirs.contains(dir), "{dir:?} not in {dirs:?}");
    }
    assert!(handle.relevant(&git, Some(OsStr::new("config"))));
    assert!(handle.relevant(&git.join("hooks"), Some(OsStr::new("x"))));
    assert!(!handle.relevant(&env.ws, Some(OsStr::new("src.txt"))));

    evil_config(&env);
    handle.check();
    assert_eq!(
        read(&git.join("config")),
        "[core]\n\tbare = false\n",
        "restored while the survivor lives"
    );
    handle.check();
    let report = env
        .session
        .begin(&env.ws, true, |_| {})
        .finish()
        .expect("a report");
    assert!(!report.blocked, "{}", report.message);
    assert!(
        report.message.starts_with("[before this command ran"),
        "{}",
        report.message
    );
    assert_eq!(
        report.message.matches("\n- .git/config: changed;").count(),
        1,
        "{}",
        report.message
    );
    // The next command has begun: the handle from before it does nothing.
    assert!(handle.dirs().is_empty());
    evil_config(&env);
    handle.check();
    assert_eq!(
        read(&git.join("config")),
        "[core]\n\tfsmonitor = /tmp/evil\n"
    );
}

#[test]
fn without_survivors_there_is_nothing_to_watch_between_commands() {
    let env = env();
    assert_eq!(env.session.begin(&env.ws, true, |_| {}).finish(), None);
    assert!(env.session.between_commands(&env.ws).is_none());
}
