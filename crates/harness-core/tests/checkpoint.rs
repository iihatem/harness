use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use harness_core::checkpoint::{self, CheckpointError, Checkpoints, MAX_FILE_SIZE};

/// A workspace and a data directory for the shadow repository, resolved through symlinks.
struct Fixture {
    _dir: tempfile::TempDir,
    ws: PathBuf,
    gitdir: PathBuf,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let ws = base.join("ws");
    std::fs::create_dir(&ws).unwrap();
    Fixture {
        _dir: dir,
        ws,
        gitdir: base.join("data/checkpoints/project.git"),
    }
}

impl Fixture {
    fn checkpoints(&self) -> Checkpoints {
        Checkpoints::open(&self.gitdir, &self.ws, "s1").unwrap()
    }

    fn write(&self, path: &str, text: &str) {
        let path = self.ws.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn read(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(self.ws.join(path)).ok()
    }
}

/// Runs the user's git in `dir` with their own configuration ignored.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8(out.stdout).unwrap()
}

// Spec: undo a bad refactor. Restoring reverts edits, recreates deleted files and removes new ones,
// whichever tool made the change.
#[test]
fn a_restore_reverts_edits_deletions_and_new_files() {
    let f = fixture();
    f.write("a.rs", "fn a() {}\n");
    f.write("src/b.rs", "fn b() {}\n");
    f.write("c.rs", "fn  c( ) {}\n");
    let checkpoints = f.checkpoints();
    let before = checkpoints.snapshot("turn 1").unwrap();
    f.write("a.rs", "fn a() { broken }\n");
    std::fs::remove_file(f.ws.join("src/b.rs")).unwrap();
    f.write("new/d.rs", "fn d() {}\n");
    // A formatter run through bash rewrites a file.
    f.write("c.rs", "fn c() {}\n");
    checkpoints.restore(&before).unwrap();
    assert_eq!(f.read("a.rs").as_deref(), Some("fn a() {}\n"));
    assert_eq!(f.read("src/b.rs").as_deref(), Some("fn b() {}\n"));
    assert_eq!(f.read("c.rs").as_deref(), Some("fn  c( ) {}\n"));
    assert!(!f.ws.join("new/d.rs").exists());
}

#[test]
fn a_restore_can_be_undone_with_the_snapshot_it_returns() {
    let f = fixture();
    f.write("a.txt", "one\n");
    let checkpoints = f.checkpoints();
    let first = checkpoints.snapshot("turn 1").unwrap();
    f.write("a.txt", "two\n");
    f.write("b.txt", "new\n");
    let undo = checkpoints.restore(&first).unwrap();
    assert_eq!(f.read("a.txt").as_deref(), Some("one\n"));
    checkpoints.restore(&undo).unwrap();
    assert_eq!(f.read("a.txt").as_deref(), Some("two\n"));
    assert_eq!(f.read("b.txt").as_deref(), Some("new\n"));
}

// Spec: the user's repository is untouched.
#[test]
fn the_users_repository_is_untouched() {
    let f = fixture();
    git(&f.ws, &["init", "-q", "-b", "main"]);
    f.write("a.txt", "one\n");
    git(&f.ws, &["add", "a.txt"]);
    git(&f.ws, &["commit", "-q", "-m", "first"]);
    git(&f.ws, &["branch", "feature"]);
    f.write("a.txt", "staged\n");
    git(&f.ws, &["add", "a.txt"]);
    f.write("a.txt", "staged and then edited\n");
    let index = std::fs::read(f.ws.join(".git/index")).unwrap();
    let status = git(&f.ws, &["status", "--porcelain"]);
    let log = git(&f.ws, &["log", "--all", "--format=%H %s"]);
    let branches = git(&f.ws, &["branch", "--list"]);

    let checkpoints = f.checkpoints();
    let before = checkpoints.snapshot("turn 1").unwrap();
    f.write("a.txt", "the agent's edit\n");
    checkpoints.restore(&before).unwrap();

    assert_eq!(f.read("a.txt").as_deref(), Some("staged and then edited\n"));
    assert_eq!(std::fs::read(f.ws.join(".git/index")).unwrap(), index);
    assert_eq!(git(&f.ws, &["status", "--porcelain"]), status);
    assert_eq!(git(&f.ws, &["log", "--all", "--format=%H %s"]), log);
    assert_eq!(git(&f.ws, &["branch", "--list"]), branches);
    assert!(
        !checkpoints
            .files(&before)
            .unwrap()
            .iter()
            .any(|p| p.starts_with(".git"))
    );
}

// Spec: non-git directory.
#[test]
fn a_directory_that_is_not_a_repository_works() {
    let f = fixture();
    f.write("notes.txt", "hi\n");
    let checkpoints = f.checkpoints();
    let snapshot = checkpoints.snapshot("turn 1").unwrap();
    assert_eq!(
        checkpoints.files(&snapshot).unwrap(),
        [PathBuf::from("notes.txt")]
    );
    assert!(!f.ws.join(".git").exists());
}

#[test]
fn snapshots_honour_gitignore_and_leave_out_builtin_excludes() {
    let f = fixture();
    f.write(".gitignore", "*.log\n");
    f.write("keep.rs", "x\n");
    f.write("debug.log", "x\n");
    f.write("node_modules/m.js", "x\n");
    f.write("target/debug/t", "x\n");
    f.write("sub/.git/config", "x\n");
    let checkpoints = f.checkpoints();
    let snapshot = checkpoints.snapshot("turn 1").unwrap();
    assert_eq!(
        checkpoints.files(&snapshot).unwrap(),
        [PathBuf::from(".gitignore"), PathBuf::from("keep.rs")]
    );
}

// Review Focus: a rewind must never overwrite or delete what no snapshot holds.
#[test]
fn large_and_ignored_files_are_left_out_and_never_overwritten() {
    let f = fixture();
    f.write("data.bin", "small\n");
    f.write(".env", "OLD=1\n");
    let checkpoints = f.checkpoints();
    let first = checkpoints.snapshot("turn 1").unwrap();
    // data.bin grows past the limit, .env becomes ignored and changes, and a large file appears.
    let large = vec![b'x'; MAX_FILE_SIZE as usize + 1];
    std::fs::write(f.ws.join("data.bin"), &large).unwrap();
    std::fs::write(f.ws.join("new.bin"), &large).unwrap();
    f.write(".gitignore", ".env\n");
    f.write(".env", "NEW=2\n");
    let second = checkpoints.snapshot("turn 2").unwrap();
    assert_eq!(
        checkpoints.files(&second).unwrap(),
        [PathBuf::from(".gitignore")]
    );

    checkpoints.restore(&first).unwrap();
    assert_eq!(
        std::fs::read(f.ws.join("data.bin")).unwrap().len(),
        large.len()
    );
    assert_eq!(
        std::fs::read(f.ws.join("new.bin")).unwrap().len(),
        large.len()
    );
    assert_eq!(f.read(".env").as_deref(), Some("NEW=2\n"));
    assert!(!f.ws.join(".gitignore").exists());
}

#[test]
fn a_restore_does_not_write_through_a_symlinked_directory() {
    let f = fixture();
    f.write("dir/f.txt", "mine\n");
    let checkpoints = f.checkpoints();
    let first = checkpoints.snapshot("turn 1").unwrap();
    let outside = f.ws.parent().unwrap().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::remove_dir_all(f.ws.join("dir")).unwrap();
    std::os::unix::fs::symlink(&outside, f.ws.join("dir")).unwrap();
    checkpoints.restore(&first).unwrap();
    assert!(
        !std::fs::symlink_metadata(f.ws.join("dir"))
            .unwrap()
            .is_symlink()
    );
    assert_eq!(f.read("dir/f.txt").as_deref(), Some("mine\n"));
    assert!(std::fs::read_dir(&outside).unwrap().next().is_none());
}

