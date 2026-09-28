//! Builds the full tier's mount plan in the parent, from the guard's index
//! of the workspace: which entries (`crate::mounts`), with the identities
//! the child checks and the uid and gid maps it writes (`mountns.rs`).
//!
//! A gitdir without `hooks/` first gets an empty one, which git treats
//! exactly like a missing one, so that planting hooks fails too. The
//! placeholder is left in place: removing it while another command uses it
//! as a mount point would detach that command's mount.

use std::collections::BTreeSet;
use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use super::mountns::{
    Fd, MountOp, MountPlan, RESOLVE_BENEATH, RESOLVE_NO_MAGICLINKS, RESOLVE_NO_SYMLINKS, openat2,
};
use crate::gitmeta::GitIndex;
use crate::mounts::{self, Failure};

/// A plan, and each op's absolute path, for messages.
pub(super) type Planned = (MountPlan, Vec<PathBuf>);

/// Gives each gitdir the full tier pins an empty `hooks/` if it has none,
/// then plans the mounts for the canonical `workspace` from `index`. `None`
/// when there is nothing to protect: the command then gets no namespace.
///
/// For `prepare`, this runs inside `GuardSession::begin`, between the scan
/// and the guard's record of which protected names exist, so the guard takes
/// the placeholders for the user's, and the plan sees what the guard does.
pub(super) fn plan(workspace: &Path, index: &GitIndex) -> Option<Planned> {
    let gitdirs = mounts::gitdirs(index);
    create_hooks_placeholders(workspace, &gitdirs);
    let mut ops = Vec::new();
    let mut paths = Vec::new();
    for mount in mounts::mounts(workspace, &gitdirs, index) {
        let Some(rel) = mount
            .path
            .strip_prefix(workspace)
            .ok()
            .and_then(|rel| CString::new(rel.as_os_str().as_bytes()).ok())
        else {
            continue;
        };
        ops.push(MountOp {
            path: rel,
            read_only: mount.read_only,
            dev: mount.dev,
            ino: mount.ino,
        });
        paths.push(mount.path);
    }
    if ops.is_empty() {
        return None;
    }
    let workspace = CString::new(workspace.as_os_str().as_bytes()).ok()?;
    Some((with_id_maps(workspace, ops), paths))
}

/// Creates an empty `hooks/` (mode 0755) in each gitdir in `gitdirs` that has
/// none. A gitdir that cannot be reached from `workspace` without following a
/// symlink is skipped.
fn create_hooks_placeholders(workspace: &Path, gitdirs: &BTreeSet<PathBuf>) {
    let Ok(root) = open_dir(libc::AT_FDCWD, workspace, RESOLVE_NO_SYMLINKS) else {
        return;
    };
    for gitdir in gitdirs {
        let Ok(rel) = gitdir.strip_prefix(workspace) else {
            continue;
        };
        if rel.as_os_str().is_empty() {
            continue;
        }
        let Ok(dir) = open_dir(root.0, rel, RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS) else {
            continue;
        };
        // SAFETY: creates `hooks` in the directory `dir` refers to; an
        // existing entry of that name makes it fail with `EEXIST`, which is
        // fine.
        unsafe { libc::mkdirat(dir.0, c"hooks".as_ptr(), 0o755) };
    }
}

fn open_dir(dirfd: i32, path: &Path, resolve: u64) -> Result<Fd, i32> {
    let path = CString::new(path.as_os_str().as_bytes()).map_err(|_| libc::EINVAL)?;
    openat2(
        dirfd,
        &path,
        libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
        resolve | RESOLVE_NO_MAGICLINKS,
    )
}

/// The plan for the probe (`tier.rs`): pin `dir/pin` and make
/// `dir/pin/file` read-only.
pub(super) fn probe(dir: &Path) -> Option<MountPlan> {
    let mut ops = Vec::new();
    for (rel, read_only) in [("pin", false), ("pin/file", true)] {
        let meta = std::fs::symlink_metadata(dir.join(rel)).ok()?;
        ops.push(MountOp {
            path: CString::new(rel).ok()?,
            read_only,
            dev: meta.dev(),
            ino: meta.ino(),
        });
    }
    Some(with_id_maps(
        CString::new(dir.as_os_str().as_bytes()).ok()?,
        ops,
    ))
}

