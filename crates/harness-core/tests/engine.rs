use std::path::{Path, PathBuf};

use harness_core::engine::{EngineConfig, PermissionEngine, RuleSet};
use harness_core::permission::{Action, Decision, FsAccess, Mode, PermissionPolicy};

fn engine(mode: Mode, ws: &Path, sandbox: bool, rules: RuleSet) -> PermissionEngine {
    PermissionEngine::new(EngineConfig {
        mode,
        workspace: ws.to_path_buf(),
        read_dirs: vec![],
        rules,
        sandbox_available: sandbox,
    })
}

fn rules(allow: &[&str], deny: &[&str], confirm: &[&str]) -> RuleSet {
    let own = |v: &[&str]| v.iter().map(|s| s.to_string()).collect();
    RuleSet {
        allow: own(allow),
        deny: own(deny),
        confirm: own(confirm),
    }
}

fn bash(cmd: &str) -> Action {
    Action::Bash(cmd.to_string())
}

fn is_ask(d: &Decision) -> bool {
    matches!(d, Decision::Ask(_))
}

fn is_deny(d: &Decision) -> bool {
    matches!(d, Decision::Deny(_))
}

#[test]
fn modes_map_to_sandbox_access() {
    assert_eq!(Mode::Plan.fs_access(), FsAccess::ReadOnly);
    assert_eq!(Mode::ReadOnly.fs_access(), FsAccess::ReadOnly);
    assert_eq!(Mode::Ask.fs_access(), FsAccess::WorkspaceWrite);
    assert_eq!(Mode::Auto.fs_access(), FsAccess::WorkspaceWrite);
}

#[test]
fn auto_runs_unlisted_commands_when_a_sandbox_exists() {
    let dir = tempfile::tempdir().unwrap();
    let e = engine(Mode::Auto, dir.path(), true, RuleSet::default());
    assert_eq!(e.check(&bash("cargo build")), Decision::Allow);
    assert_eq!(
        e.check(&bash("echo hi > out.txt && cat out.txt")),
        Decision::Allow
    );
}

#[test]
fn read_only_and_plan_run_unlisted_commands_sandboxed() {
    let dir = tempfile::tempdir().unwrap();
    for mode in [Mode::Plan, Mode::ReadOnly] {
        assert_eq!(
            engine(mode, dir.path(), true, RuleSet::default()).check(&bash("ls -la")),
            Decision::Allow
        );
    }
}

#[test]
fn ask_mode_prompts_for_unlisted_but_not_allow_listed_commands() {
    let dir = tempfile::tempdir().unwrap();
    let e = engine(
        Mode::Ask,
        dir.path(),
        true,
        rules(&["bash:cargo test*"], &[], &[]),
    );
    assert!(is_ask(&e.check(&bash("cargo build"))));
    assert_eq!(e.check(&bash("cargo test --all")), Decision::Allow);
}

#[test]
fn without_a_sandbox_every_command_prompts_even_if_allow_listed() {
    let dir = tempfile::tempdir().unwrap();
    let e = engine(
        Mode::Auto,
        dir.path(),
        false,
        rules(&["bash:cargo test*"], &[], &[]),
    );
    let d = e.check(&bash("cargo test"));
    assert!(is_ask(&d));
    assert!(format!("{d:?}").contains("no sandbox"), "{d:?}");
}

#[test]
fn deny_rules_win_in_every_mode_including_full_access() {
    let dir = tempfile::tempdir().unwrap();
    for mode in [Mode::Plan, Mode::Ask, Mode::Auto, Mode::FullAccess] {
        let e = engine(
            mode,
            dir.path(),
            true,
            rules(&["bash:git *"], &["bash:git push*"], &[]),
        );
        assert!(is_deny(&e.check(&bash("git status && git push"))), "{mode}");
    }
}