#[test]
fn a_slow_snapshot_is_abandoned_without_leaving_a_lock() {
    let f = fixture();
    f.write("a.txt", "x\n");
    let checkpoints = f.checkpoints().with_timeout(Duration::from_millis(1));
    assert!(matches!(
        checkpoints.snapshot("turn 1"),
        Err(CheckpointError::TooSlow)
    ));
    let locks: Vec<_> = std::fs::read_dir(f.gitdir.join("indexes"))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".lock"))
        .collect();
    assert!(locks.is_empty());
}

// Final review, minor 5: a workspace too large to snapshot in time makes every run wait at its
// first change, so the warning says what makes snapshots faster.
#[test]
fn a_slow_snapshot_names_the_remedy() {
    let f = fixture();
    f.write("a.txt", "x\n");
    let error = f
        .checkpoints()
        .with_timeout(Duration::from_millis(1))
        .snapshot("turn 1")
        .unwrap_err();
    assert!(matches!(error, CheckpointError::TooSlow), "{error:?}");
    let text = error.to_string();
    assert!(
        text.contains("a snapshot took longer than 5 seconds")
            && text.contains("add large generated directories to `.gitignore`"),
        "{text}"
    );
}

#[test]
fn a_missing_git_is_reported() {
    let f = fixture();
    let missing = Checkpoints::open_with_git(Path::new("/nonexistent/git"), &f.gitdir, &f.ws, "s1");
    assert!(matches!(missing, Err(CheckpointError::GitMissing)));
}

#[test]
fn sessions_of_one_project_keep_their_own_snapshots() {
    let f = fixture();
    f.write("a.txt", "one\n");
    let first = Checkpoints::open(&f.gitdir, &f.ws, "s1").unwrap();
    let second = Checkpoints::open(&f.gitdir, &f.ws, "s2").unwrap();
    let one = first.snapshot("s1 turn 1").unwrap();
    f.write("a.txt", "two\n");
    let two = second.snapshot("s2 turn 1").unwrap();
    assert_ne!(one, two);
    first.restore(&one).unwrap();
    assert_eq!(f.read("a.txt").as_deref(), Some("one\n"));
}

// Review E minor 9: the session id becomes paths in the shadow repository, and commit ids come
// from the session file; neither may name anything else.
#[test]
fn session_ids_and_commit_ids_are_checked() {
    let f = fixture();
    for id in ["../escape", "a/b", "", "s 1"] {
        assert!(
            matches!(
                Checkpoints::open(&f.gitdir, &f.ws, id),
                Err(CheckpointError::InvalidSession(_))
            ),
            "{id:?}"
        );
    }
    assert!(!f.gitdir.join("escape").exists());
    f.write("a.txt", "one\n");
    let checkpoints = f.checkpoints();
    let snapshot = checkpoints.snapshot("turn 1").unwrap();
    f.write("a.txt", "two\n");
    let short = &snapshot[..12];
    let upper = snapshot.to_uppercase();
    for commit in [
        "refs/harness/s1",
        "HEAD",
        "--help",
        short,
        upper.as_str(),
        "",
    ] {
        assert!(
            matches!(
                checkpoints.restore(commit),
                Err(CheckpointError::InvalidCommit(_))
            ),
            "{commit:?}"
        );
        assert!(
            matches!(
                checkpoints.files(commit),
                Err(CheckpointError::InvalidCommit(_))
            ),
            "{commit:?}"
        );
    }
    assert_eq!(f.read("a.txt").as_deref(), Some("two\n"));
}

// Review E minor 7: the workspace's .gitattributes cannot change the bytes a restore writes.
#[test]
fn a_restore_is_byte_exact_whatever_gitattributes_say() {
    let f = fixture();
    f.write(".gitattributes", "* text=auto eol=lf\n*.id ident\n");
    std::fs::write(f.ws.join("win.txt"), b"a\r\nb\r\n").unwrap();
    std::fs::write(f.ws.join("x.id"), b"$Id$\n").unwrap();
    let checkpoints = f.checkpoints();
    let first = checkpoints.snapshot("turn 1").unwrap();
    std::fs::write(f.ws.join("win.txt"), b"agent\n").unwrap();
    std::fs::remove_file(f.ws.join("x.id")).unwrap();
    checkpoints.restore(&first).unwrap();
    assert_eq!(std::fs::read(f.ws.join("win.txt")).unwrap(), b"a\r\nb\r\n");
    assert_eq!(std::fs::read(f.ws.join("x.id")).unwrap(), b"$Id$\n");
}

