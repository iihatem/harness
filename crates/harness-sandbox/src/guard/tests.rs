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
            .contains("[harness stops at 10000 changes in one check, and left these as they are"),
        "{}",
        report.message
    );
    assert!(
        report
            .message
            .contains("\n- .git/hooks/h10000: new in a protected directory"),
        "{}",
        report.message
    );
    // 50 lines, then a count, in each section.
    assert_eq!(
        report.message.matches("\n- and 9950 more]").count(),
        2,
        "{}",
        report.message
    );
    assert!(report.message.lines().count() < 120, "{}", report.message);
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

/// Sets the most changes one check makes.
fn cap(env: &Env, max: usize) {
    lock(&env.session.hooks).max_changes = Some(max);
}

#[test]
fn a_repository_past_the_cap_is_named_and_moved_before_the_next_command() {
    let env = env();
    cap(&env, 3);
    let guard = env.session.begin(&env.ws, true, |_| {});
    for decoy in ["a1", "a2", "a3"] {
        std::fs::create_dir_all(env.ws.join(decoy).join(".git")).unwrap();
    }
    std::fs::create_dir_all(env.ws.join("zz/.git/hooks")).unwrap();
    std::fs::write(env.ws.join("zz/.git/config"), EVIL).unwrap();
    let report = guard.finish().expect("a report");
    assert!(report.blocked);
    assert!(
        report.message.contains(
            "[harness stops at 3 changes in one check, and left these as they are; it goes on with them before the next command, and the command counts as blocked:\n- zz/.git: a new repository]"
        ),
        "{}",
        report.message
    );
    assert!(env.ws.join("zz/.git").exists());
    let report = env
        .session
        .begin(&env.ws, true, |_| {})
        .finish()
        .expect("a report");
    assert!(!report.blocked, "{}", report.message);
    assert!(
        report.message.starts_with(&format!(
            "{}\n- zz/.git: a new repository; moved to ",
            report::BEFORE
        )),
        "{}",
        report.message
    );
    assert!(!env.ws.join("zz/.git").exists());
}

#[test]
fn a_nested_gitdir_past_the_cap_is_moved_before_the_next_command() {
    let env = env();
    cap(&env, 3);
    let guard = env.session.begin(&env.ws, true, |_| {});
    let worktrees = env.ws.join(".git/worktrees");
    for name in ["a1", "a2", "a3", "zz"] {
        std::fs::create_dir_all(worktrees.join(name)).unwrap();
        std::fs::write(worktrees.join(name).join("config.worktree"), EVIL).unwrap();
    }
    let report = guard.finish().expect("a report");
    // The gitdirs are found in directory order: one of them is left.
    let left: Vec<String> = std::fs::read_dir(&worktrees)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(left.len(), 1, "{left:?}");
    let rel = format!(".git/worktrees/{}", left[0]);
    assert!(
        report
            .message
            .contains(&format!("\n- {rel}: a new worktree or submodule gitdir]")),
        "{}",
        report.message
    );
    let report = env
        .session
        .begin(&env.ws, true, |_| {})
        .finish()
        .expect("a report");
    assert!(
        report.message.contains(&format!(
            "\n- {rel}: a new worktree or submodule gitdir; moved to "
        )),
        "{}",
        report.message
    );
    assert_eq!(std::fs::read_dir(&worktrees).unwrap().count(), 0);
}

#[test]
fn what_is_left_undone_between_commands_is_moved_while_survivors_live() {
    let env = env();
    cap(&env, 3);
    env.session.set_survivor_probe(Arc::new(|| true));
    let guard = env.session.begin(&env.ws, true, |_| {});
    for name in ["a1", "a2", "a3", "zz"] {
        std::fs::create_dir_all(env.ws.join(name).join(".git")).unwrap();
    }
    assert!(guard.finish().expect("a report").blocked);
    let left = env.ws.join("zz/.git");
    assert!(left.exists(), "the fourth is past the cap");
    env.session
        .between_commands(&env.ws)
        .expect("survivors")
        .check();
    assert!(!left.exists());
}

#[test]
fn what_is_left_undone_stays_unknown_until_it_is_moved() {
    // The move before the next command fails too: its scan finds the
    // repository, which must still count as new.
    let env = env();
    cap(&env, 1);
    let planted = env.ws.join("zz/.git");
    let guard = env.session.begin(&env.ws, true, |_| {});
    std::fs::create_dir_all(env.ws.join("a1/.git")).unwrap();
    std::fs::create_dir_all(&planted).unwrap();
    assert!(guard.finish().expect("a report").blocked);
    lock(&env.session.hooks).max_changes = None;
    lock(&env.session.hooks).stuck.insert(planted.clone());
    let guard = env.session.begin(&env.ws, true, |_| {});
    assert!(!guard.index().dot_gits.contains(&planted));
    let report = guard.finish().expect("a report");
    assert!(report.blocked, "{}", report.message);
    assert!(
        report
            .message
            .contains("\n- zz/.git: a new repository; could not move it"),
        "{}",
        report.message
    );
    lock(&env.session.hooks).stuck.clear();
    let report = env
        .session
        .begin(&env.ws, true, |_| {})
        .finish()
        .expect("a report");
    assert!(
        report
            .message
            .contains("\n- zz/.git: a new repository; moved to "),
        "{}",
        report.message
    );
    assert!(!planted.exists());
}

