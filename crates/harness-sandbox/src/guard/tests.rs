//! Guard tests that need to reach inside it: a scan that runs out of budget,
//! a process that writes while the guard scans, a move that fails, and how
//! much work a check does.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::*;
use crate::gitmeta::Budget;

struct Env {
    _dir: tempfile::TempDir,
    ws: PathBuf,
    quarantine: PathBuf,
    session: Arc<GuardSession>,
}

/// A workspace holding a repository with `config`, `HEAD` and one hook.
fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let ws = base.join("ws");
    std::fs::create_dir_all(ws.join(".git/hooks")).unwrap();
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

const EVIL: &str = "[core]\n\tfsmonitor = /tmp/evil\n";

/// Sets what runs once right after the next scan of the workspace: while
/// `finish` scans, a process the command left running writes.
fn while_finish_scans(env: &Env) {
    let ws = env.ws.clone();
    lock(&env.session.hooks).after_scan = Some(Box::new(move || {
        std::fs::write(ws.join(".git/commondir"), "/tmp/evil\n").unwrap();
        std::fs::write(ws.join(".git/config"), EVIL).unwrap();
    }));
}

#[test]
fn with_survivors_what_is_written_while_finish_scans_is_undone_at_the_next_begin() {
    let env = env();
    env.session.set_survivor_probe(Arc::new(|| true));
    let guard = env.session.begin(&env.ws, true, |_| {});
    while_finish_scans(&env);
    assert_eq!(guard.finish(), None);
    assert_eq!(
        read(&env.ws.join(".git/config")),
        EVIL,
        "written after the check"
    );
    let report = env
        .session
        .begin(&env.ws, true, |_| {})
        .finish()
        .expect("a report");
    assert!(!report.blocked, "{}", report.message);
    assert!(
        report
            .message
            .contains("\n- .git/commondir: new; moved to "),
        "{}",
        report.message
    );
    assert!(
        report
            .message
            .contains("\n- .git/config: changed; restored the earlier version"),
        "{}",
        report.message
    );
    assert!(!env.ws.join(".git/commondir").exists());
    assert_eq!(
        read(&env.ws.join(".git/config")),
        "[core]\n\tbare = false\n"
    );
    // And it stays that way.
    assert_eq!(env.session.begin(&env.ws, true, |_| {}).finish(), None);
}

#[test]
fn without_survivors_a_name_planted_while_finish_scans_is_still_caught_at_the_next_begin() {
    let env = env();
    let guard = env.session.begin(&env.ws, true, |_| {});
    while_finish_scans(&env);
    assert_eq!(guard.finish(), None);
    let report = env
        .session
        .begin(&env.ws, true, |_| {})
        .finish()
        .expect("a report");
    assert!(!report.blocked, "{}", report.message);
    assert!(
        report
            .message
            .contains("\n- .git/commondir: new; moved to ")
    );
    assert!(!env.ws.join(".git/commondir").exists());
    // Without survivors, a change to an existing file between commands is
    // the user's.
    assert_eq!(read(&env.ws.join(".git/config")), EVIL);
}

#[test]
fn a_name_the_guard_could_not_move_is_tried_again_at_the_next_begin() {
    let env = env();
    let commondir = env.ws.join(".git/commondir");
    lock(&env.session.hooks).stuck.insert(commondir.clone());
    let guard = env.session.begin(&env.ws, true, |_| {});
    std::fs::write(&commondir, "/tmp/evil\n").unwrap();
    let report = guard.finish().expect("a report");
    assert!(report.blocked);
    assert!(
        report
            .message
            .contains("\n- .git/commondir: new; could not move it"),
        "{}",
        report.message
    );
    assert!(commondir.exists());
    lock(&env.session.hooks).stuck.clear();
    let report = env
        .session
        .begin(&env.ws, true, |_| {})
        .finish()
        .expect("a report");
    assert!(
        report
            .message
            .contains("\n- .git/commondir: new; moved to "),
        "{}",
        report.message
    );
    assert!(!commondir.exists());
}

/// A repository with a submodule and a linked worktree gitdir, next to a
/// directory whose listing runs a small scan budget out, so the scan never
/// reaches `modules/` or `worktrees/`.
fn incomplete_env() -> Env {
    let env = env();
    std::fs::create_dir_all(env.ws.join(".git/modules/sub/hooks")).unwrap();
    std::fs::write(env.ws.join(".git/modules/sub/HEAD"), "ref: x\n").unwrap();
    std::fs::create_dir_all(env.ws.join(".git/worktrees/wt")).unwrap();
    std::fs::write(env.ws.join(".git/worktrees/wt/HEAD"), "ref: x\n").unwrap();
    std::fs::write(env.ws.join(".git/worktrees/wt/commondir"), "../..\n").unwrap();
    // A worktree of the submodule, which only a look into the submodule's
    // gitdir finds.
    std::fs::create_dir_all(env.ws.join(".git/modules/sub/worktrees/w")).unwrap();
    std::fs::create_dir(env.ws.join("big")).unwrap();
    for i in 0..50 {
        std::fs::write(env.ws.join(format!("big/f{i}")), "").unwrap();
    }
    env.session.prime(&env.ws);
    lock(&env.session.hooks).budget = Some(Budget {
        entries: 20,
        ..Budget::DEFAULT
    });
    env
}

