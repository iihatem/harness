//! The checkpoint repository holds copies of workspace files, private ones included, so only its
//! owner may read it, as with session files. A test binary of its own: it sets the process's
//! umask.

use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use harness_core::checkpoint::Checkpoints;

fn mode(path: &Path) -> u32 {
    std::fs::symlink_metadata(path)
        .unwrap()
        .permissions()
        .mode()
        & 0o777
}

/// Every directory and file below `dir`.
fn below(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if std::fs::symlink_metadata(&path).unwrap().is_dir() {
            out.extend(below(&path));
        }
        out.push(path);
    }
    out
}

/// Asserts what `session_files_and_their_folders_are_private` asserts of sessions: folders 0700,
/// and files no one but their owner may read (git keeps objects read-only, 0400).
fn assert_private(gitdir: &Path) {
    assert_eq!(mode(gitdir), 0o700, "{}", gitdir.display());
    let paths = below(gitdir);
    assert!(paths.iter().any(|p| p.ends_with("records/s1")), "{paths:?}");
    assert!(
        paths.iter().any(|p| p.ends_with("pathspecs/s1")),
        "{paths:?}"
    );
    for path in paths {
        if std::fs::symlink_metadata(&path).unwrap().is_dir() {
            assert_eq!(mode(&path), 0o700, "{}", path.display());
        } else {
            assert_eq!(mode(&path) & 0o077, 0, "{}", path.display());
        }
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    ws: PathBuf,
    data: PathBuf,
    gitdir: PathBuf,
}

/// A workspace holding a private file, and a data directory that does not exist yet, with the
/// umask most systems default to.
fn fixture() -> Fixture {
    unsafe { libc::umask(0o022) };
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let ws = base.join("ws");
    std::fs::create_dir(&ws).unwrap();
    std::fs::write(ws.join("a.txt"), "a\n").unwrap();
    std::fs::write(ws.join(".env.local"), "SECRET=1\n").unwrap();
    std::fs::set_permissions(
        ws.join(".env.local"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let data = base.join("data");
    Fixture {
        _dir: dir,
        ws,
        gitdir: data.join("checkpoints/project.git"),
        data,
    }
}

// Final review, important 2 (probe p1): with umask 022 the data directory and the shadow
// repository were 0755 and its objects 0444, so another local user could read a copy of a 0600
// `.env.local`.
#[test]
fn the_checkpoint_repository_and_the_data_directory_are_private() {
    let f = fixture();
    std::fs::create_dir(f.ws.join("dir")).unwrap();
    std::fs::write(f.ws.join("dir/d.txt"), "d\n").unwrap();
    let checkpoints = Checkpoints::open(&f.gitdir, &f.ws, "s1").unwrap();
    let first = checkpoints.snapshot("turn 1").unwrap();
    std::fs::write(f.ws.join("a.txt"), "b\n").unwrap();
    std::fs::write(f.ws.join("new.txt"), "c\n").unwrap();
    std::fs::remove_dir_all(f.ws.join("dir")).unwrap();
    checkpoints.snapshot("turn 2").unwrap();
    checkpoints.restore(&first).unwrap();
    assert_eq!(mode(&f.data), 0o700);
    assert_eq!(mode(&f.data.join("checkpoints")), 0o700);
    assert_private(&f.gitdir);
    // What a restore writes in the workspace follows the umask, as before.
    assert_eq!(mode(&f.ws.join("a.txt")), 0o644);
    assert_eq!(mode(&f.ws.join("dir")), 0o755);
    assert_eq!(mode(&f.ws.join("dir/d.txt")), 0o644);
    assert_eq!(mode(&f.ws.join(".env.local")), 0o600);
}

// A repository an older harness left readable is made private when opened; the folder's mode
// alone keeps everyone else out of what it holds.
#[test]
fn an_existing_checkpoint_repository_is_made_private_when_opened() {
    let f = fixture();
    Checkpoints::open(&f.gitdir, &f.ws, "s1")
        .unwrap()
        .snapshot("turn 1")
        .unwrap();
    std::fs::set_permissions(&f.gitdir, std::fs::Permissions::from_mode(0o755)).unwrap();
    Checkpoints::open(&f.gitdir, &f.ws, "s2").unwrap();
    assert_eq!(mode(&f.gitdir), 0o700);
}