/// Lets the next command's guard make as many changes as it likes.
fn uncap(env: &Env) {
    lock(&env.session.hooks).max_changes = None;
}

#[test]
fn a_capped_config_change_is_restored_at_the_next_begin() {
    let env = env();
    cap(&env, 0);
    let guard = env.session.begin(&env.ws, true, |_| {});
    std::fs::write(env.ws.join(".git/config"), EVIL).unwrap();
    let report = guard.finish().expect("a report");
    assert!(
        report.message.contains("\n- .git/config: changed]"),
        "{}",
        report.message
    );
    uncap(&env);
    let report = env
        .session
        .begin(&env.ws, true, |_| {})
        .finish()
        .expect("a report");
    assert!(
        report.message.contains(
            "\n- .git/config: changed; restored the earlier version (the changed one is in "
        ),
        "{}",
        report.message
    );
    assert_eq!(
        read(&env.ws.join(".git/config")),
        "[core]\n\tbare = false\n"
    );
    assert_eq!(env.session.begin(&env.ws, true, |_| {}).finish(), None);
}

#[test]
fn a_failed_config_move_is_retried_at_the_next_begin() {
    let env = env();
    let config = env.ws.join(".git/config");
    lock(&env.session.hooks).stuck.insert(config.clone());
    let guard = env.session.begin(&env.ws, true, |_| {});
    std::fs::write(&config, EVIL).unwrap();
    let report = guard.finish().expect("a report");
    assert!(report.blocked);
    assert!(
        report
            .message
            .contains("\n- .git/config: changed; could not move it"),
        "{}",
        report.message
    );
    lock(&env.session.hooks).stuck.clear();
    let report = env
        .session
        .begin(&env.ws, true, |_| {})
        .finish()
        .expect("a report");
    assert!(
        report.message.contains(
            "\n- .git/config: changed; restored the earlier version (the changed one is in "
        ),
        "{}",
        report.message
    );
    assert_eq!(read(&config), "[core]\n\tbare = false\n");
}

#[test]
fn a_replaced_gitfile_left_undone_is_put_back_at_the_next_begin() {
    let env = env();
    std::fs::create_dir_all(env.ws.join(".git/modules/sub")).unwrap();
    std::fs::write(env.ws.join(".git/modules/sub/HEAD"), "ref: x\n").unwrap();
    std::fs::create_dir_all(env.ws.join("sub")).unwrap();
    let gitfile = env.ws.join("sub/.git");
    std::fs::write(&gitfile, "gitdir: ../.git/modules/sub\n").unwrap();
    cap(&env, 0);
    let guard = env.session.begin(&env.ws, true, |_| {});
    // Renamed over it, so its inode differs whatever the filesystem reuses.
    std::fs::write(env.ws.join("sub/new"), "gitdir: /tmp/evil\n").unwrap();
    std::fs::rename(env.ws.join("sub/new"), &gitfile).unwrap();
    let report = guard.finish().expect("a report");
    assert!(
        report.message.contains("\n- sub/.git: moved or replaced]"),
        "{}",
        report.message
    );
    uncap(&env);
    let report = env
        .session
        .begin(&env.ws, true, |_| {})
        .finish()
        .expect("a report");
    assert!(
        report.message.contains(
            "\n- sub/.git: moved or replaced; restored the earlier version (the changed one is in "
        ),
        "{}",
        report.message
    );
    assert_eq!(read(&gitfile), "gitdir: ../.git/modules/sub\n");
    assert_eq!(env.session.begin(&env.ws, true, |_| {}).finish(), None);
}