// Review E minor 9: the shadow repository's own configuration cannot make git run a program.
#[test]
fn programs_named_in_the_shadow_config_do_not_run() {
    let f = fixture();
    f.write("a.txt", "one\n");
    let checkpoints = f.checkpoints();
    let first = checkpoints.snapshot("turn 1").unwrap();
    let marker = f.ws.parent().unwrap().join("ran");
    let script = f.ws.parent().unwrap().join("evil.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\necho \"$0 $*\" >> {}\nexit 1\n",
            marker.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let hooks = f.ws.parent().unwrap().join("hooks");
    std::fs::create_dir(&hooks).unwrap();
    for hook in [
        "pre-commit",
        "post-commit",
        "reference-transaction",
        "post-index-change",
    ] {
        std::fs::copy(&script, hooks.join(hook)).unwrap();
    }
    let config = f.gitdir.join("config");
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str(&format!(
        "[core]\n\tfsmonitor = {s}\n\thooksPath = {h}\n[commit]\n\tgpgSign = true\n[gpg]\n\tprogram = {s}\n",
        s = script.display(),
        h = hooks.display()
    ));
    std::fs::write(&config, text).unwrap();
    f.write("a.txt", "two\n");
    let second = checkpoints.snapshot("turn 2").unwrap();
    checkpoints.restore(&first).unwrap();
    checkpoints.restore(&second).unwrap();
    assert_eq!(f.read("a.txt").as_deref(), Some("two\n"));
    assert!(
        !marker.exists(),
        "{}",
        std::fs::read_to_string(&marker).unwrap_or_default()
    );
}

// Review E minor 12: a restore that runs out of time leaves no lock behind either, so later
// snapshots and restores still work.
#[test]
fn a_restore_that_runs_out_of_time_leaves_no_lock() {
    let f = fixture();
    f.write("a.txt", "one\n");
    let checkpoints = f.checkpoints();
    let first = checkpoints.snapshot("turn 1").unwrap();
    f.write("a.txt", "two\n");
    // What git killed at the time limit leaves behind.
    let locks = [
        f.gitdir.join("indexes/s1.lock"),
        f.gitdir.join("indexes/restore-s1.lock"),
        f.gitdir.join("refs/harness/s1.lock"),
    ];
    for lock in &locks {
        std::fs::write(lock, "").unwrap();
    }
    let slow = Checkpoints::open(&f.gitdir, &f.ws, "s1")
        .unwrap()
        .with_restore_timeout(Duration::ZERO);
    assert!(matches!(
        slow.restore(&first),
        Err(CheckpointError::TooSlow)
    ));
    for lock in &locks {
        assert!(!lock.exists(), "{}", lock.display());
    }
    checkpoints.restore(&first).unwrap();
    assert_eq!(f.read("a.txt").as_deref(), Some("one\n"));
    checkpoints.snapshot("turn 2").unwrap();
}

// Review E minor 12: finding git has a time limit too.
#[test]
fn a_git_that_does_not_answer_is_given_up_on() {
    let f = fixture();
    let git = f.ws.parent().unwrap().join("git");
    std::fs::write(&git, "#!/bin/sh\nexec sleep 30\n").unwrap();
    std::fs::set_permissions(&git, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let start = Instant::now();
    let opened = Checkpoints::open_with_git(&git, &f.gitdir, &f.ws, "s1");
    assert!(opened.is_err());
    assert!(
        start.elapsed() < Duration::from_secs(15),
        "{:?}",
        start.elapsed()
    );
}

// Review E minor 8 (probe H): a large file whose name no ignore pattern can express is left out.
#[test]
fn a_large_file_with_a_newline_in_its_name_is_left_out() {
    let f = fixture();
    f.write("a.txt", "a\n");
    let large = vec![b'x'; MAX_FILE_SIZE as usize + 1];
    std::fs::write(f.ws.join("big\nname"), &large).unwrap();
    let checkpoints = f.checkpoints();
    let snapshot = checkpoints.snapshot("turn 1").unwrap();
    assert_eq!(
        checkpoints.files(&snapshot).unwrap(),
        [PathBuf::from("a.txt")]
    );
}

// Review E minor 8 (probe T): a .gitignore negation cannot bring back what snapshots always
// leave out.
#[test]
fn gitignore_negations_do_not_outrank_the_size_limit_or_the_builtin_excludes() {
    let f = fixture();
    f.write(".gitignore", "*.tmp\n!big.bin\n!node_modules/\n!target/\n");
    let large = vec![b'x'; MAX_FILE_SIZE as usize + 1];
    std::fs::write(f.ws.join("big.bin"), &large).unwrap();
    f.write("node_modules/m.js", "x\n");
    f.write("web/node_modules/pkg/i.js", "x\n");
    f.write("target/debug/t", "x\n");
    f.write("src/target", "a file named target is kept\n");
    let checkpoints = f.checkpoints();
    let snapshot = checkpoints.snapshot("turn 1").unwrap();
    assert_eq!(
        checkpoints.files(&snapshot).unwrap(),
        [PathBuf::from(".gitignore"), PathBuf::from("src/target")]
    );
    // What an earlier snapshot holds leaves later ones once it is excluded.
    std::fs::write(f.ws.join("big.bin"), b"small").unwrap();
    let small = checkpoints.snapshot("turn 2").unwrap();
    assert!(
        checkpoints
            .files(&small)
            .unwrap()
            .contains(&PathBuf::from("big.bin"))
    );
    std::fs::write(f.ws.join("big.bin"), &large).unwrap();
    let again = checkpoints.snapshot("turn 3").unwrap();
    assert!(
        !checkpoints
            .files(&again)
            .unwrap()
            .contains(&PathBuf::from("big.bin"))
    );
}

// Review E issue 3 (probe F): in a subdirectory of a repository, the repository's ignore rules
// apply: its root .gitignore and its own info/exclude.
#[test]
fn a_subdirectory_of_a_repository_follows_the_repositorys_ignore_rules() {
    let f = fixture();
    git(&f.ws, &["init", "-q"]);
    f.write(".gitignore", "*.log\n.env\n");
    std::fs::write(f.ws.join(".git/info/exclude"), "local-only/\n").unwrap();
    f.write("sub/code.rs", "x\n");
    f.write("sub/debug.log", "x\n");
    f.write("sub/.env", "SECRET\n");
    f.write("sub/local-only/notes.txt", "x\n");
    f.write("sub/deeper/lib.rs", "y\n");
    let checkpoints = Checkpoints::open(&f.gitdir, &f.ws.join("sub"), "s1").unwrap();
    let snapshot = checkpoints.snapshot("turn 1").unwrap();
    assert_eq!(
        checkpoints.files(&snapshot).unwrap(),
        [PathBuf::from("code.rs"), PathBuf::from("deeper/lib.rs")]
    );
}

// Review E issue 3 (probe F2): so a rewind there leaves the files the repository ignores alone,
// and never touches anything outside the workspace.
#[test]
fn a_restore_in_a_subdirectory_leaves_ignored_files_and_the_rest_of_the_repository_alone() {
    let f = fixture();
    git(&f.ws, &["init", "-q"]);
    f.write(".gitignore", "*.log\n.env\n");
    f.write("top.txt", "top\n");
    f.write("sub/code.rs", "x\n");
    f.write("sub/.env", "OLD\n");
    let sub = f.ws.join("sub");
    let checkpoints = Checkpoints::open(&f.gitdir, &sub, "s1").unwrap();
    let first = checkpoints.snapshot("turn 1").unwrap();
    f.write("sub/.env", "NEW (edited by the user)\n");
    f.write("sub/server.log", "log\n");
    f.write("sub/code.rs", "changed\n");
    f.write("top.txt", "changed outside the workspace\n");
    f.write("new-at-top.txt", "new\n");
    checkpoints.restore(&first).unwrap();
    assert_eq!(f.read("sub/code.rs").as_deref(), Some("x\n"));
    assert_eq!(
        f.read("sub/.env").as_deref(),
        Some("NEW (edited by the user)\n")
    );
    assert_eq!(f.read("sub/server.log").as_deref(), Some("log\n"));
    assert_eq!(
        f.read("top.txt").as_deref(),
        Some("changed outside the workspace\n")
    );
    assert_eq!(f.read("new-at-top.txt").as_deref(), Some("new\n"));
}

// A new session starts from the index another session left, which may cover a different part of
// the repository: none of it may enter this workspace's snapshots or be removed by its restores.
#[test]
fn an_index_left_by_a_session_elsewhere_in_the_repository_stays_out() {
    let f = fixture();
    git(&f.ws, &["init", "-q"]);
    f.write("top.txt", "top\n");
    f.write("other/o.txt", "o\n");
    f.write("sub/s.txt", "s\n");
    let at_root = Checkpoints::open(&f.gitdir, &f.ws, "s1").unwrap();
    at_root.snapshot("turn 1").unwrap();
    let in_sub = Checkpoints::open(&f.gitdir, &f.ws.join("sub"), "s2").unwrap();
    let first = in_sub.snapshot("turn 1").unwrap();
    assert_eq!(in_sub.files(&first).unwrap(), [PathBuf::from("s.txt")]);
    f.write("sub/s.txt", "changed\n");
    in_sub.restore(&first).unwrap();
    assert_eq!(f.read("sub/s.txt").as_deref(), Some("s\n"));
    assert_eq!(f.read("top.txt").as_deref(), Some("top\n"));
    assert_eq!(f.read("other/o.txt").as_deref(), Some("o\n"));
}

// A linked worktree's .git is a file; the ignore rules of the repository it belongs to still apply.
#[test]
fn a_linked_worktree_follows_its_repositorys_info_exclude() {
    let f = fixture();
    let main = f.ws.join("main");
    std::fs::create_dir(&main).unwrap();
    git(&main, &["init", "-q", "-b", "main"]);
    std::fs::write(main.join("a.txt"), "a\n").unwrap();
    git(&main, &["add", "a.txt"]);
    git(&main, &["commit", "-q", "-m", "first"]);
    std::fs::write(main.join(".git/info/exclude"), "*.secret\n").unwrap();
    let linked = f.ws.join("linked");
    git(&main, &["worktree", "add", "-q", linked.to_str().unwrap()]);
    std::fs::write(linked.join("key.secret"), "x\n").unwrap();
    let checkpoints = Checkpoints::open(&f.gitdir, &linked, "s1").unwrap();
    let snapshot = checkpoints.snapshot("turn 1").unwrap();
    assert_eq!(
        checkpoints.files(&snapshot).unwrap(),
        [PathBuf::from("a.txt")]
    );
}

// Review E minor 15: `.harness/` and a `HEAD` at the top of the workspace are left out, as the
// permission engine and the sandbox protect them, whatever .gitignore says; a rewind neither
// recreates nor removes them.
#[test]
fn harness_settings_and_a_top_level_head_are_left_out() {
    let f = fixture();
    f.write(".gitignore", "!/HEAD\n!.harness/\n");
    f.write(".harness/config.toml", "x\n");
    f.write("HEAD", "ref: refs/heads/main\n");
    f.write("src/HEAD", "only the top level is protected\n");
    let checkpoints = f.checkpoints();
    let first = checkpoints.snapshot("turn 1").unwrap();
    assert_eq!(
        checkpoints.files(&first).unwrap(),
        [PathBuf::from(".gitignore"), PathBuf::from("src/HEAD")]
    );
    std::fs::remove_dir_all(f.ws.join(".harness")).unwrap();
    std::fs::remove_file(f.ws.join("HEAD")).unwrap();
    checkpoints.restore(&first).unwrap();
    assert!(!f.ws.join(".harness").exists());
    assert!(!f.ws.join("HEAD").exists());
    f.write(".HARNESS/config.toml", "made since\n");
    f.write("Head", "made since\n");
    checkpoints.restore(&first).unwrap();
    assert_eq!(
        f.read(".HARNESS/config.toml").as_deref(),
        Some("made since\n")
    );
    assert_eq!(f.read("Head").as_deref(), Some("made since\n"));
}

// The same at the top of a workspace in a repository's subdirectory.
#[test]
fn harness_settings_in_a_subdirectory_workspace_are_left_out() {
    let f = fixture();
    git(&f.ws, &["init", "-q"]);
    f.write("sub/.harness/config.toml", "x\n");
    f.write("sub/HEAD", "x\n");
    f.write("sub/a.txt", "a\n");
    let checkpoints = Checkpoints::open(&f.gitdir, &f.ws.join("sub"), "s1").unwrap();
    let first = checkpoints.snapshot("turn 1").unwrap();
    assert_eq!(checkpoints.files(&first).unwrap(), [PathBuf::from("a.txt")]);
}

// Review E issue 1 (probe A): a file the target snapshot left out because it was ignored then
// existed then, so a restore does not delete it, even though it is no longer ignored.
#[test]
fn a_file_ignored_when_the_snapshot_was_taken_survives_a_restore() {
    let f = fixture();
    f.write(".gitignore", ".env\nbuild/\n");
    f.write(".env", "SECRET=1\n");
    f.write("build/out.o", "object\n");
    f.write("a.txt", "a\n");
    let checkpoints = f.checkpoints();
    let first = checkpoints.snapshot("turn 1").unwrap();
    // The agent drops the ignore rules and changes a file.
    f.write(".gitignore", "node_modules/\n");
    f.write("a.txt", "changed\n");
    f.write("created.txt", "new\n");
    checkpoints.restore(&first).unwrap();
    assert_eq!(f.read(".gitignore").as_deref(), Some(".env\nbuild/\n"));
    assert_eq!(f.read("a.txt").as_deref(), Some("a\n"));
    assert_eq!(f.read(".env").as_deref(), Some("SECRET=1\n"));
    assert_eq!(f.read("build/out.o").as_deref(), Some("object\n"));
    assert_eq!(f.read("created.txt"), None);
}

// Review E issue 1 (probe M): a file git could not read at the snapshot existed then too.
#[test]
fn a_file_unreadable_when_the_snapshot_was_taken_survives_a_restore() {
    use std::os::unix::fs::PermissionsExt;
    if is_root() {
        return; // root reads the file anyway
    }
    let f = fixture();
    f.write("a.txt", "a\n");
    f.write("locked.txt", "user data\n");
    let locked = f.ws.join("locked.txt");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let checkpoints = f.checkpoints();
    let first = checkpoints.snapshot("turn 1").unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644)).unwrap();
    f.write("locked.txt", "user data, edited\n");
    checkpoints.restore(&first).unwrap();
    assert_eq!(f.read("locked.txt").as_deref(), Some("user data, edited\n"));
}