/// The plan for `ops` in `workspace`, mapping this process's effective uid
/// and gid to themselves.
fn with_id_maps(workspace: CString, ops: Vec<MountOp>) -> MountPlan {
    // SAFETY: `geteuid` and `getegid` cannot fail.
    let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
    MountPlan {
        workspace,
        uid_map: format!("{uid} {uid} 1\n").into_bytes(),
        gid_map: format!("{gid} {gid} 1\n").into_bytes(),
        ops,
    }
}

/// A pipe for the child's setup failure: the read end for the parent, the
/// write end for the child. Both are close-on-exec and non-blocking.
pub(super) fn setup_pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: `pipe2` fills `fds` with two new descriptors on success.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: both descriptors were just created and are owned by nobody else.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// The failure the child wrote to `reader`, if it wrote one. Call it once
/// the child has exec'd or failed to: the pipe is non-blocking.
pub(super) fn read_failure(reader: &OwnedFd) -> Option<Failure> {
    let mut bytes = [0u8; Failure::LEN];
    // SAFETY: reads at most `bytes.len()` bytes into `bytes`; the pipe is
    // non-blocking, so this returns at once when it is empty.
    let n = unsafe { libc::read(reader.as_raw_fd(), bytes.as_mut_ptr().cast(), bytes.len()) };
    (n == bytes.len() as isize)
        .then(|| Failure::decode(&bytes))
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failure_written_by_the_child_is_read_back() {
        let (reader, writer) = setup_pipe().unwrap();
        assert_eq!(read_failure(&reader), None, "nothing written yet");
        let failure = Failure {
            step: mounts::Step::MoveMount,
            op: Some(2),
            errno: libc::EINVAL,
        };
        let bytes = failure.encode();
        // SAFETY: writes `bytes` to the pipe just made.
        let n = unsafe { libc::write(writer.as_raw_fd(), bytes.as_ptr().cast(), bytes.len()) };
        assert_eq!(n, bytes.len() as isize);
        assert_eq!(read_failure(&reader), Some(failure));
    }

    #[test]
    fn a_plan_names_its_entries_relative_to_the_workspace_and_maps_ids_one_to_one() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().canonicalize().unwrap();
        std::fs::create_dir(ws.join(".git")).unwrap();
        std::fs::write(ws.join(".git/config"), "").unwrap();
        let index = GitIndex {
            gitdirs: [ws.join(".git")].into(),
            dot_gits: [ws.join(".git")].into(),
            ..GitIndex::default()
        };
        let (plan, paths) = plan(&ws, &index).expect("a plan");
        // The missing `hooks/` got its placeholder, and is covered.
        assert!(ws.join(".git/hooks").is_dir());
        let ops: Vec<(&[u8], bool)> = plan
            .ops
            .iter()
            .map(|op| (op.path.as_bytes(), op.read_only))
            .collect();
        assert_eq!(
            ops,
            [
                (&b".git"[..], false),
                (b".git/config", true),
                (b".git/hooks", true)
            ]
        );
        assert_eq!(
            paths,
            [
                ws.join(".git"),
                ws.join(".git/config"),
                ws.join(".git/hooks")
            ]
        );
        assert_eq!(plan.workspace.as_bytes(), ws.as_os_str().as_bytes());
        // SAFETY: `geteuid` and `getegid` cannot fail.
        let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
        assert_eq!(plan.uid_map, format!("{uid} {uid} 1\n").into_bytes());
        assert_eq!(plan.gid_map, format!("{gid} {gid} 1\n").into_bytes());
    }

    #[test]
    fn nothing_to_protect_means_no_plan() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().canonicalize().unwrap();
        assert!(plan(&ws, &GitIndex::default()).is_none());
    }
}
