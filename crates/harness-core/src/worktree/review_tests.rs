use super::{
    testing::{git, marking_script, write},
    tests::{fixture, request},
    *,
};

/// A worktree for an agent that changed `README.md` to `text` and is done (unlocked).
fn finished(f: &super::tests::Fixture, task: &str, path: &str, text: &str) -> Worktree {
    let wt = f
        .wts
        .create(&request("a1", "general", task))
        .unwrap()
        .worktree;
    write(&wt.path, path, text);
    wt
}

fn done(f: &super::tests::Fixture, wt: &Worktree, task: &str) -> Outcome {
    let outcome = f
        .wts
        .commit_result(wt, "general", task)
        .unwrap()
        .expect("there were changes");
    f.wts.release(wt).unwrap();
    outcome
}

fn head(dir: &Path) -> String {
    git(dir, &["rev-parse", "HEAD"]).trim().to_string()
}

#[test]
fn what_an_agent_changed_is_committed_as_the_user_naming_the_agent_and_task() {
    let f = fixture();
    git(f.repo.path(), &["config", "user.name", "Ada Lovelace"]);
    git(f.repo.path(), &["config", "user.email", "ada@example.org"]);
    // A hook that would stop a commit.
    let blocker = f.repo.path().join(".git/hooks/pre-commit");
    write(
        f.repo.path(),
        ".git/hooks/pre-commit",
        "#!/bin/sh\nexit 1\n",
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&blocker, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    write(f.repo.path(), ".gitignore", "*.log\n");
    git(f.repo.path(), &["add", "-A"]);
    git(
        f.repo.path(),
        &[
            "-c",
            "core.hooksPath=/dev/null",
            "commit",
            "-q",
            "-m",
            "ignore",
        ],
    );
    let wt = f
        .wts
        .create(&request("a1", "general", "make the parser faster"))
        .unwrap()
        .worktree;
    write(&wt.path, "README.md", "hello\nworld\n");
    write(&wt.path, "src/new.rs", "pub fn two() -> u32 { 2 }\n");
    write(&wt.path, "debug.log", "ignored\n");
    let outcome = f
        .wts
        .commit_result(&wt, "general", "make the parser faster\nand keep the tests")
        .unwrap()
        .unwrap();
    assert_eq!(outcome.branch, wt.branch);
    assert_eq!(
        outcome.files,
        vec!["README.md".to_string(), "src/new.rs".to_string()]
    );
    assert_eq!((outcome.insertions, outcome.deletions), (2, 0));
    let log = git(&wt.path, &["log", "-1", "--format=%an|%ae|%cn|%s%n%b"]);
    assert!(
        log.starts_with("Ada Lovelace|ada@example.org|Ada Lovelace|harness: general"),
        "{log}"
    );
    assert!(log.contains("make the parser faster"), "{log}");
    assert!(log.contains("a1"), "names the agent: {log}");
    let subject = git(&wt.path, &["log", "-1", "--format=%s"]);
    assert!(
        !subject.contains("and keep"),
        "only the first line is in the subject: {subject}"
    );
    assert_eq!(git(&wt.path, &["rev-parse", "HEAD^"]).trim(), wt.base);
    assert!(!git(&wt.path, &["ls-files"]).contains("debug.log"));
    assert_eq!(outcome.commit, head(&wt.path));
}

#[test]
fn nothing_is_committed_when_nothing_changed() {
    let f = fixture();
    let wt = f
        .wts
        .create(&request("a1", "general", "look"))
        .unwrap()
        .worktree;
    assert_eq!(f.wts.commit_result(&wt, "general", "look").unwrap(), None);
    assert_eq!(head(&wt.path), wt.base);
}

#[test]
fn without_a_git_identity_the_commit_is_harnesss() {
    assert_eq!(
        author_of(None),
        ("harness".to_string(), "harness@localhost".to_string())
    );
    assert_eq!(
        author_of(Some(("A".into(), "a@b".into()))),
        ("A".to_string(), "a@b".to_string())
    );
}

#[test]
fn the_diff_is_the_branchs_own_and_runs_no_diff_program_of_the_repository() {
    let f = fixture();
    let marker = f.repo.path().join("diff-ran");
    let script = f.repo.path().join(".git/diff.sh");
    marking_script(&script, &marker);
    git(
        f.repo.path(),
        &["config", "diff.external", script.to_str().unwrap()],
    );
    git(
        f.repo.path(),
        &["config", "diff.evil.textconv", script.to_str().unwrap()],
    );
    write(f.repo.path(), ".git/info/attributes", "*.md diff=evil\n");
    let wt = finished(&f, "edit", "README.md", "hello\nthere\n");
    done(&f, &wt, "edit");
    let diff = f.wts.diff(&wt.name, 100_000).unwrap();
    assert!(diff.patch.contains("+there"), "{}", diff.patch);
    assert!(diff.stat.contains("README.md"), "{}", diff.stat);
    assert!(!diff.cut);
    assert!(!marker.exists(), "a diff program ran");
    let small = f.wts.diff(&wt.name, 20).unwrap();
    assert!(small.cut && small.patch.len() <= 20);
}

#[test]
fn siblings_that_changed_the_same_files_are_named() {
    let f = fixture();
    let one = finished(&f, "one", "README.md", "one\n");
    let two = f
        .wts
        .create(&request("a2", "general", "two"))
        .unwrap()
        .worktree;
    write(&two.path, "README.md", "two\n");
    write(&two.path, "other.txt", "two\n");
    let three = f
        .wts
        .create(&request("a3", "general", "three"))
        .unwrap()
        .worktree;
    write(&three.path, "elsewhere.txt", "three\n");
    done(&f, &one, "one");
    // The second is still running (its change is not committed yet): it counts all the same.
    let overlaps = f.wts.overlaps(&one.name);
    assert_eq!(overlaps.len(), 1, "{overlaps:?}");
    assert_eq!(overlaps[0].other, two.name);
    assert_eq!(overlaps[0].files, vec!["README.md".to_string()]);
    assert!(f.wts.overlaps(&three.name).is_empty());
}

#[test]
fn applying_puts_the_commit_on_the_current_branch_and_removes_the_worktree_and_branch() {
    let f = fixture();
    let wt = finished(&f, "edit", "src/new.rs", "pub fn two() -> u32 { 2 }\n");
    done(&f, &wt, "edit");
    // Work of the user's that is not committed, and in no way touched.
    write(f.repo.path(), "scratch.txt", "mine\n");
    let before = head(f.repo.path());
    let applied = f.wts.apply(&wt.name).unwrap();
    assert_ne!(head(f.repo.path()), before);
    assert_eq!(applied.commit, head(f.repo.path()));
    assert_eq!(git(f.repo.path(), &["rev-parse", "HEAD^"]).trim(), before);
    assert!(f.repo.path().join("src/new.rs").exists());
    assert_eq!(
        std::fs::read_to_string(f.repo.path().join("scratch.txt")).unwrap(),
        "mine\n"
    );
    assert!(!wt.path.exists());
    assert!(
        git(f.repo.path(), &["branch", "--list", &wt.branch])
            .trim()
            .is_empty()
    );
    assert!(f.wts.records().is_empty());
    assert_eq!(applied.files, vec!["src/new.rs".to_string()]);
}

#[test]
fn a_conflict_changes_nothing_and_keeps_the_branch() {
    let f = fixture();
    let wt = finished(&f, "edit", "README.md", "agent\n");
    done(&f, &wt, "edit");
    write(f.repo.path(), "README.md", "user\n");
    git(f.repo.path(), &["commit", "-q", "-am", "user edit"]);
    let before = head(f.repo.path());
    let error = f.wts.apply(&wt.name).unwrap_err();
    match &error {
        WorktreeError::Conflict { files, .. } => assert_eq!(files, &vec!["README.md".to_string()]),
        other => panic!("{other}"),
    }
    assert!(error.to_string().contains("nothing was changed"), "{error}");
    assert_eq!(head(f.repo.path()), before);
    assert_eq!(git(f.repo.path(), &["status", "--porcelain"]).trim(), "");
    assert_eq!(
        std::fs::read_to_string(f.repo.path().join("README.md")).unwrap(),
        "user\n"
    );
    assert!(!f.repo.path().join(".git/CHERRY_PICK_HEAD").exists());
    assert!(wt.path.exists());
    assert!(
        !git(f.repo.path(), &["branch", "--list", &wt.branch])
            .trim()
            .is_empty()
    );
    assert_eq!(f.wts.records().len(), 1);
}

#[test]
fn uncommitted_work_that_the_commit_would_overwrite_stops_the_apply() {
    let f = fixture();
    let wt = finished(&f, "edit", "README.md", "agent\n");
    done(&f, &wt, "edit");
    write(f.repo.path(), "README.md", "my unsaved thoughts\n");
    let before = head(f.repo.path());
    f.wts.apply(&wt.name).unwrap_err();
    assert_eq!(head(f.repo.path()), before);
    assert_eq!(
        std::fs::read_to_string(f.repo.path().join("README.md")).unwrap(),
        "my unsaved thoughts\n"
    );
    assert!(!f.repo.path().join(".git/CHERRY_PICK_HEAD").exists());
    assert!(wt.path.exists());
}

#[test]
fn a_detached_head_a_running_agent_and_an_unfinished_operation_refuse_the_apply() {
    let f = fixture();
    let wt = finished(&f, "edit", "src/new.rs", "x\n");
    // Still running: locked.
    f.wts.commit_result(&wt, "general", "edit").unwrap();
    assert!(matches!(
        f.wts.apply(&wt.name).unwrap_err(),
        WorktreeError::Locked { .. }
    ));
    f.wts.release(&wt).unwrap();
    // A merge that is not finished.
    write(
        f.repo.path(),
        ".git/MERGE_HEAD",
        &format!("{}\n", head(f.repo.path())),
    );
    assert!(matches!(
        f.wts.apply(&wt.name).unwrap_err(),
        WorktreeError::Refused(_)
    ));
    std::fs::remove_file(f.repo.path().join(".git/MERGE_HEAD")).unwrap();
    // No branch to put it on.
    git(f.repo.path(), &["checkout", "-q", "--detach"]);
    let refused = f.wts.apply(&wt.name).unwrap_err();
    assert!(matches!(refused, WorktreeError::Refused(_)), "{refused}");
    assert!(refused.to_string().contains("detached"), "{refused}");
    assert!(wt.path.exists());
}

#[test]
fn a_merge_driver_or_hook_of_the_repository_does_not_run_while_applying() {
    let f = fixture();
    let wt = f
        .wts
        .create(&request("a1", "general", "edit"))
        .unwrap()
        .worktree;
    write(&wt.path, "README.md", "hello\nagent line\n");
    done(&f, &wt, "edit");
    // The user changed the same file elsewhere, so the apply needs a merge: with a driver in
    // play, git would run it.
    write(f.repo.path(), "README.md", "user line\nhello\n");
    git(f.repo.path(), &["commit", "-q", "-am", "user"]);
    let marker = f.repo.path().join("ran");
    let script = f.repo.path().join(".git/driver.sh");
    marking_script(&script, &marker);
    git(
        f.repo.path(),
        &[
            "config",
            "merge.evil.driver",
            &format!("{} %A", script.display()),
        ],
    );
    write(
        f.repo.path(),
        ".git/info/attributes",
        "README.md merge=evil\n",
    );
    marking_script(&f.repo.path().join(".git/hooks/post-commit"), &marker);
    marking_script(&f.repo.path().join(".git/hooks/post-checkout"), &marker);
    let _ = f.wts.apply(&wt.name);
    assert!(!marker.exists(), "repository code ran");
}

#[test]
fn keeping_removes_the_worktree_and_leaves_the_branch_the_users() {
    let f = fixture();
    let wt = finished(&f, "edit", "src/new.rs", "x\n");
    done(&f, &wt, "edit");
    f.wts.keep(&wt.name).unwrap();
    assert!(!wt.path.exists());
    assert!(
        !git(f.repo.path(), &["branch", "--list", &wt.branch])
            .trim()
            .is_empty()
    );
    assert!(f.wts.records().is_empty());
    // It is not harness's to remove any more.
    assert!(matches!(
        f.wts.remove(&wt.name).unwrap_err(),
        WorktreeError::NotOurs { .. }
    ));
    assert!(
        !git(f.repo.path(), &["branch", "--list", &wt.branch])
            .trim()
            .is_empty()
    );
}

#[test]
fn discarding_removes_the_worktree_the_branch_and_the_record() {
    let f = fixture();
    let wt = finished(&f, "edit", "src/new.rs", "x\n");
    done(&f, &wt, "edit");
    f.wts.discard(&wt.name).unwrap();
    assert!(!wt.path.exists());
    assert!(
        git(f.repo.path(), &["branch", "--list", &wt.branch])
            .trim()
            .is_empty()
    );
    assert!(f.wts.records().is_empty());
}
