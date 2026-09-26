use std::os::unix::fs::symlink;
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
    assert_eq!(Mode::FullAccess.fs_access(), FsAccess::WorkspaceWrite);
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
    for mode in [
        Mode::Plan,
        Mode::ReadOnly,
        Mode::Ask,
        Mode::Auto,
        Mode::FullAccess,
    ] {
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

// --- C1: full-access must still refuse a command that may match a deny rule ---

#[test]
fn full_access_asks_when_a_command_may_match_a_deny_rule() {
    let dir = tempfile::tempdir().unwrap();
    let full = engine(
        Mode::FullAccess,
        dir.path(),
        true,
        rules(&[], &["bash:curl*", "bash:git push*"], &[]),
    );
    for cmd in [
        "$(echo curl) https://x",
        "c=curl; $c https://x",
        "git $X origin",
        "echo curl x | sh",
        "git -c alias.p=push p",
    ] {
        assert!(is_ask(&full.check(&bash(cmd))), "{cmd}");
    }
}

#[test]
fn full_access_still_allows_unlisted_and_destructive_only_commands() {
    let dir = tempfile::tempdir().unwrap();
    let with_deny = engine(
        Mode::FullAccess,
        dir.path(),
        true,
        rules(&[], &["bash:curl*", "bash:git push*"], &[]),
    );
    assert_eq!(with_deny.check(&bash("cargo build")), Decision::Allow);
    let no_deny = engine(Mode::FullAccess, dir.path(), true, RuleSet::default());
    assert_eq!(
        no_deny.check(&bash("git reset --hard HEAD~1")),
        Decision::Allow
    );
}

// --- C2: deny/confirm path rules and the .git guard are case-insensitive ---

#[test]
fn deny_and_confirm_path_rules_are_case_insensitive() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let e = engine(
        Mode::Auto,
        &ws,
        true,
        rules(&[], &["read:.env", "write:secrets/*"], &[]),
    );
    assert!(is_deny(&e.check(&Action::Read(PathBuf::from(".ENV")))));
    assert!(is_deny(
        &e.check(&Action::Write(PathBuf::from("SECRETS/k")))
    ));
}

#[test]
fn git_guard_is_case_insensitive() {
    let dir = tempfile::tempdir().unwrap();
    let auto = engine(Mode::Auto, dir.path(), true, RuleSet::default());
    assert!(is_ask(
        &auto.check(&Action::Write(PathBuf::from(".GIT/hooks/pre-commit")))
    ));
}

// --- C3: absolute path rules resolve symlinked directories ---

#[test]
fn absolute_rules_follow_a_symlinked_directory() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let real = dir.path().join("real");
    std::fs::create_dir(&real).unwrap();
    let link = dir.path().join("link");
    symlink(&real, &link).unwrap();

    let deny_glob = format!("write:{}/*", link.display());
    let e = engine(
        Mode::FullAccess,
        &ws,
        true,
        rules(&[], &[deny_glob.as_str()], &[]),
    );
    // The rule is written against the symlink, but the target is reached through the real
    // path (as `resolve_path` would resolve any write, symlinked or not).
    assert!(is_deny(&e.check(&Action::Write(real.join("secret.txt")))));
}

#[test]
fn absolute_write_deny_resolves_a_symlinked_etc() {
    if !is_symlink("/etc") {
        return; // not a symlink on this host (e.g. most Linux); nothing to prove here.
    }
    let dir = tempfile::tempdir().unwrap();
    let full = engine(
        Mode::FullAccess,
        dir.path(),
        true,
        rules(&[], &["write:/etc/*"], &[]),
    );
    assert!(is_deny(
        &full.check(&Action::Write(PathBuf::from("/etc/hosts")))
    ));
}

#[test]
fn absolute_read_deny_resolves_a_symlinked_tmp() {
    if !is_symlink("/tmp") {
        return; // not a symlink on this host; nothing to prove here.
    }
    let dir = tempfile::tempdir().unwrap();
    let e = engine(
        Mode::Auto,
        dir.path(),
        true,
        rules(&[], &["read:/tmp/secret*"], &[]),
    );
    assert!(is_deny(
        &e.check(&Action::Read(PathBuf::from("/tmp/secret")))
    ));
}

fn is_symlink(path: &str) -> bool {
    std::fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

// --- C4: read confirm rules must be consulted ---

#[test]
fn read_confirm_rules_prompt_outside_full_access() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let ask = engine(Mode::Ask, &ws, true, rules(&[], &[], &["read:.env"]));
    assert!(is_ask(&ask.check(&Action::Read(PathBuf::from(".env")))));
    let auto = engine(Mode::Auto, &ws, true, rules(&[], &[], &["read:.env"]));
    assert!(is_ask(&auto.check(&Action::Read(PathBuf::from(".env")))));
    let full = engine(Mode::FullAccess, &ws, true, rules(&[], &[], &["read:.env"]));
    assert_eq!(
        full.check(&Action::Read(PathBuf::from(".env"))),
        Decision::Allow
    );
}