// The same for a directory git could not open at the snapshot: what was in it existed then.
#[test]
fn a_directory_unreadable_when_the_snapshot_was_taken_survives_a_restore() {
    use std::os::unix::fs::PermissionsExt;
    if is_root() {
        return; // root reads the directory anyway
    }
    let f = fixture();
    f.write("a.txt", "a\n");
    f.write("locked/notes.txt", "user data\n");
    let locked = f.ws.join("locked");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let checkpoints = f.checkpoints();
    let first = checkpoints.snapshot("turn 1");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    let first = first.unwrap();
    f.write("a.txt", "changed\n");
    checkpoints.restore(&first).unwrap();
    assert_eq!(f.read("a.txt").as_deref(), Some("a\n"));
    assert_eq!(f.read("locked/notes.txt").as_deref(), Some("user data\n"));
}

// Review E issue 1 (probe N): so did a file that was too large then and has shrunk since.
#[test]
fn a_file_too_large_when_the_snapshot_was_taken_survives_a_restore() {
    let f = fixture();
    let large = vec![b'x'; MAX_FILE_SIZE as usize + 1];
    std::fs::write(f.ws.join("data.bin"), &large).unwrap();
    let checkpoints = f.checkpoints();
    let first = checkpoints.snapshot("turn 1").unwrap();
    std::fs::write(f.ws.join("data.bin"), b"small now").unwrap();
    checkpoints.restore(&first).unwrap();
    assert_eq!(std::fs::read(f.ws.join("data.bin")).unwrap(), b"small now");
}

// A snapshot without a record (as an older harness took them) cannot be restored safely, so it is
// refused rather than restored.
#[test]
fn a_snapshot_without_a_record_is_refused() {
    let f = fixture();
    f.write("a.txt", "one\n");
    let checkpoints = f.checkpoints();
    let snapshot = checkpoints.snapshot("turn 1").unwrap();
    let tree = git(&f.gitdir, &["rev-parse", &format!("{snapshot}^{{tree}}")]);
    let old = git(&f.gitdir, &["commit-tree", tree.trim(), "-m", "old"]);
    f.write("a.txt", "two\n");
    assert!(matches!(
        checkpoints.restore(old.trim()),
        Err(CheckpointError::NoRecord(_))
    ));
    assert_eq!(f.read("a.txt").as_deref(), Some("two\n"));
}

// Review E issue 2 (probe D1): where the target has `d/a.txt`, `d` is now a file too large to
// snapshot. Making the directory would delete it; it is left alone instead.
#[test]
fn a_large_file_where_the_target_had_a_directory_is_left_alone() {
    let f = fixture();
    f.write("d/a.txt", "a\n");
    f.write("b.txt", "b\n");
    let checkpoints = f.checkpoints();
    let first = checkpoints.snapshot("turn 1").unwrap();
    std::fs::remove_dir_all(f.ws.join("d")).unwrap();
    let large = vec![b'x'; MAX_FILE_SIZE as usize + 1];
    std::fs::write(f.ws.join("d"), &large).unwrap();
    f.write("b.txt", "changed\n");
    checkpoints.restore(&first).unwrap();
    assert_eq!(std::fs::read(f.ws.join("d")).unwrap(), large);
    assert_eq!(f.read("b.txt").as_deref(), Some("b\n"));
}

// Review E issue 2 (probe D2): the same with a git-ignored file.
#[test]
fn an_ignored_file_where_the_target_had_a_directory_is_left_alone() {
    let f = fixture();
    f.write("e/a.txt", "a\n");
    let checkpoints = f.checkpoints();
    let first = checkpoints.snapshot("turn 1").unwrap();
    std::fs::remove_dir_all(f.ws.join("e")).unwrap();
    f.write(".gitignore", "/e\n");
    f.write("e", "precious ignored file\n");
    checkpoints.restore(&first).unwrap();
    assert_eq!(f.read("e").as_deref(), Some("precious ignored file\n"));
}

