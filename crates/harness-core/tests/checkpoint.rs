use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

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