// --- I1: session path approvals are exact paths, not globs ---

#[test]
fn session_read_approval_is_an_exact_path_not_a_glob() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    let outside = dir.path().join("outside");
    std::fs::create_dir(&ws).unwrap();
    std::fs::create_dir(&outside).unwrap();
    let e = engine(Mode::Ask, &ws, true, RuleSet::default());
    let literal = outside.join("x*");
    assert!(is_ask(&e.check(&Action::Read(literal.clone()))));
    assert!(e.remember(&Action::Read(literal.clone())));
    // The remembered path is exact: it must not act as a glob over `x*`'s siblings.
    assert!(is_ask(&e.check(&Action::Read(outside.join("xy")))));
    assert_eq!(e.check(&Action::Read(literal)), Decision::Allow);
}

#[test]
fn bash_session_prefixes_containing_an_asterisk_are_not_remembered() {
    let dir = tempfile::tempdir().unwrap();
    let e = engine(Mode::Ask, dir.path(), true, RuleSet::default());
    assert!(!e.remember(&bash("cargo '*evil*'")));
}

// --- I2: `~` expands to $HOME in path rules ---

#[test]
fn tilde_expands_to_home_in_path_rules() {
    let home = PathBuf::from(std::env::var("HOME").expect("HOME must be set for this test"));
    let dir = tempfile::tempdir().unwrap();
    let e = engine(
        Mode::Auto,
        dir.path(),
        true,
        rules(&[], &["read:~/.ssh/*"], &[]),
    );
    assert!(is_deny(&e.check(&Action::Read(home.join(".ssh/id_rsa")))));
}

// --- M1: remember() is a no-op when it could not change a later decision ---

#[test]
fn remember_is_a_noop_without_a_sandbox() {
    let dir = tempfile::tempdir().unwrap();
    let e = engine(Mode::Ask, dir.path(), false, RuleSet::default());
    assert!(!e.remember(&bash("cargo test")));
}

#[test]
fn remember_is_a_noop_when_a_confirm_rule_would_still_ask() {
    let dir = tempfile::tempdir().unwrap();
    let e = engine(
        Mode::Ask,
        dir.path(),
        true,
        rules(&[], &[], &["bash:terraform apply*"]),
    );
    assert!(!e.remember(&bash("terraform apply -auto-approve")));
}

#[test]
fn remember_is_a_noop_for_a_write_inside_dot_git() {
    let dir = tempfile::tempdir().unwrap();
    let e = engine(Mode::Auto, dir.path(), true, RuleSet::default());
    assert!(!e.remember(&Action::Write(PathBuf::from(".git/hooks/pre-commit"))));
    assert!(is_ask(
        &e.check(&Action::Write(PathBuf::from(".git/hooks/pre-commit")))
    ));
}

// --- M3: additional decision-matrix coverage ---

#[test]
fn deny_beats_a_session_approval() {
    let dir = tempfile::tempdir().unwrap();
    let e = engine(
        Mode::Ask,
        dir.path(),
        true,
        rules(&[], &["bash:curl evil.com*"], &[]),
    );
    assert!(e.remember(&bash("curl https://example.com")));
    assert!(is_deny(&e.check(&bash("curl evil.com/x"))));
}

#[test]
fn confirm_beats_a_session_approval() {
    let dir = tempfile::tempdir().unwrap();
    let e = engine(
        Mode::Ask,
        dir.path(),
        true,
        rules(&[], &[], &["bash:terraform apply*"]),
    );
    // "terraform" isn't a subcommand tool, so approving "terraform plan" remembers the
    // broad, bare "terraform" prefix for the session.
    assert!(e.remember(&bash("terraform plan")));
    assert_eq!(e.check(&bash("terraform plan")), Decision::Allow);
    // The confirm rule still catches a different terraform invocation.
    assert!(is_ask(&e.check(&bash("terraform apply -auto-approve"))));
}

#[test]
fn remembered_prefix_does_not_match_a_longer_word() {
    let dir = tempfile::tempdir().unwrap();
    let e = engine(Mode::Ask, dir.path(), true, RuleSet::default());
    assert!(e.remember(&bash("cargo test")));
    assert_eq!(e.check(&bash("cargo test --all")), Decision::Allow);
    assert!(is_ask(&e.check(&bash("cargo testx"))));
}