// Review E issue 2 (probe G): `dir` is now a symlink to a directory outside that holds a file of
// the same name. The symlink is in the pre-rewind snapshot, so the restore replaces it with the
// directory, and the file outside is never touched.
#[test]
fn a_symlink_whose_target_holds_the_same_names_is_replaced_by_the_directory() {
    let f = fixture();
    f.write("dir/f.txt", "mine\n");
    let checkpoints = f.checkpoints();
    let first = checkpoints.snapshot("turn 1").unwrap();
    let outside = f.ws.parent().unwrap().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("f.txt"), "outside\n").unwrap();
    std::fs::remove_dir_all(f.ws.join("dir")).unwrap();
    std::os::unix::fs::symlink(&outside, f.ws.join("dir")).unwrap();
    checkpoints.restore(&first).unwrap();
    assert!(
        std::fs::symlink_metadata(f.ws.join("dir"))
            .unwrap()
            .is_dir()
    );
    assert_eq!(f.read("dir/f.txt").as_deref(), Some("mine\n"));
    assert_eq!(
        std::fs::read_to_string(outside.join("f.txt")).unwrap(),
        "outside\n"
    );
}

// Review E probe U: an ignored symlink where the target had a directory is left alone, and
// nothing is written through it.
#[test]
fn an_ignored_symlink_where_the_target_had_a_directory_is_left_alone() {
    let f = fixture();
    f.write("dir/f.txt", "mine\n");
    let checkpoints = f.checkpoints();
    let first = checkpoints.snapshot("turn 1").unwrap();
    let outside = f.ws.parent().unwrap().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::remove_dir_all(f.ws.join("dir")).unwrap();
    std::os::unix::fs::symlink(&outside, f.ws.join("dir")).unwrap();
    f.write(".gitignore", "/dir\n");
    checkpoints.restore(&first).unwrap();
    assert!(
        std::fs::symlink_metadata(f.ws.join("dir"))
            .unwrap()
            .is_symlink()
    );
    assert!(std::fs::read_dir(&outside).unwrap().next().is_none());
}

// A file the agent replaced with a directory comes back when everything in the directory is in
// the pre-rewind snapshot; a directory that holds something snapshots leave out is left alone.
#[test]
fn a_file_replaced_by_a_directory_comes_back_unless_the_directory_holds_what_snapshots_leave_out() {
    let f = fixture();
    f.write(".gitignore", "*.log\n");
    f.write("x", "file x\n");
    f.write("y", "file y\n");
    let checkpoints = f.checkpoints();
    let first = checkpoints.snapshot("turn 1").unwrap();
    for name in ["x", "y"] {
        std::fs::remove_file(f.ws.join(name)).unwrap();
        f.write(&format!("{name}/a.txt"), "made by the agent\n");
    }
    f.write("y/keep.log", "ignored, so no snapshot holds it\n");
    checkpoints.restore(&first).unwrap();
    assert_eq!(f.read("x").as_deref(), Some("file x\n"));
    assert!(f.ws.join("y").is_dir());
    assert_eq!(
        f.read("y/keep.log").as_deref(),
        Some("ignored, so no snapshot holds it\n")
    );
}

// A directory that replaced a file and holds a nested repository is left alone: no snapshot holds
// what is inside a nested repository.
#[test]
fn a_directory_holding_a_nested_repository_is_never_removed() {
    let f = fixture();
    f.write("x", "file x\n");
    let checkpoints = f.checkpoints();
    let first = checkpoints.snapshot("turn 1").unwrap();
    std::fs::remove_file(f.ws.join("x")).unwrap();
    let inner = f.ws.join("x/inner");
    std::fs::create_dir_all(&inner).unwrap();
    git(&inner, &["init", "-q"]);
    std::fs::write(inner.join("work.txt"), "the user's work\n").unwrap();
    git(&inner, &["add", "work.txt"]);
    git(&inner, &["commit", "-q", "-m", "work"]);
    checkpoints.restore(&first).unwrap();
    assert_eq!(
        f.read("x/inner/work.txt").as_deref(),
        Some("the user's work\n")
    );
}

// The same with a nested repository that has no commit yet: git cannot add it, and no snapshot
// holds its files.
#[test]
fn a_directory_holding_an_empty_nested_repository_is_never_removed() {
    let f = fixture();
    f.write("x", "file x\n");
    let checkpoints = f.checkpoints();
    let first = checkpoints.snapshot("turn 1").unwrap();
    std::fs::remove_file(f.ws.join("x")).unwrap();
    let inner = f.ws.join("x/inner");
    std::fs::create_dir_all(&inner).unwrap();
    git(&inner, &["init", "-q"]);
    std::fs::write(inner.join("work.txt"), "the user's work\n").unwrap();
    checkpoints.restore(&first).unwrap();
    assert_eq!(
        f.read("x/inner/work.txt").as_deref(),
        Some("the user's work\n")
    );
}

// Review E minor 13: a private file comes back private, not with the default mode git gives it.
#[test]
fn private_files_come_back_private() {
    use std::os::unix::fs::PermissionsExt;
    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o7777;
    let set = |path: &Path, mode: u32| {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    };
    let f = fixture();
    f.write(".env", "SECRET=1\n");
    set(&f.ws.join(".env"), 0o600);
    f.write("id_key", "key\n");
    set(&f.ws.join("id_key"), 0o400);
    f.write("run.sh", "#!/bin/sh\n");
    set(&f.ws.join("run.sh"), 0o755);
    let checkpoints = f.checkpoints();
    let first = checkpoints.snapshot("turn 1").unwrap();
    std::fs::remove_file(f.ws.join(".env")).unwrap();
    std::fs::remove_file(f.ws.join("id_key")).unwrap();
    f.write("id_key", "replaced by the agent\n");
    f.write("run.sh", "#!/bin/sh\necho changed\n");
    checkpoints.restore(&first).unwrap();
    assert_eq!(f.read(".env").as_deref(), Some("SECRET=1\n"));
    assert_eq!(mode(&f.ws.join(".env")), 0o600);
    assert_eq!(f.read("id_key").as_deref(), Some("key\n"));
    assert_eq!(mode(&f.ws.join("id_key")), 0o400);
    assert_eq!(mode(&f.ws.join("run.sh")) & 0o100, 0o100);
}

// Review E issue 4 (probe C): a session continued from a subdirectory cannot restore a snapshot
// taken for the directory above: its paths would land in the wrong place.
#[test]
fn a_snapshot_is_restored_only_in_the_workspace_it_was_taken_for() {
    for repository in [false, true] {
        let f = fixture();
        if repository {
            git(&f.ws, &["init", "-q"]);
        }
        f.write("a.txt", "a\n");
        f.write("sub/x.txt", "x\n");
        let at_root = Checkpoints::open(&f.gitdir, &f.ws, "s1").unwrap();
        let first = at_root.snapshot("turn 1").unwrap();
        drop(at_root);
        f.write("sub/x.txt", "changed\n");
        let sub = f.ws.join("sub");
        let in_sub = Checkpoints::open(&f.gitdir, &sub, "s1").unwrap();
        match in_sub.restore(&first) {
            Err(CheckpointError::OtherWorkspace { taken }) => assert_eq!(taken, f.ws),
            other => panic!("repository {repository}: {other:?}"),
        }
        assert_eq!(f.read("sub/x.txt").as_deref(), Some("changed\n"));
        assert!(!sub.join("a.txt").exists());
        assert!(!sub.join("sub").exists());
    }
}

// Review E minor 11: a restore that fails after its pre-rewind snapshot names that snapshot, which
// holds the files as they were before the restore began.
#[test]
fn a_restore_that_fails_partway_names_the_snapshot_taken_before_it() {
    use std::os::unix::fs::PermissionsExt;
    if is_root() {
        return; // root writes into read-only directories
    }
    let f = fixture();
    f.write("ro/a.txt", "one\n");
    f.write("b.txt", "one\n");
    let checkpoints = f.checkpoints();
    let first = checkpoints.snapshot("turn 1").unwrap();
    f.write("ro/a.txt", "two\n");
    f.write("b.txt", "two\n");
    let ro = f.ws.join("ro");
    std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o555)).unwrap();
    let failed = checkpoints.restore(&first);
    std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o755)).unwrap();
    let Err(CheckpointError::Restore { before, .. }) = failed else {
        panic!("{failed:?}");
    };
    checkpoints.restore(&before).unwrap();
    assert_eq!(f.read("ro/a.txt").as_deref(), Some("two\n"));
    assert_eq!(f.read("b.txt").as_deref(), Some("two\n"));
}

