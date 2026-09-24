use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use harness_core::permission::{
    Action, BaselinePolicy, Decision, Mode, PermissionPolicy, resolve_path,
};

fn is_ask(d: Decision) -> bool {
    matches!(d, Decision::Ask(_))
}

fn write(p: &str) -> Action {
    Action::Write(PathBuf::from(p))
}

#[test]
fn auto_allows_writes_inside_the_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let policy = BaselinePolicy::new(Mode::Auto, dir.path(), vec![]);
    assert_eq!(policy.check(&write("src/new.rs")), Decision::Allow);
}

#[test]
fn plan_and_read_only_reject_writes() {
    let dir = tempfile::tempdir().unwrap();
    for mode in [Mode::Plan, Mode::ReadOnly] {
        let policy = BaselinePolicy::new(mode, dir.path(), vec![]);
        assert!(
            matches!(policy.check(&write("a.txt")), Decision::Deny(_)),
            "{mode}"
        );
    }
}

#[test]
fn ask_mode_asks_for_writes() {
    let dir = tempfile::tempdir().unwrap();
    let policy = BaselinePolicy::new(Mode::Ask, dir.path(), vec![]);
    assert!(is_ask(policy.check(&write("a.txt"))));
}

#[test]
fn writes_outside_the_workspace_ask_even_in_auto() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let policy = BaselinePolicy::new(Mode::Auto, &ws, vec![]);
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
    let policy = BaselinePolicy::new(Mode::Auto, &ws, vec![]);
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
    let policy = BaselinePolicy::new(Mode::Auto, &alias, vec![]);
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
    let policy = BaselinePolicy::new(Mode::Auto, &ws, vec![spill.clone()]);
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
    let auto = BaselinePolicy::new(Mode::Auto, dir.path(), vec![]);
    assert!(is_ask(auto.check(&Action::Bash("cargo test".into()))));
    let full = BaselinePolicy::new(Mode::FullAccess, dir.path(), vec![]);
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
