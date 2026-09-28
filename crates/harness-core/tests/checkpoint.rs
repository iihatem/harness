use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use harness_core::checkpoint::{CheckpointError, Checkpoints, MAX_FILE_SIZE};

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
