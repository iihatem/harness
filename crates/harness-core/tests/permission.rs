use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use harness_core::engine::{EngineConfig, PermissionEngine};
use harness_core::permission::{Action, Decision, Mode, PermissionPolicy, resolve_path};

fn is_ask(d: Decision) -> bool {
    matches!(d, Decision::Ask(_))
}

fn write(p: &str) -> Action {
    Action::Write(PathBuf::from(p))
}

fn policy(mode: Mode, workspace: &Path, read_dirs: Vec<PathBuf>) -> PermissionEngine {
    PermissionEngine::new(EngineConfig {
        mode,
        workspace: workspace.to_path_buf(),
        read_dirs,
        rules: Default::default(),
        sandbox_available: false,
    })
}

#[test]
fn auto_allows_writes_inside_the_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let policy = policy(Mode::Auto, dir.path(), vec![]);
    assert_eq!(policy.check(&write("src/new.rs")), Decision::Allow);
}

#[test]
fn plan_and_read_only_reject_writes() {
    let dir = tempfile::tempdir().unwrap();
    for mode in [Mode::Plan, Mode::ReadOnly] {
        let policy = policy(mode, dir.path(), vec![]);
        assert!(
            matches!(policy.check(&write("a.txt")), Decision::Deny(_)),
            "{mode}"
        );
    }
}

#[test]
fn ask_mode_asks_for_writes() {
    let dir = tempfile::tempdir().unwrap();
    let policy = policy(Mode::Ask, dir.path(), vec![]);
    assert!(is_ask(policy.check(&write("a.txt"))));
}

#[test]
fn writes_outside_the_workspace_ask_even_in_auto() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let policy = policy(Mode::Auto, &ws, vec![]);
    assert!(is_ask(policy.check(&write("../outside.txt"))));
    assert!(is_ask(policy.check(&write("/etc/hosts"))));
}

#[test]
fn symlink_escapes_are_detected() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    let outside = dir.path().join("outside");
    std::fs::create_dir(&ws).unwrap();
    std::fs::create_dir(&outside).unwrap();
    symlink(&outside, ws.join("link")).unwrap();
    let policy = policy(Mode::Auto, &ws, vec![]);
    assert!(is_ask(policy.check(&write("link/file.txt"))));
    // `link/..` is the parent of the real target, i.e. outside the workspace.
    assert!(is_ask(policy.check(&write("link/../escape.txt"))));
}

// Review Focus: non-canonical or symlinked workspace paths.
#[test]
fn a_symlinked_workspace_path_still_counts_as_inside() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real");
    std::fs::create_dir(&real).unwrap();
    let alias = dir.path().join("alias");
    symlink(&real, &alias).unwrap();
    let policy = policy(Mode::Auto, &alias, vec![]);
    assert_eq!(policy.check(&write("a.txt")), Decision::Allow);
    assert_eq!(
        policy.check(&Action::Write(real.join("b.txt"))),
        Decision::Allow
    );
    assert_eq!(
        policy.check(&Action::Write(alias.join("c.txt"))),
        Decision::Allow
    );
}

#[test]
fn reads_outside_ask_except_allowed_read_dirs() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    let spill = dir.path().join("spill");
    std::fs::create_dir(&ws).unwrap();
    std::fs::create_dir(&spill).unwrap();
    let policy = policy(Mode::Auto, &ws, vec![spill.clone()]);
    assert_eq!(
        policy.check(&Action::Read(PathBuf::from("README.md"))),
        Decision::Allow
    );
    assert_eq!(
        policy.check(&Action::Read(spill.join("call_1.txt"))),
        Decision::Allow
    );
    assert!(is_ask(
        policy.check(&Action::Read(dir.path().join("secret")))
    ));
}

#[test]
fn bash_asks_without_a_sandbox_and_full_access_allows_everything() {
    let dir = tempfile::tempdir().unwrap();
    let auto = policy(Mode::Auto, dir.path(), vec![]);
    assert!(is_ask(auto.check(&Action::Bash("cargo test".into()))));
    let full = policy(Mode::FullAccess, dir.path(), vec![]);
    assert_eq!(
        full.check(&Action::Bash("rm -rf target".into())),
        Decision::Allow
    );
    assert_eq!(full.check(&write("/tmp/x")), Decision::Allow);
}

