//! M3d: `harness worktrees` lists what harness made, and `clean` removes only its own: never a
//! running agent's, never one the user made, never the user's branches; results not applied stay
//! unless `--all` says they may go.

mod common;
use common::Isolate;

use std::time::Duration;

use assert_cmd::Command;
use harness_context::project::{project_key, project_root};
use harness_core::{
    agents::AgentStatus,
    worktree::{
        Request, Worktrees,
        testing::{git, init_repo, write},
    },
};

const BIN: &str = env!("CARGO_BIN_EXE_harness");

struct Env {
    home: tempfile::TempDir,
    ws: tempfile::TempDir,
    wts: Worktrees,
}

impl Env {
    fn new() -> Env {
        let home = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        init_repo(ws.path());
        let root = project_root(&ws.path().canonicalize().unwrap());
        let wts = Worktrees::new(
            ws.path(),
            home.path().join("data/worktrees").join(project_key(&root)),
        )
        .unwrap();
        Env { home, ws, wts }
    }

    fn cmd(&self, args: &[&str]) -> std::process::Output {
        Command::new(BIN)
            .timeout(Duration::from_secs(60))
            .current_dir(self.ws.path())
            .env("HARNESS_HOME", self.home.path())
            .isolate()
            .args(args)
            .output()
            .unwrap()
    }

    fn make(&self, agent: &str, task: &str, change: bool, status: AgentStatus) -> String {
        let wt = self
            .wts
            .create(&Request {
                agent,
                label: "general",
                task,
            })
            .unwrap()
            .worktree;
        if change {
            write(&wt.path, "src/new.rs", "pub fn x() {}\n");
        }
        let outcome = self.wts.commit_result(&wt, "general", task).unwrap();
        if status.is_final() {
            self.wts.release(&wt).unwrap();
        }
        self.wts
            .update_record(&wt.name, |r| {
                r.status = status;
                r.outcome = outcome;
            })
            .unwrap();
        wt.name
    }

    fn branch_exists(&self, name: &str) -> bool {
        !git(
            self.ws.path(),
            &["branch", "--list", &format!("harness/{name}")],
        )
        .trim()
        .is_empty()
    }
}