#[test]
fn destructive_and_confirm_matches_prompt_outside_full_access() {
    let dir = tempfile::tempdir().unwrap();
    let e = engine(
        Mode::Auto,
        dir.path(),
        true,
        rules(&["bash:git *"], &[], &["bash:terraform apply*"]),
    );
    assert!(is_ask(&e.check(&bash("git reset --hard HEAD~1"))));
    assert!(is_ask(&e.check(&bash("terraform apply -auto-approve"))));
    let full = engine(Mode::FullAccess, dir.path(), true, RuleSet::default());
    assert_eq!(
        full.check(&bash("git reset --hard HEAD~1")),
        Decision::Allow
    );
}

#[test]
fn undecomposable_commands_prompt_in_auto() {
    let dir = tempfile::tempdir().unwrap();
    let e = engine(Mode::Auto, dir.path(), true, RuleSet::default());
    assert!(is_ask(&e.check(&bash("for f in *; do echo \"$f\"; done"))));
}

#[test]
fn write_rules_and_boundaries() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let e = engine(
        Mode::Ask,
        &ws,
        true,
        rules(&["write:docs/*"], &["write:secrets/*"], &[]),
    );
    assert_eq!(
        e.check(&Action::Write(PathBuf::from("docs/guide.md"))),
        Decision::Allow
    );
    assert!(is_ask(
        &e.check(&Action::Write(PathBuf::from("src/main.rs")))
    ));
    assert!(is_deny(
        &e.check(&Action::Write(PathBuf::from("secrets/key.pem")))
    ));
    assert!(is_ask(
        &e.check(&Action::Write(dir.path().join("outside.txt")))
    ));
    let auto = engine(Mode::Auto, &ws, true, RuleSet::default());
    assert_eq!(
        auto.check(&Action::Write(PathBuf::from("src/main.rs"))),
        Decision::Allow
    );
    assert!(is_ask(
        &auto.check(&Action::Write(PathBuf::from(".git/hooks/pre-commit")))
    ));
    let ro = engine(Mode::ReadOnly, &ws, true, RuleSet::default());
    assert!(is_deny(&ro.check(&Action::Write(PathBuf::from("a.txt")))));
}

#[test]
fn read_rules_and_boundaries() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let e = engine(Mode::Auto, &ws, true, rules(&[], &["read:.env"], &[]));
    assert_eq!(
        e.check(&Action::Read(PathBuf::from("src/lib.rs"))),
        Decision::Allow
    );
    assert!(is_deny(&e.check(&Action::Read(PathBuf::from(".env")))));
    assert!(is_ask(
        &e.check(&Action::Read(dir.path().join("elsewhere.txt")))
    ));
}

#[test]
fn approve_for_session_covers_the_same_prefix_only() {
    let dir = tempfile::tempdir().unwrap();
    let e = engine(Mode::Ask, dir.path(), true, RuleSet::default());
    assert!(is_ask(&e.check(&bash("cargo test"))));
    assert!(e.remember(&bash("cargo test")));
    assert_eq!(e.check(&bash("cargo test --all")), Decision::Allow);
    assert!(is_ask(&e.check(&bash("cargo publish"))));

    assert!(e.remember(&bash("git status")));
    assert!(is_ask(&e.check(&bash("git push"))));
}

#[test]
fn destructive_commands_are_never_remembered() {
    let dir = tempfile::tempdir().unwrap();
    let e = engine(Mode::Ask, dir.path(), true, RuleSet::default());
    assert!(!e.remember(&bash("git push --force")));
    assert!(is_ask(&e.check(&bash("git push --force"))));
}

#[test]
fn unknown_rule_tools_are_reported() {
    let dir = tempfile::tempdir().unwrap();
    let e = engine(
        Mode::Auto,
        dir.path(),
        true,
        rules(&["bash:ls*", "shell:rm*"], &["web:*"], &[]),
    );
    assert_eq!(e.unknown_rules(), ["shell:rm*", "web:*"]);
}