#[test]
fn a_capped_restore_in_the_full_tier_is_done_at_the_next_command() {
    let env = env();
    let hooks = env.ws.join(".git/hooks");
    std::fs::rename(&hooks, env.ws.join("tracked-hooks")).unwrap();
    std::os::unix::fs::symlink("../tracked-hooks", &hooks).unwrap();
    cap(&env, 0);
    let guard = env.session.begin(&env.ws, false, |_| {});
    std::fs::remove_file(&hooks).unwrap();
    let report = guard.finish().expect("a report");
    assert!(report.blocked);
    assert!(
        report.message.contains("\n- .git/hooks: deleted]"),
        "{}",
        report.message
    );
    uncap(&env);
    let report = env
        .session
        .begin(&env.ws, false, |_| {})
        .finish()
        .expect("a report");
    assert!(!report.blocked, "{}", report.message);
    assert!(
        report
            .message
            .contains("\n- .git/hooks: deleted; restored the earlier version"),
        "{}",
        report.message
    );
    assert_eq!(
        std::fs::read_link(&hooks).unwrap(),
        PathBuf::from("../tracked-hooks")
    );
    assert_eq!(env.session.begin(&env.ws, false, |_| {}).finish(), None);
}

#[test]
fn a_restore_nothing_can_do_is_reported_once_then_dropped() {
    let env = env();
    std::fs::create_dir_all(env.ws.join("sub/.git/hooks")).unwrap();
    std::fs::write(env.ws.join("sub/.git/HEAD"), "ref: x\n").unwrap();
    std::fs::write(env.ws.join("sub/.git/config"), "[core]\n").unwrap();
    cap(&env, 0);
    let guard = env.session.begin(&env.ws, true, |_| {});
    std::fs::write(env.ws.join("sub/.git/config"), EVIL).unwrap();
    assert!(guard.finish().expect("a report").blocked);
    // The user removes the repository between commands.
    std::fs::remove_dir_all(env.ws.join("sub")).unwrap();
    uncap(&env);
    let report = env
        .session
        .begin(&env.ws, true, |_| {})
        .finish()
        .expect("a report");
    assert_eq!(
        report
            .message
            .matches("\n- sub/.git/config: changed; ")
            .count(),
        1,
        "{}",
        report.message
    );
    assert!(
        report
            .message
            .contains("harness can no longer put the earlier version back"),
        "{}",
        report.message
    );
    assert_eq!(env.session.begin(&env.ws, true, |_| {}).finish(), None);
}

/// How many lookups in the undone record the next `begin` makes, with `n`
/// planted hooks left undone.
fn lookups_at_begin(n: usize) -> usize {
    let env = env();
    cap(&env, 0);
    let guard = env.session.begin(&env.ws, true, |_| {});
    for i in 0..n {
        std::fs::write(env.ws.join(format!(".git/hooks/h{i:05}")), "x").unwrap();
    }
    let _ = guard.finish();
    LOOKUPS.with(|looks| looks.set(0));
    let guard = env.session.begin(&env.ws, true, |_| {});
    let looks = LOOKUPS.with(std::cell::Cell::get);
    assert_eq!(lock(&guard.state).undone.len(), n);
    looks
}

#[test]
fn the_work_at_begin_grows_linearly_with_what_was_left_undone() {
    let small = lookups_at_begin(500);
    let large = lookups_at_begin(2000);
    eprintln!("lookups at begin: {small} for 500 left undone, {large} for 2000");
    assert!(small > 0);
    assert!(
        large <= small * 5,
        "{small} lookups for 500, {large} for 2000"
    );
}

#[test]
fn a_move_that_keeps_failing_blocks_only_the_first_command() {
    let env = env();
    let planted = env.ws.join("zz/.git");
    lock(&env.session.hooks).stuck.insert(planted.clone());
    let guard = env.session.begin(&env.ws, true, |_| {});
    std::fs::create_dir_all(&planted).unwrap();
    let report = guard.finish().expect("a report");
    assert!(report.blocked);
    assert!(
        report
            .message
            .contains("\n- zz/.git: a new repository; could not move it"),
        "{}",
        report.message
    );
    for _ in 0..2 {
        let report = env
            .session
            .begin(&env.ws, true, |_| {})
            .finish()
            .expect("a reminder");
        assert!(!report.blocked, "{}", report.message);
        assert_eq!(
            report.message,
            "[1 entry harness could not move or restore is still as it was: zz/.git]\n"
        );
    }
    lock(&env.session.hooks).stuck.clear();
    let report = env
        .session
        .begin(&env.ws, true, |_| {})
        .finish()
        .expect("a report");
    assert!(
        report
            .message
            .contains("\n- zz/.git: a new repository; moved to "),
        "{}",
        report.message
    );
    assert!(!planted.exists());
}

#[test]
fn an_entry_born_again_under_the_same_inode_is_another() {
    let inode = |birth| Identity::Inode {
        dir: false,
        dev: 1,
        ino: 7,
        birth,
    };
    assert_eq!(inode(Some((10, 0))), inode(Some((10, 0))));
    assert_ne!(inode(Some((10, 0))), inode(Some((10, 1))));
    // Where the system keeps no birth time, only the inode tells.
    assert_eq!(inode(None), inode(Some((10, 0))));
    assert_eq!(inode(Some((10, 0))), inode(None));
}