// git only warns when it cannot remove a file; a restore that leaves one behind fails too, naming
// the snapshot taken before it.
#[test]
fn a_restore_that_cannot_remove_a_file_fails() {
    use std::os::unix::fs::PermissionsExt;
    if is_root() {
        return; // root writes into read-only directories
    }
    let f = fixture();
    f.write("b.txt", "one\n");
    let checkpoints = f.checkpoints();
    let first = checkpoints.snapshot("turn 1").unwrap();
    f.write("ro/new.txt", "made since\n");
    f.write("b.txt", "two\n");
    let ro = f.ws.join("ro");
    std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o555)).unwrap();
    let failed = checkpoints.restore(&first);
    std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o755)).unwrap();
    let Err(CheckpointError::Restore { before, source }) = failed else {
        panic!("{failed:?}");
    };
    assert!(source.to_string().contains("ro/new.txt"), "{source}");
    checkpoints.restore(&before).unwrap();
    assert_eq!(f.read("b.txt").as_deref(), Some("two\n"));
}

// Review E minor 9 (probe L): a shadow repository inside the workspace would snapshot itself, and
// sandboxed commands could change its configuration, which git reads outside the sandbox.
#[test]
fn a_shadow_repository_inside_the_workspace_is_refused() {
    let f = fixture();
    f.write("a.txt", "a\n");
    let base = f.ws.parent().unwrap();
    std::os::unix::fs::symlink(&f.ws, base.join("link")).unwrap();
    for gitdir in [
        f.ws.join(".local/share/harness/checkpoints/p.git"),
        base.join("link/data/p.git"),
        base.join("elsewhere/missing/../../ws/data/p.git"),
    ] {
        // Re-review E, nit c: the cause is where harness runs, and the message says so.
        match Checkpoints::open(&gitdir, &f.ws, "s1") {
            Err(e @ CheckpointError::InWorkspace { .. }) => {
                let message = e.to_string();
                assert!(
                    message.contains(&format!("inside the workspace {}", f.ws.display()))
                        && message.contains("run harness in a project directory"),
                    "{message}"
                );
            }
            other => panic!("{}: {other:?}", gitdir.display()),
        }
    }
    assert!(!f.ws.join(".local").exists());
    assert!(!f.ws.join("data").exists());
}

// The CLI checks the directories sandboxed commands can write to the same way.
#[test]
fn a_shadow_repository_where_commands_can_write_is_refused() {
    let f = fixture();
    let tmp = f.ws.parent().unwrap().join("tmp");
    std::fs::create_dir(&tmp).unwrap();
    let roots = [f.ws.clone(), tmp.clone()];
    match checkpoint::check_location(&tmp.join("harness/checkpoints/p.git"), &f.ws, &roots) {
        Err(CheckpointError::Exposed { root, .. }) => assert_eq!(root, tmp),
        other => panic!("{other:?}"),
    }
    // The workspace is among the roots the CLI passes: it is named as the cause.
    assert!(matches!(
        checkpoint::check_location(&f.ws.join("data/p.git"), &f.ws, &roots),
        Err(CheckpointError::InWorkspace { .. })
    ));
    checkpoint::check_location(&f.gitdir, &f.ws, &roots).unwrap();
}

// Review E minor 14: the snapshots of sessions that no longer exist are pruned: their refs, their
// files in the shadow repository, and the objects nothing else reaches.
#[test]
fn snapshots_of_sessions_that_are_gone_are_pruned() {
    let f = fixture();
    f.write("a.txt", "shared\n");
    let gone = Checkpoints::open(&f.gitdir, &f.ws, "gone").unwrap();
    f.write("b.txt", "only the session that is gone saw this\n");
    let old = gone.snapshot("turn 1").unwrap();
    let blob = git(&f.gitdir, &["rev-parse", &format!("{old}:b.txt")]);
    drop(gone);
    std::fs::remove_file(f.ws.join("b.txt")).unwrap();
    let live = Checkpoints::open(&f.gitdir, &f.ws, "live").unwrap();
    let kept = live.snapshot("turn 1").unwrap();
    // A later snapshot with the same record reuses it; the earlier one must stay reachable.
    f.write("a.txt", "turn 2\n");
    live.snapshot("turn 2").unwrap();
    // A session's last snapshot less than a day old may belong to one whose file is not written
    // yet: it stays.
    assert_eq!(live.prune(|id| id == "live").unwrap(), 0);
    let live = live.with_prune_age(Duration::ZERO);
    assert_eq!(live.prune(|id| id == "live").unwrap(), 1);
    assert_eq!(
        git(
            &f.gitdir,
            &["for-each-ref", "--format=%(refname)", "refs/harness/"]
        ),
        "refs/harness/live\n"
    );
    assert!(!f.gitdir.join("indexes/gone").exists());
    let exists = Command::new("git")
        .args(["--git-dir"])
        .arg(&f.gitdir)
        .args(["cat-file", "-e", blob.trim()])
        .status()
        .unwrap();
    assert!(!exists.success());
    f.write("a.txt", "changed\n");
    live.restore(&kept).unwrap();
    assert_eq!(f.read("a.txt").as_deref(), Some("shared\n"));
}

// An index can name objects that were pruned since (a new session starts from the last index any
// session wrote): the snapshot then starts again from an empty index rather than failing.
#[test]
fn an_index_naming_a_pruned_object_is_started_afresh() {
    let f = fixture();
    f.write("a.txt", "one\n");
    // An old file, so git trusts the index entry rather than hashing the file again.
    std::fs::File::options()
        .write(true)
        .open(f.ws.join("a.txt"))
        .unwrap()
        .set_modified(std::time::SystemTime::now() - Duration::from_secs(3600))
        .unwrap();
    let checkpoints = f.checkpoints();
    checkpoints.snapshot("turn 1").unwrap();
    // What pruning this session's snapshots, and the index new sessions start from, would leave.
    git(&f.gitdir, &["update-ref", "-d", "refs/harness/s1"]);
    std::fs::remove_file(f.gitdir.join("index")).unwrap();
    git(&f.gitdir, &["prune", "--expire=now"]);
    let second = checkpoints.snapshot("turn 2").unwrap();
    assert_eq!(
        checkpoints.files(&second).unwrap(),
        [PathBuf::from("a.txt")]
    );
}