#[test]
fn resolve_path_handles_missing_tails() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().canonicalize().unwrap();
    assert_eq!(
        resolve_path(&ws, Path::new("a/b/../c.txt")),
        ws.join("a/c.txt")
    );
    assert_eq!(resolve_path(&ws, Path::new("./x")), ws.join("x"));
}

#[test]
fn symlink_to_a_not_yet_created_outside_dir_is_outside() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    let outside = dir.path().join("outside");
    std::fs::create_dir(&ws).unwrap();
    std::fs::create_dir(&outside).unwrap();
    let newsub = outside.join("newsub");
    symlink(&newsub, ws.join("link")).unwrap();
    let policy = policy(Mode::Auto, &ws, vec![]);
    assert!(is_ask(policy.check(&write("link/payload.txt"))));
}

#[test]
fn dangling_symlink_outside_is_outside() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let missing = dir.path().join("missing-unique");
    symlink(&missing, ws.join("link")).unwrap();
    let policy = policy(Mode::Auto, &ws, vec![]);
    assert!(is_ask(policy.check(&write("link/x.txt"))));
}

#[test]
fn symlink_chains_are_followed() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    let outside = dir.path().join("outside");
    std::fs::create_dir(&ws).unwrap();
    std::fs::create_dir(&outside).unwrap();
    let sub = ws.join("sub");
    std::fs::create_dir(&sub).unwrap();
    // ws/a -> ws/b (relative)
    symlink("b", ws.join("a")).unwrap();
    // ws/b -> <outside> (absolute)
    symlink(&outside, ws.join("b")).unwrap();
    // ws/c -> sub (relative, inside, sub exists)
    symlink("sub", ws.join("c")).unwrap();
    let policy = policy(Mode::Auto, &ws, vec![]);
    // a -> b -> outside, so a/x is outside
    assert!(is_ask(policy.check(&write("a/x.txt"))));
    // c -> sub (inside), so c/x is inside
    assert_eq!(policy.check(&write("c/x.txt")), Decision::Allow);
}

#[test]
fn sibling_directory_with_a_common_prefix_is_outside() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    let ws_evil = dir.path().join("ws-evil");
    std::fs::create_dir(&ws).unwrap();
    std::fs::create_dir(&ws_evil).unwrap();
    let policy = policy(Mode::Auto, &ws, vec![]);
    assert!(is_ask(policy.check(&Action::Write(ws_evil.join("x")))));
}

#[test]
fn spill_dir_through_a_symlink_is_readable_even_before_it_exists() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let real = dir.path().join("real");
    std::fs::create_dir(&real).unwrap();
    let alias = dir.path().join("alias");
    symlink(&real, &alias).unwrap();

    // The per-run tool-output dir doesn't exist yet when the policy is built.
    let spill = alias.join("state/tool-output/run-1");
    let policy = policy(Mode::Auto, &ws, vec![spill.clone()]);

    // Now the run creates the spill dir and writes a file into it.
    let real_spill = real.join("state/tool-output/run-1");
    std::fs::create_dir_all(&real_spill).unwrap();
    std::fs::write(real_spill.join("c.txt"), b"hello").unwrap();

    assert_eq!(
        policy.check(&Action::Read(real_spill.join("c.txt"))),
        Decision::Allow
    );
    assert_eq!(
        policy.check(&Action::Read(spill.join("c.txt"))),
        Decision::Allow
    );
}

#[test]
fn auto_mode_asks_before_writing_inside_dot_git() {
    let dir = tempfile::tempdir().unwrap();
    let p = policy(Mode::Auto, dir.path(), vec![]);
    assert!(is_ask(p.check(&write(".git/hooks/pre-commit"))));
    assert!(is_ask(p.check(&write(".git/config"))));
    // A file that merely starts with ".git" but isn't the .git directory is unaffected.
    assert_eq!(p.check(&write("src/.git_notes.txt")), Decision::Allow);
    let full = policy(Mode::FullAccess, dir.path(), vec![]);
    assert_eq!(full.check(&write(".git/hooks/pre-commit")), Decision::Allow);
}

#[test]
fn symlink_loops_do_not_hang() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    symlink("loop", ws.join("loop")).unwrap();
    // Should complete without hanging; exact path doesn't matter
    let _result = resolve_path(&ws, Path::new("loop/x"));
}
