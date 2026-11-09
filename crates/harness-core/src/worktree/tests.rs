use std::path::Path;

use super::*;
use crate::{
    agents::ChildWorkspace,
    worktree::testing::{git, init_repo, write},
};

struct Fixture {
    repo: tempfile::TempDir,
    home: tempfile::TempDir,
    wts: Worktrees,
}

fn fixture() -> Fixture {
    let repo = tempfile::tempdir().unwrap();
    init_repo(repo.path());
    let home = tempfile::tempdir().unwrap();
    let wts = Worktrees::new(repo.path(), home.path().join("proj")).unwrap();
    Fixture { repo, home, wts }
}

fn request<'a>(agent: &'a str, label: &'a str, task: &'a str) -> Request<'a> {
    Request { agent, label, task }
}

fn dead_pid() -> u32 {
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

#[test]
fn a_worktree_is_made_on_its_own_branch_from_head_locked_and_marked() {
    let f = fixture();
    let made = f
        .wts
        .create(&request("a1", "general", "fix the parser"))
        .unwrap();
    let wt = &made.worktree;
    assert!(
        wt.branch.starts_with("harness/general-fix-the-parser"),
        "{}",
        wt.branch
    );
    assert!(wt.path.starts_with(f.home.path().join("proj")));
    assert!(wt.path.join("src/lib.rs").exists());
    let head = git(f.repo.path(), &["rev-parse", "HEAD"]);
    assert_eq!(wt.base, head.trim());
    assert_eq!(git(&wt.path, &["rev-parse", "HEAD"]).trim(), wt.base);
    assert_eq!(
        git(&wt.path, &["branch", "--show-current"]).trim(),
        wt.branch
    );
    assert!(f.wts.lock_reason(wt).unwrap().contains("harness pid"));
    assert!(f.wts.is_marked(&wt.path));
    assert!(
        f.home
            .path()
            .join("proj")
            .join(format!("{}.json", wt.name))
            .exists()
    );
    assert_eq!(made.uncommitted, 0);
}

#[test]
fn the_base_is_the_local_head_and_uncommitted_changes_are_counted_not_copied() {
    let f = fixture();
    // An unpushed commit (there is no remote at all) and work that is not committed.
    write(f.repo.path(), "new.txt", "committed\n");
    git(f.repo.path(), &["add", "-A"]);
    git(f.repo.path(), &["commit", "-q", "-m", "unpushed"]);
    write(f.repo.path(), "README.md", "edited\n");
    write(f.repo.path(), "scratch.txt", "untracked\n");
    let made = f.wts.create(&request("a1", "general", "task")).unwrap();
    assert!(made.worktree.path.join("new.txt").exists());
    assert_eq!(
        std::fs::read_to_string(made.worktree.path.join("README.md")).unwrap(),
        "hello\n"
    );
    assert!(!made.worktree.path.join("scratch.txt").exists());
    assert_eq!(made.uncommitted, 2);
    assert!(made.said_uncommitted, "the first worktree says so");
    let second = f.wts.create(&request("a2", "general", "task")).unwrap();
    assert!(!second.said_uncommitted, "and it is said once");
    assert_eq!(second.uncommitted, 2);
}

#[test]
fn names_are_safe_and_unique() {
    let f = fixture();
    let odd = f
        .wts
        .create(&request(
            "a1",
            "../../etc/passwd",
            "rm -rf / && echo $(whoami)",
        ))
        .unwrap();
    assert!(!odd.worktree.name.contains('/') && !odd.worktree.name.contains('.'));
    assert!(odd.worktree.name.len() <= 40, "{}", odd.worktree.name);
    assert!(odd.worktree.path.starts_with(f.home.path().join("proj")));
    let one = f
        .wts
        .create(&request("a2", "general", "same task"))
        .unwrap();
    let two = f
        .wts
        .create(&request("a3", "general", "same task"))
        .unwrap();
    assert_ne!(one.worktree.name, two.worktree.name);
    assert_ne!(one.worktree.branch, two.worktree.branch);
}

#[test]
fn a_name_taken_by_a_branch_of_the_users_is_not_used() {
    let f = fixture();
    git(f.repo.path(), &["branch", "harness/general-task"]);
    let made = f.wts.create(&request("a1", "general", "task")).unwrap();
    assert_ne!(made.worktree.branch, "harness/general-task");
    // The user's branch is untouched by removing ours.
    f.wts.release(&made.worktree).unwrap();
    f.wts.remove(&made.worktree.name).unwrap();
    assert!(
        git(f.repo.path(), &["branch", "--list", "harness/general-task"])
            .contains("harness/general-task")
    );
}

#[test]
fn removing_takes_the_worktree_branch_and_record_and_never_a_locked_one() {
    let f = fixture();
    let wt = f
        .wts
        .create(&request("a1", "general", "task"))
        .unwrap()
        .worktree;
    let refused = f.wts.remove(&wt.name).unwrap_err();
    assert!(matches!(refused, WorktreeError::Locked { .. }), "{refused}");
    assert!(wt.path.exists(), "a running agent's worktree stays");
    f.wts.release(&wt).unwrap();
    f.wts.remove(&wt.name).unwrap();
    assert!(!wt.path.exists());
    assert!(
        git(f.repo.path(), &["branch", "--list", &wt.branch])
            .trim()
            .is_empty()
    );
    assert!(
        !f.home
            .path()
            .join("proj")
            .join(format!("{}.json", wt.name))
            .exists()
    );
    assert!(!git(f.repo.path(), &["worktree", "list"]).contains(&wt.name));
}

#[test]
fn only_harnesss_own_worktrees_are_removed() {
    let f = fixture();
    // A worktree the user made, in the place harness would use, with a record forged for it.
    let own = f.home.path().join("proj/forged");
    git(
        f.repo.path(),
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "harness/forged",
            own.to_str().unwrap(),
        ],
    );
    let mut forged = Record::new(
        "forged",
        "harness/forged",
        &own,
        "base",
        "a9",
        "general",
        "task",
    );
    forged.pid = std::process::id();
    f.wts.write_record(&forged).unwrap();
    let refused = f.wts.remove("forged").unwrap_err();
    assert!(
        matches!(refused, WorktreeError::NotOurs { .. }),
        "{refused}"
    );
    assert!(own.exists());
    assert!(
        !git(f.repo.path(), &["branch", "--list", "harness/forged"])
            .trim()
            .is_empty()
    );
    // A name that is no record at all.
    let unknown = f.wts.remove("does-not-exist").unwrap_err();
    assert!(
        matches!(unknown, WorktreeError::NotOurs { .. }),
        "{unknown}"
    );
    // A record that points outside harness's directory.
    let outside = f.repo.path().to_path_buf();
    let mut escape = Record::new("escape", "harness/escape", &outside, "base", "a8", "g", "t");
    escape.pid = 1;
    f.wts.write_record(&escape).unwrap();
    assert!(matches!(
        f.wts.remove("escape").unwrap_err(),
        WorktreeError::NotOurs { .. }
    ));
    assert!(outside.join("README.md").exists());
}