// Review E probe I: names that look like options, pathspec magic or glob patterns round-trip,
// whether snapshots hold them, leave them out as large, or ignore them.
#[test]
fn hostile_names_round_trip_and_stay_literal() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let f = fixture();
    let names: [&[u8]; 11] = [
        b"-rf",
        b"--force",
        b"new\nline",
        b" lead",
        b"trail ",
        b"#hash",
        b"!bang",
        b":(top)x",
        b":(exclude)y",
        b"*",
        b"[a]",
    ];
    for name in names {
        std::fs::write(f.ws.join(OsStr::from_bytes(name)), b"orig").unwrap();
    }
    f.write("a", "matched by the glob [a], were it one\n");
    let large = vec![b'x'; MAX_FILE_SIZE as usize + 1];
    std::fs::write(f.ws.join(OsStr::from_bytes(b":(glob)*big")), &large).unwrap();
    f.write(".gitignore", "*.ign\n");
    std::fs::write(f.ws.join(OsStr::from_bytes(b"-z\n.ign")), b"ignored").unwrap();
    let checkpoints = f.checkpoints();
    let first = checkpoints.snapshot("turn 1").unwrap();
    let files = checkpoints.files(&first).unwrap();
    assert_eq!(files.len(), names.len() + 2, "{files:?}");
    for name in names {
        std::fs::write(f.ws.join(OsStr::from_bytes(name)), b"changed").unwrap();
    }
    std::fs::remove_file(f.ws.join("-rf")).unwrap();
    std::fs::remove_file(f.ws.join("*")).unwrap();
    std::fs::write(f.ws.join(OsStr::from_bytes(b":(glob)*big")), b"small").unwrap();
    std::fs::write(f.ws.join(OsStr::from_bytes(b"-z\n.ign")), b"still ignored").unwrap();
    checkpoints.restore(&first).unwrap();
    for name in names {
        assert_eq!(
            std::fs::read(f.ws.join(OsStr::from_bytes(name))).unwrap(),
            b"orig",
            "{}",
            String::from_utf8_lossy(name)
        );
    }
    assert_eq!(
        f.read("a").as_deref(),
        Some("matched by the glob [a], were it one\n")
    );
    assert_eq!(
        std::fs::read(f.ws.join(OsStr::from_bytes(b":(glob)*big"))).unwrap(),
        b"small"
    );
    assert_eq!(
        std::fs::read(f.ws.join(OsStr::from_bytes(b"-z\n.ign"))).unwrap(),
        b"still ignored"
    );
}

// Review E probe K: the user's repository configuration and attributes run nothing, now that
// git's work tree is the repository and its info/exclude is read.
#[test]
fn the_users_git_config_and_attributes_run_nothing() {
    let f = fixture();
    git(&f.ws, &["init", "-q"]);
    let base = f.ws.parent().unwrap();
    let marker = base.join("ran");
    let script = base.join("evil.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\necho \"$0 $*\" >> {}\ncat\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let config = f.ws.join(".git/config");
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str(&format!(
        "[filter \"evil\"]\n\tclean = {s}\n\tsmudge = {s}\n\tprocess = {s}\n[core]\n\tfsmonitor = {s}\n\thooksPath = {h}\n\tattributesFile = {a}\n[diff \"evil\"]\n\ttextconv = {s}\n",
        s = script.display(),
        h = base.display(),
        a = base.join("attributes").display()
    ));
    std::fs::write(&config, text).unwrap();
    std::fs::write(base.join("attributes"), "* filter=evil\n").unwrap();
    std::fs::write(
        f.ws.join(".git/info/attributes"),
        "* filter=evil diff=evil\n",
    )
    .unwrap();
    f.write(".gitattributes", "* filter=evil diff=evil\n");
    f.write("sub/a.txt", "one\n");
    for workspace in [f.ws.clone(), f.ws.join("sub")] {
        let checkpoints = Checkpoints::open(&f.gitdir, &workspace, "s1").unwrap();
        let first = checkpoints.snapshot("turn 1").unwrap();
        f.write("sub/a.txt", "two\n");
        checkpoints.restore(&first).unwrap();
        assert_eq!(f.read("sub/a.txt").as_deref(), Some("one\n"));
    }
    assert!(
        !marker.exists(),
        "{}",
        std::fs::read_to_string(&marker).unwrap_or_default()
    );
}

// Review E probe E and minor 10: what is inside a nested repository, with commits or without, is
// neither snapshotted nor rewound, and a restore removes none of it.
#[test]
fn nested_repositories_are_left_alone() {
    let f = fixture();
    let inner = f.ws.join("inner");
    std::fs::create_dir(&inner).unwrap();
    git(&inner, &["init", "-q"]);
    std::fs::write(inner.join("f.txt"), "one\n").unwrap();
    git(&inner, &["add", "f.txt"]);
    git(&inner, &["commit", "-q", "-m", "c"]);
    let empty = f.ws.join("empty");
    std::fs::create_dir(&empty).unwrap();
    git(&empty, &["init", "-q"]);
    std::fs::write(empty.join("g.txt"), "g\n").unwrap();
    f.write("top.txt", "t\n");
    let checkpoints = f.checkpoints();
    let first = checkpoints.snapshot("turn 1").unwrap();
    let files = checkpoints.files(&first).unwrap();
    assert!(!files.iter().any(|p| p.starts_with("empty")), "{files:?}");
    std::fs::write(inner.join("f.txt"), "changed\n").unwrap();
    std::fs::write(inner.join("new.txt"), "new\n").unwrap();
    std::fs::write(empty.join("g.txt"), "changed\n").unwrap();
    f.write("top.txt", "changed\n");
    checkpoints.restore(&first).unwrap();
    assert_eq!(f.read("top.txt").as_deref(), Some("t\n"));
    assert_eq!(f.read("inner/f.txt").as_deref(), Some("changed\n"));
    assert_eq!(f.read("inner/new.txt").as_deref(), Some("new\n"));
    assert_eq!(f.read("empty/g.txt").as_deref(), Some("changed\n"));
}

// Re-review E, V2: a restore that removes every file of a workspace in a repository's
// subdirectory must not remove the workspace, nor the empty directories above it: git removes
// directories it empties, up to its work tree, which is the repository's root.
#[test]
fn a_restore_that_empties_a_subdirectory_workspace_keeps_it_and_its_parents() {
    use std::os::unix::fs::PermissionsExt;
    let f = fixture();
    git(&f.ws, &["init", "-q"]);
    f.write("root.txt", "root\n");
    let ws = f.ws.join("a/b");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::set_permissions(&ws, std::fs::Permissions::from_mode(0o750)).unwrap();
    let checkpoints = Checkpoints::open(&f.gitdir, &ws, "s1").unwrap();
    let first = checkpoints.snapshot("turn 1").unwrap();
    std::fs::write(ws.join("new.txt"), "agent\n").unwrap();
    std::fs::create_dir_all(ws.join("deep")).unwrap();
    std::fs::write(ws.join("deep/x.txt"), "agent\n").unwrap();
    checkpoints.restore(&first).unwrap();
    assert!(ws.is_dir());
    assert!(!ws.join("new.txt").exists());
    assert!(!ws.join("deep").exists());
    assert_eq!(
        std::fs::metadata(&ws).unwrap().permissions().mode() & 0o777,
        0o750
    );
    assert_eq!(f.read("root.txt").as_deref(), Some("root\n"));
    // It still works: the next turn's snapshot and a rewind to it.
    std::fs::write(ws.join("again.txt"), "again\n").unwrap();
    let second = checkpoints.snapshot("turn 2").unwrap();
    std::fs::remove_file(ws.join("again.txt")).unwrap();
    checkpoints.restore(&second).unwrap();
    assert_eq!(f.read("a/b/again.txt").as_deref(), Some("again\n"));
}

// Re-review E, V2b: at the root, as before, nothing above the files is removed.
#[test]
fn a_restore_that_empties_a_root_workspace_keeps_it() {
    let f = fixture();
    let checkpoints = f.checkpoints();
    let first = checkpoints.snapshot("turn 1").unwrap();
    f.write("new/x.txt", "agent\n");
    checkpoints.restore(&first).unwrap();
    assert!(f.ws.is_dir());
    assert!(!f.ws.join("new").exists());
}

// Re-review E, V3: a workspace in a directory its repository ignores would get empty snapshots,
// and a rewind would restore nothing while claiming success. It is treated as a directory outside
// any repository instead: its own ignore files apply, not the repository's.
#[test]
fn a_workspace_its_repository_ignores_is_snapshotted_as_a_directory_of_its_own() {
    let f = fixture();
    git(&f.ws, &["init", "-q"]);
    f.write(".gitignore", "scratch/\n*.tmp\n");
    f.write("scratch/notes.txt", "one\n");
    f.write(
        "scratch/draft.tmp",
        "kept: only the workspace's own rules apply\n",
    );
    f.write("scratch/.gitignore", "*.log\n");
    f.write("scratch/debug.log", "log\n");
    let ws = f.ws.join("scratch");
    let checkpoints = Checkpoints::open(&f.gitdir, &ws, "s1").unwrap();
    let first = checkpoints.snapshot("turn 1").unwrap();
    assert_eq!(
        checkpoints.files(&first).unwrap(),
        [
            PathBuf::from(".gitignore"),
            PathBuf::from("draft.tmp"),
            PathBuf::from("notes.txt")
        ]
    );
    f.write("scratch/notes.txt", "changed by the agent\n");
    f.write("scratch/new.txt", "agent\n");
    checkpoints.restore(&first).unwrap();
    assert_eq!(f.read("scratch/notes.txt").as_deref(), Some("one\n"));
    assert_eq!(f.read("scratch/new.txt"), None);
    assert_eq!(f.read("scratch/debug.log").as_deref(), Some("log\n"));
}

// Re-review E, V3: the same for a directory under a home directory that is a repository ignoring
// everything, as dotfiles repositories do.
#[test]
fn a_workspace_under_a_home_repository_that_ignores_everything_is_snapshotted() {
    let f = fixture();
    git(&f.ws, &["init", "-q"]);
    f.write(".gitignore", "*\n");
    f.write("projects/foo/main.rs", "fn main() {}\n");
    let ws = f.ws.join("projects/foo");
    let checkpoints = Checkpoints::open(&f.gitdir, &ws, "s1").unwrap();
    let first = checkpoints.snapshot("turn 1").unwrap();
    assert_eq!(
        checkpoints.files(&first).unwrap(),
        [PathBuf::from("main.rs")]
    );
    f.write("projects/foo/main.rs", "broken\n");
    checkpoints.restore(&first).unwrap();
    assert_eq!(
        f.read("projects/foo/main.rs").as_deref(),
        Some("fn main() {}\n")
    );
}

// A workspace the repository does not ignore keeps the repository's rules (as in probe F), also
// when a rule re-includes it.
#[test]
fn a_workspace_a_negation_re_includes_keeps_the_repositorys_rules() {
    let f = fixture();
    git(&f.ws, &["init", "-q"]);
    f.write(".gitignore", "*\n!sub/\n!sub/**\nsub/*.log\n");
    f.write("sub/code.rs", "x\n");
    f.write("sub/debug.log", "x\n");
    let checkpoints = Checkpoints::open(&f.gitdir, &f.ws.join("sub"), "s1").unwrap();
    let first = checkpoints.snapshot("turn 1").unwrap();
    assert_eq!(
        checkpoints.files(&first).unwrap(),
        [PathBuf::from("code.rs")]
    );
}

// Re-review E, nit d: a record is read only from a record commit. An older snapshot's parent is
// another snapshot, whose top-level file named `record` is the user's, whatever it holds.
#[test]
fn a_users_file_named_record_is_never_read_as_a_record() {
    use std::io::Write;
    let f = fixture();
    f.write("a.txt", "one\n");
    let checkpoints = f.checkpoints();
    let snapshot = checkpoints.snapshot("turn 1").unwrap();
    let tree = git(&f.gitdir, &["rev-parse", &format!("{snapshot}^{{tree}}")]);
    // An older snapshot whose workspace held a file `record` that looks like one.
    let forged = f.ws.parent().unwrap().join("forged");
    let mut bytes = b"harness snapshot record 1\0workspace ".to_vec();
    bytes.extend_from_slice(f.ws.as_os_str().as_encoded_bytes());
    bytes.push(0);
    std::fs::write(&forged, bytes).unwrap();
    let blob = git(&f.gitdir, &["hash-object", "-w", forged.to_str().unwrap()]);
    let mut mktree = Command::new("git")
        .arg("--git-dir")
        .arg(&f.gitdir)
        .arg("mktree")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    writeln!(
        mktree.stdin.take().unwrap(),
        "100644 blob {}\trecord",
        blob.trim()
    )
    .unwrap();
    let old_tree = String::from_utf8(mktree.wait_with_output().unwrap().stdout).unwrap();
    let older = git(
        &f.gitdir,
        &[
            "commit-tree",
            old_tree.trim(),
            "-m",
            "before a turn of session s0",
        ],
    );
    let old = git(
        &f.gitdir,
        &[
            "commit-tree",
            tree.trim(),
            "-p",
            older.trim(),
            "-m",
            "before a turn of session s0",
        ],
    );
    f.write("a.txt", "two\n");
    assert!(matches!(
        checkpoints.restore(old.trim()),
        Err(CheckpointError::NoRecord(_))
    ));
    assert_eq!(f.read("a.txt").as_deref(), Some("two\n"));
}

/// Every file under `dir`, relative to it, leaving out `.git`.
fn tree(dir: &Path) -> Vec<String> {
    fn go(base: &Path, dir: &Path, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.file_name().is_some_and(|n| n == ".git") {
                continue;
            }
            if path.is_dir() {
                go(base, &path, out);
            } else {
                out.push(path.strip_prefix(base).unwrap().display().to_string());
            }
        }
    }
    let mut out = Vec::new();
    go(dir, dir, &mut out);
    out.sort();
    out
}