#[test]
fn after_an_incomplete_scan_known_submodule_and_worktree_gitdirs_are_left_alone() {
    let env = incomplete_env();
    let guard = env.session.begin(&env.ws, true, |_| {});
    let index = guard.index();
    assert!(index.incomplete, "{index:?}");
    assert!(
        !index.gitdirs.contains(&env.ws.join(".git/modules/sub")),
        "{index:?}"
    );
    let report = guard.finish().expect("the incomplete scan is reported");
    assert!(!report.blocked, "{}", report.message);
    assert!(!report.message.contains("moved to"), "{}", report.message);
    assert!(
        !report.message.contains("found, not checked"),
        "they are known: {}",
        report.message
    );
    assert!(env.ws.join(".git/modules/sub/HEAD").exists());
    assert!(env.ws.join(".git/worktrees/wt/commondir").exists());
    assert!(!env.quarantine.exists());
}

#[test]
fn after_an_incomplete_scan_a_new_nested_gitdir_is_listed_and_left_in_place() {
    let env = incomplete_env();
    let guard = env.session.begin(&env.ws, true, |_| {});
    std::fs::create_dir_all(env.ws.join(".git/worktrees/new")).unwrap();
    let report = guard.finish().expect("a report");
    assert!(!report.blocked, "{}", report.message);
    assert!(
        report.message.contains("found, not checked"),
        "{}",
        report.message
    );
    assert!(
        report.message.contains("\n- .git/worktrees/new"),
        "{}",
        report.message
    );
    assert!(env.ws.join(".git/worktrees/new").is_dir());
}

#[test]
fn after_an_incomplete_scan_names_planted_in_known_nested_gitdirs_are_still_caught() {
    let env = incomplete_env();
    let guard = env.session.begin(&env.ws, true, |_| {});
    let planted = env.ws.join(".git/modules/sub/commondir");
    std::fs::write(&planted, "/tmp/evil\n").unwrap();
    let report = guard.finish().expect("a report");
    assert!(report.blocked, "{}", report.message);
    assert!(
        report
            .message
            .contains("\n- .git/modules/sub/commondir: new; moved to "),
        "{}",
        report.message
    );
    assert!(!planted.exists());
}

#[test]
fn twenty_thousand_planted_hooks_take_bounded_work_and_a_bounded_report() {
    let env = env();
    let guard = env.session.begin(&env.ws, true, |_| {});
    let hooks = env.ws.join(".git/hooks");
    for i in 0..20_000 {
        std::fs::write(hooks.join(format!("h{i:05}")), "x").unwrap();
    }
    let state = Arc::clone(&guard.state);
    let report = guard.finish().expect("a report");
    assert!(report.blocked);
    // At most 10,000 moves in one check; the rest are reported, not moved.
    let left = std::fs::read_dir(&hooks).unwrap().count();
    assert_eq!(left, 10_001, "the rest and pre-commit are left");
    assert!(
        report
            .message
            .contains("more than 10000 changes in one check; 10000 more were left as they are"),
        "{}",
        report.message
    );
    // 50 lines, then a count.
    assert!(
        report.message.contains("\n- and 9950 more]"),
        "{}",
        report.message
    );
    assert!(report.message.lines().count() < 70, "{}", report.message);
    assert!(
        report.message.len() < 20_000,
        "{} bytes",
        report.message.len()
    );
    // A free name in quarantine is found at the first try, not by a search.
    let probes = lock(&state).quarantine.probes();
    assert!(probes <= 10_000, "{probes} names tried for 10,000 moves");
    // The next command's guard goes on where this one stopped.
    let report = env
        .session
        .begin(&env.ws, true, |_| {})
        .finish()
        .expect("a report");
    assert!(
        report.message.starts_with("[before this command ran"),
        "{}",
        report.message
    );
    assert_eq!(std::fs::read_dir(&hooks).unwrap().count(), 1);
}

#[test]
fn the_same_name_planted_again_and_again_is_not_searched_for_a_free_name() {
    let env = env();
    let guard = env.session.begin(&env.ws, true, |_| {});
    let handle = guard.watch_handle();
    for _ in 0..500 {
        std::fs::write(env.ws.join("HEAD"), "ref: x\n").unwrap();
        handle.check();
    }
    let probes = lock(&guard.state).quarantine.probes();
    assert!(probes <= 2 * 500, "{probes} names tried for 500 moves");
    let report = guard.finish().expect("a report");
    assert_eq!(
        report.message.matches("\n- HEAD: new; moved to ").count(),
        50
    );
    assert!(
        report.message.contains("\n- and 450 more]"),
        "{}",
        report.message
    );
}