fn dead_pid() -> u32 {
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

#[test]
fn worktrees_lists_what_harness_made_with_its_state() {
    let env = Env::new();
    let pending = env.make("a1", "pending work", true, AgentStatus::Done);
    let running = env.make("a2", "running work", false, AgentStatus::Working);
    let out = env.cmd(&["worktrees"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains(&format!("harness/{pending}")), "{text}");
    assert!(text.contains("ready for review"), "{text}");
    assert!(text.contains("1 file"), "{text}");
    assert!(text.contains(&format!("harness/{running}")), "{text}");
    assert!(text.contains("running"), "{text}");
}

#[test]
fn worktrees_outside_a_repository_says_so() {
    let home = tempfile::tempdir().unwrap();
    let plain = tempfile::tempdir().unwrap();
    let out = Command::new(BIN)
        .timeout(Duration::from_secs(60))
        .current_dir(plain.path())
        .env("HARNESS_HOME", home.path())
        .isolate()
        .args(["worktrees"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("git repository"));
}

#[test]
fn clean_removes_only_harnesss_own_finished_empty_and_stale_ones() {
    let env = Env::new();
    let pending = env.make("a1", "pending work", true, AgentStatus::Done);
    let empty = env.make("a2", "empty work", false, AgentStatus::Done);
    let running = env.make("a3", "running work", false, AgentStatus::Working);
    // An agent of a harness that died: its lock is stale.
    let stale = env.make("a4", "stale work", false, AgentStatus::Working);
    let gone = dead_pid();
    env.wts.update_record(&stale, |r| r.pid = gone).unwrap();
    let stale_path = env
        .wts
        .records()
        .into_iter()
        .find(|r| r.name == stale)
        .unwrap()
        .path;
    git(&stale_path, &["worktree", "unlock", "."]);
    git(
        &stale_path,
        &[
            "worktree",
            "lock",
            "--reason",
            &format!("harness pid {gone}"),
            ".",
        ],
    );
    // What is not harness's: a worktree the user made where harness keeps its own, and a branch.
    let foreign = env.wts.home().join("foreign");
    git(
        env.ws.path(),
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "harness/foreign",
            foreign.to_str().unwrap(),
        ],
    );
    git(env.ws.path(), &["branch", "harness/mine"]);

    let out = env.cmd(&["worktrees", "clean"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains(&empty), "{text}");
    // Gone: the empty one and the stale one (it changed nothing). Kept: the rest.
    assert!(!env.branch_exists(&empty));
    assert!(!env.branch_exists(&stale));
    assert!(env.branch_exists(&pending), "a result not applied stays");
    assert!(text.contains("--all"), "says how to remove it: {text}");
    assert!(env.branch_exists(&running), "a running agent's stays");
    assert!(env.wts.records().iter().any(|r| r.name == running));
    assert!(
        foreign.exists() && env.branch_exists("foreign"),
        "the user's worktree stays"
    );
    assert!(env.branch_exists("mine"));

    // `--all` alone does not take a result whose commits are on no other branch.
    let out = env.cmd(&["worktrees", "clean", "--all"]);
    assert!(out.status.success());
    assert!(env.branch_exists(&pending));
    assert!(String::from_utf8_lossy(&out.stdout).contains("no other branch"));
    // Once they are on another branch it goes, and what is running and not harness's stays.
    git(
        env.ws.path(),
        &["merge", "-q", "--ff-only", &format!("harness/{pending}")],
    );
    let out = env.cmd(&["worktrees", "clean", "--all"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!env.branch_exists(&pending));
    assert!(env.branch_exists(&running));
    assert!(foreign.exists() && env.branch_exists("foreign") && env.branch_exists("mine"));
}

/// An agent that ended with its changes still uncommitted in the worktree (the commit failed).
fn make_unsaved(env: &Env, agent: &str) -> String {
    let wt = env
        .wts
        .create(&Request {
            agent,
            label: "general",
            task: "unsaved work",
        })
        .unwrap()
        .worktree;
    write(&wt.path, "src/precious.rs", "pub fn precious() {}\n");
    env.wts.release(&wt).unwrap();
    env.wts
        .update_record(&wt.name, |r| {
            r.status = AgentStatus::Failed;
            r.error = Some("its changes could not be committed: x".into());
        })
        .unwrap();
    wt.name
}

#[test]
fn clean_never_removes_a_worktree_with_uncommitted_changes_and_lists_it() {
    let env = Env::new();
    let unsaved = make_unsaved(&env, "a1");
    let path = env.wts.home().join(&unsaved);
    let listed = String::from_utf8_lossy(&env.cmd(&["worktrees"]).stdout).into_owned();
    assert!(listed.contains("not saved"), "{listed}");
    for args in [
        vec!["worktrees", "clean"],
        vec!["worktrees", "clean", "--all"],
    ] {
        let out = env.cmd(&args);
        assert!(out.status.success());
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(
            text.contains("left harness/") && text.contains("not committed"),
            "{text}"
        );
        assert!(path.join("src/precious.rs").exists(), "{args:?}");
        assert!(env.branch_exists(&unsaved));
    }
}

#[test]
fn discarding_unsaved_work_is_refused_without_a_terminal_unless_told_it_is_sure() {
    let env = Env::new();
    let unsaved = make_unsaved(&env, "a1");
    let path = env.wts.home().join(&unsaved);
    let out = env.cmd(&["worktrees", "clean", "--all", "--discard-unsaved"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(path.join("src/precious.rs").exists(), "nothing was removed");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("precious")
            || String::from_utf8_lossy(&out.stdout).contains("harness/")
    );
    // `--discard-unsaved` alone, or `--yes` alone, is not accepted.
    assert!(
        !env.cmd(&["worktrees", "clean", "--discard-unsaved"])
            .status
            .success()
    );
    assert!(
        !env.cmd(&["worktrees", "clean", "--all", "--yes"])
            .status
            .success()
    );
    assert!(path.exists());
    let out = env.cmd(&["worktrees", "clean", "--all", "--discard-unsaved", "--yes"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!path.exists() && !env.branch_exists(&unsaved));
}