#[test]
fn a_lock_of_a_dead_harness_is_released_and_other_locks_are_not() {
    let f = fixture();
    let dead = f
        .wts
        .create(&request("a1", "general", "dead"))
        .unwrap()
        .worktree;
    let live = f
        .wts
        .create(&request("a2", "general", "live"))
        .unwrap()
        .worktree;
    let foreign = f
        .wts
        .create(&request("a3", "general", "foreign"))
        .unwrap()
        .worktree;
    git(&dead.path, &["worktree", "unlock", "."]);
    git(
        &dead.path,
        &[
            "worktree",
            "lock",
            "--reason",
            &format!("harness pid {}", dead_pid()),
            ".",
        ],
    );
    git(&foreign.path, &["worktree", "unlock", "."]);
    git(
        &foreign.path,
        &["worktree", "lock", "--reason", "my own lock", "."],
    );
    let released = f.wts.release_stale_locks();
    assert_eq!(released, vec![dead.name.clone()]);
    assert!(f.wts.lock_reason(&dead).is_none());
    assert!(f.wts.lock_reason(&live).is_some(), "ours and alive");
    assert_eq!(f.wts.lock_reason(&foreign).as_deref(), Some("my own lock"));
}

#[test]
fn port_offsets_differ_between_live_worktrees_and_are_reused_after_removal() {
    let f = fixture();
    let a = f
        .wts
        .create(&request("a1", "general", "one"))
        .unwrap()
        .worktree;
    let b = f
        .wts
        .create(&request("a2", "general", "two"))
        .unwrap()
        .worktree;
    assert_ne!(a.port_offset, b.port_offset);
    assert!(a.port_offset >= 100);
    f.wts.release(&a).unwrap();
    f.wts.remove(&a.name).unwrap();
    let c = f
        .wts
        .create(&request("a3", "general", "three"))
        .unwrap()
        .worktree;
    assert_eq!(c.port_offset, a.port_offset);
}

#[test]
fn what_a_command_in_the_worktree_is_told() {
    let f = fixture();
    let wt = f
        .wts
        .create(&request("a7", "general", "task"))
        .unwrap()
        .worktree;
    let env = wt.env();
    assert!(env.contains(&("HARNESS_AGENT_ID".to_string(), "a7".to_string())));
    assert!(env.contains(&(
        "HARNESS_PORT_OFFSET".to_string(),
        wt.port_offset.to_string()
    )));
}

#[test]
fn only_a_made_worktree_gives_a_child_workspace() {
    let f = fixture();
    let wt = f
        .wts
        .create(&request("a1", "general", "task"))
        .unwrap()
        .worktree;
    let workspace = ChildWorkspace::worktree(&wt);
    assert_eq!(workspace.resolve(Path::new("/parent")), wt.path);
}

#[test]
fn a_directory_that_is_no_repository_or_has_no_commit_is_refused() {
    let plain = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let err = Worktrees::new(plain.path(), home.path().join("p"))
        .and_then(|w| w.create(&request("a1", "g", "t")).map(|_| ()))
        .unwrap_err();
    assert!(matches!(err, WorktreeError::NotARepository(_)), "{err}");
    let empty = tempfile::tempdir().unwrap();
    git(empty.path(), &["init", "-q", "-b", "main"]);
    let err = Worktrees::new(empty.path(), home.path().join("q"))
        .unwrap()
        .create(&request("a1", "g", "t"))
        .map(|_| ())
        .unwrap_err();
    assert!(matches!(err, WorktreeError::NoCommits), "{err}");
}

#[test]
fn a_session_started_below_the_root_works_in_the_same_place_in_the_worktree() {
    let repo = tempfile::tempdir().unwrap();
    init_repo(repo.path());
    let home = tempfile::tempdir().unwrap();
    let wts = Worktrees::new(&repo.path().join("src"), home.path().join("p")).unwrap();
    let wt = wts
        .create(&request("a1", "general", "task"))
        .unwrap()
        .worktree;
    assert_eq!(wt.workspace(), wt.path.join("src"));
    assert!(wt.workspace().join("lib.rs").exists());
    assert_eq!(
        wts.root().canonicalize().unwrap(),
        repo.path().canonicalize().unwrap()
    );
}