// Re-review E, probe X: the repository ignored `scratch/` when the snapshot was taken there, so it
// was its own work tree; now it does not, so the repository is. The snapshot's paths are relative
// to the old root: restoring it would write at the repository's root and delete the workspace's
// files. It is refused before anything changes.
#[test]
fn a_snapshot_taken_under_another_work_tree_is_refused_x() {
    let f = fixture();
    git(&f.ws, &["init", "-q"]);
    f.write(".gitignore", "scratch/\n");
    f.write("root.txt", "root\n");
    f.write("scratch/notes.txt", "one\n");
    let ws = f.ws.join("scratch");
    let first = Checkpoints::open(&f.gitdir, &ws, "s1").unwrap();
    let snapshot = first.snapshot("turn 1").unwrap();
    drop(first);
    f.write(".gitignore", "\n");
    f.write("scratch/notes.txt", "two\n");
    f.write("scratch/mine.txt", "user file\n");
    let before = tree(&f.ws);
    let second = Checkpoints::open(&f.gitdir, &ws, "s1").unwrap();
    match second.restore(&snapshot) {
        Err(CheckpointError::OtherRoot { taken, now }) => {
            assert_eq!(taken, ws);
            assert_eq!(now, f.ws);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(tree(&f.ws), before);
    assert_eq!(f.read("scratch/notes.txt").as_deref(), Some("two\n"));
}

// Re-review E, probe Y: the reverse, from the repository's work tree to the workspace's own.
#[test]
fn a_snapshot_taken_under_another_work_tree_is_refused_y() {
    let f = fixture();
    git(&f.ws, &["init", "-q"]);
    f.write("root.txt", "root\n");
    f.write("scratch/notes.txt", "one\n");
    let ws = f.ws.join("scratch");
    let first = Checkpoints::open(&f.gitdir, &ws, "s1").unwrap();
    let snapshot = first.snapshot("turn 1").unwrap();
    drop(first);
    f.write(".gitignore", "scratch/\n");
    f.write("scratch/notes.txt", "two\n");
    let before = tree(&f.ws);
    let second = Checkpoints::open(&f.gitdir, &ws, "s1").unwrap();
    match second.restore(&snapshot) {
        Err(CheckpointError::OtherRoot { taken, now }) => {
            assert_eq!(taken, f.ws);
            assert_eq!(now, ws);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(tree(&f.ws), before);
    assert!(!f.ws.join("scratch/scratch").exists());
}

/// Whether the tests run as root, which permissions do not stop.
fn is_root() -> bool {
    // SAFETY: `geteuid` cannot fail.
    unsafe { libc::geteuid() == 0 }
}
