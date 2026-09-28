//! The full tier's mount setup, run in the forked child from `pre_exec`
//! (see `preexec.rs`), after the close-on-exec marking and before
//! `no_new_privs`, Landlock and seccomp.
//!
//! The child unshares a user and a mount namespace, which gives it every
//! capability in the new user namespace, maps its own uid and gid 1:1,
//! makes every mount private, and then, for each [`MountOp`] in order,
//! self-binds the entry at its path relative to the workspace: read-write
//! for a pin (a mount point can no longer be renamed, removed or replaced)
//! and read-only for a protected entry (`crate::mounts` says which is
//! which). It changes into its working directory again, so that a working
//! directory inside a newly covered directory resolves through the new
//! mount. Last, it locks the securebits (no root privileges, no set-uid
//! fixups, no ambient capabilities, all locked, on top of any it inherited)
//! and drops every capability it holds. Without that, `execve` would clear them only for a command
//! that is not uid 0 in the namespace, and harness may run as root.
//!
//! Every step works on file descriptors: entries are opened with `openat2`
//! beneath the workspace without following any symlink, checked against the
//! device and inode the parent saw when it built the plan, cloned with
//! `open_tree`, made read-only with `mount_setattr` (which changes only that
//! flag, unlike a classic read-only remount, which must restate the locked
//! `nosuid`/`nodev`/`noexec` flags inside a user namespace), and attached
//! with `move_mount`. These calls need Linux 5.12; the Landlock ABI 3 floor
//! already requires 6.2.
//!
//! Like the rest of `pre_exec`, this is async-signal-safe: raw syscalls on
//! data the parent prepared, stack buffers, no allocation, no locks. A
//! failure returns the raw `errno`, and first writes a [`Failure`] record to
//! the setup pipe, so the parent can say which step failed.

use std::ffi::{CStr, CString};
use std::io;
use std::mem::{MaybeUninit, size_of};
use std::os::fd::RawFd;

use libc::c_uint;

use crate::mounts::{Failure, Step};

// From <linux/mount.h> and <linux/openat2.h>: stable kernel UAPI.
const AT_EMPTY_PATH: c_uint = 0x1000;
const AT_RECURSIVE: c_uint = 0x8000;
const OPEN_TREE_CLONE: c_uint = 1;
const OPEN_TREE_CLOEXEC: c_uint = libc::O_CLOEXEC as c_uint;
const MOVE_MOUNT_F_EMPTY_PATH: c_uint = 0x04;
const MOVE_MOUNT_T_EMPTY_PATH: c_uint = 0x40;
const MOUNT_ATTR_RDONLY: u64 = 0x01;
// From <linux/securebits.h> and <linux/capability.h>.
const SECBIT_NOROOT: libc::c_ulong = 1 << 0;
const SECBIT_NOROOT_LOCKED: libc::c_ulong = 1 << 1;
const SECBIT_NO_SETUID_FIXUP: libc::c_ulong = 1 << 2;
const SECBIT_NO_SETUID_FIXUP_LOCKED: libc::c_ulong = 1 << 3;
const SECBIT_KEEP_CAPS_LOCKED: libc::c_ulong = 1 << 5;
const SECBIT_NO_CAP_AMBIENT_RAISE: libc::c_ulong = 1 << 6;
const SECBIT_NO_CAP_AMBIENT_RAISE_LOCKED: libc::c_ulong = 1 << 7;
/// What the child sets and locks: root gets no capability from `execve`,
/// changing uids adjusts none, `keep_caps` stays off, and nothing is raised
/// into the ambient set.
const LOCKED_SECUREBITS: libc::c_ulong = SECBIT_NOROOT
    | SECBIT_NOROOT_LOCKED
    | SECBIT_NO_SETUID_FIXUP
    | SECBIT_NO_SETUID_FIXUP_LOCKED
    | SECBIT_KEEP_CAPS_LOCKED
    | SECBIT_NO_CAP_AMBIENT_RAISE
    | SECBIT_NO_CAP_AMBIENT_RAISE_LOCKED;
const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;
pub(super) const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
pub(super) const RESOLVE_NO_SYMLINKS: u64 = 0x04;
pub(super) const RESOLVE_BENEATH: u64 = 0x08;

/// `struct open_how`.
#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

/// `struct mount_attr`.
#[repr(C)]
struct MountAttr {
    attr_set: u64,
    attr_clr: u64,
    propagation: u64,
    userns_fd: u64,
}

/// `struct __user_cap_header_struct`.
#[repr(C)]
struct CapHeader {
    version: u32,
    pid: i32,
}

/// `struct __user_cap_data_struct`: version 3 takes two, for 64 bits.
#[repr(C)]
#[derive(Clone, Copy)]
struct CapData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

/// Everything the child needs, built by the parent (`mountplan.rs`).
#[derive(Debug)]
pub(super) struct MountPlan {
    /// The canonical workspace.
    pub(super) workspace: CString,
    /// `/proc/self/uid_map` and `gid_map` contents: `"<id> <id> 1\n"`.
    pub(super) uid_map: Vec<u8>,
    pub(super) gid_map: Vec<u8>,
    /// Parents before children.
    pub(super) ops: Vec<MountOp>,
}

/// One self-bind.
#[derive(Debug)]
pub(super) struct MountOp {
    /// Relative to the workspace, never empty.
    pub(super) path: CString,
    /// Read-only, or a read-write pin.
    pub(super) read_only: bool,
    /// What the parent saw at `path`; anything else there fails the setup.
    pub(super) dev: u64,
    pub(super) ino: u64,
}

/// Sets up the namespace and mounts in the calling process. Must only be
/// called from `pre_exec` (or another single-threaded child before
/// `execve`). On failure writes a [`Failure`] to `report`, if any, and
/// returns the `errno`.
pub(super) fn enter(plan: &MountPlan, report: Option<RawFd>) -> io::Result<()> {
    setup(plan).map_err(|failure| {
        if let Some(fd) = report {
            let bytes = failure.encode();
            // SAFETY: writes `bytes`, a stack array, to a pipe the parent
            // created; a failed write only loses the diagnosis.
            unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        }
        io::Error::from_raw_os_error(failure.errno)
    })
}

fn setup(plan: &MountPlan) -> Result<(), Failure> {
    let fail = |step: Step| {
        move |errno: i32| Failure {
            step,
            op: None,
            errno,
        }
    };
    // SAFETY: `unshare` takes only flags. The child is single-threaded, as
    // `CLONE_NEWUSER` requires.
    check(unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS) }.into())
        .map_err(fail(Step::Unshare))?;
    write_file(c"/proc/self/setgroups", b"deny").map_err(fail(Step::Setgroups))?;
    write_file(c"/proc/self/uid_map", &plan.uid_map).map_err(fail(Step::UidMap))?;
    write_file(c"/proc/self/gid_map", &plan.gid_map).map_err(fail(Step::GidMap))?;
    // SAFETY: null source, type and data are valid for a propagation change.
    check(
        unsafe {
            libc::mount(
                std::ptr::null(),
                c"/".as_ptr(),
                std::ptr::null(),
                libc::MS_REC | libc::MS_PRIVATE,
                std::ptr::null(),
            )
        }
        .into(),
    )
    .map_err(fail(Step::Private))?;
    // Opened after `unshare`: the mount calls below only accept mounts in
    // the caller's own namespace.
    let workspace = openat2(
        libc::AT_FDCWD,
        &plan.workspace,
        libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
        RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS,
    )
    .map_err(fail(Step::Workspace))?;
    for (i, op) in plan.ops.iter().enumerate() {
        bind(workspace.0, op).map_err(|(step, errno)| Failure {
            step,
            op: u16::try_from(i).ok(),
            errno,
        })?;
    }
    drop(workspace);
    chdir_again().map_err(fail(Step::Chdir))?;
    // The securebits first: setting them takes `CAP_SETPCAP`, which the
    // next step drops.
    lock_securebits().map_err(fail(Step::Securebits))?;
    drop_capabilities().map_err(fail(Step::Capabilities))
}

/// Sets and locks [`LOCKED_SECUREBITS`], keeping every bit already set:
/// a locked bit cannot be cleared, so a call that tried would fail.
fn lock_securebits() -> Result<(), i32> {
    // SAFETY: `prctl` with integer arguments only.
    let current = check(unsafe { libc::prctl(libc::PR_GET_SECUREBITS, 0, 0, 0, 0) }.into())?;
    let bits = securebits_to_set(current as libc::c_ulong);
    // SAFETY: `prctl` with integer arguments only.
    check(unsafe { libc::prctl(libc::PR_SET_SECUREBITS, bits, 0, 0, 0) }.into()).map(|_| ())
}

/// The securebits to set when `current` are set: ours, and whatever was
/// already there (harness may inherit a locked bit, `keep-caps` from
/// systemd, say).
fn securebits_to_set(current: libc::c_ulong) -> libc::c_ulong {
    current | LOCKED_SECUREBITS
}

/// Empties this process's effective, permitted and inheritable capability
/// sets, which empties the ambient set too.
fn drop_capabilities() -> Result<(), i32> {
    let header = CapHeader {
        version: LINUX_CAPABILITY_VERSION_3,
        pid: 0,
    };
    let none = [CapData {
        effective: 0,
        permitted: 0,
        inheritable: 0,
    }; 2];
    // SAFETY: `capset` reads `header` and the two `CapData` of version 3,
    // all on the stack.
    check(unsafe { libc::syscall(libc::SYS_capset, &header as *const CapHeader, none.as_ptr()) })
        .map(|_| ())
}

/// Self-binds `op`'s entry, found beneath `workspace`, which is opened in
/// this namespace. A path through an earlier op's entry resolves through its
/// new mount, so a read-only entry lands on its pinned gitdir's mount.
fn bind(workspace: RawFd, op: &MountOp) -> Result<(), (Step, i32)> {
    let target = openat2(
        workspace,
        &op.path,
        libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS,
    )
    .map_err(|e| (Step::Open, e))?;
    let mut stat = MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `fstat` fills `stat`, a stack buffer of the right type.
    check(unsafe { libc::fstat(target.0, stat.as_mut_ptr()) }.into())
        .map_err(|e| (Step::Identity, e))?;
    // SAFETY: `fstat` succeeded, so it initialized `stat`.
    let stat = unsafe { stat.assume_init() };
    if stat.st_dev != op.dev || stat.st_ino != op.ino {
        return Err((Step::Identity, libc::ESTALE));
    }
    // SAFETY: `open_tree` on an fd with an empty path; the flags clone the
    // whole subtree into a new, detached mount.
    let tree = check(unsafe {
        libc::syscall(
            libc::SYS_open_tree,
            target.0,
            c"".as_ptr(),
            AT_EMPTY_PATH | OPEN_TREE_CLONE | OPEN_TREE_CLOEXEC | AT_RECURSIVE,
        )
    })
    .map(|fd| Fd(fd as RawFd))
    .map_err(|e| (Step::OpenTree, e))?;
    if op.read_only {
        let attr = MountAttr {
            attr_set: MOUNT_ATTR_RDONLY,
            attr_clr: 0,
            propagation: 0,
            userns_fd: 0,
        };
        // SAFETY: `attr` is a valid `struct mount_attr` of the size passed.
        check(unsafe {
            libc::syscall(
                libc::SYS_mount_setattr,
                tree.0,
                c"".as_ptr(),
                AT_EMPTY_PATH | AT_RECURSIVE,
                &attr as *const MountAttr,
                size_of::<MountAttr>(),
            )
        })
        .map_err(|e| (Step::ReadOnly, e))?;
    }
    // SAFETY: both paths are empty, so both fds are used as they are.
    check(unsafe {
        libc::syscall(
            libc::SYS_move_mount,
            tree.0,
            c"".as_ptr(),
            target.0,
            c"".as_ptr(),
            MOVE_MOUNT_F_EMPTY_PATH | MOVE_MOUNT_T_EMPTY_PATH,
        )
    })
    .map_err(|e| (Step::MoveMount, e))?;
    Ok(())
}

/// Changes into the current working directory by name, so it resolves
/// through the mounts just made.
fn chdir_again() -> Result<(), i32> {
    let mut path = [0u8; libc::PATH_MAX as usize];
    // SAFETY: `getcwd` writes at most `path.len()` bytes into `path`.
    check(unsafe { libc::syscall(libc::SYS_getcwd, path.as_mut_ptr(), path.len()) })?;
    // SAFETY: `getcwd` succeeded, so `path` holds a NUL-terminated path.
    check(unsafe { libc::chdir(path.as_ptr().cast()) }.into()).map(|_| ())
}

/// A file descriptor closed on drop.
pub(super) struct Fd(pub(super) RawFd);

impl Drop for Fd {
    fn drop(&mut self) {
        // SAFETY: closes an fd this process opened and nothing else owns.
        unsafe { libc::close(self.0) };
    }
}

/// `openat2(dirfd, path, flags, resolve)`.
pub(super) fn openat2(
    dirfd: RawFd,
    path: &CStr,
    flags: libc::c_int,
    resolve: u64,
) -> Result<Fd, i32> {
    let how = OpenHow {
        flags: flags as u64,
        mode: 0,
        resolve,
    };
    // SAFETY: `how` is a valid `struct open_how` of the size passed.
    check(unsafe {
        libc::syscall(
            libc::SYS_openat2,
            dirfd,
            path.as_ptr(),
            &how as *const OpenHow,
            size_of::<OpenHow>(),
        )
    })
    .map(|fd| Fd(fd as RawFd))
}

/// Writes all of `bytes` to the file at `path`.
fn write_file(path: &CStr, bytes: &[u8]) -> Result<(), i32> {
    // SAFETY: `path` is NUL-terminated; `open` allocates nothing.
    let fd = check(unsafe { libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC) }.into())
        .map(|fd| Fd(fd as RawFd))?;
    // SAFETY: writes from `bytes`, which outlives the call.
    let written = check(unsafe { libc::write(fd.0, bytes.as_ptr().cast(), bytes.len()) } as i64)?;
    if written as usize == bytes.len() {
        Ok(())
    } else {
        Err(libc::EIO)
    }
}

/// A syscall's return value, or the `errno` it set.
fn check(rc: i64) -> Result<i64, i32> {
    if rc < 0 {
        Err(io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EIO))
    } else {
        Ok(rc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uapi_structs_have_the_kernel_sizes() {
        assert_eq!(size_of::<OpenHow>(), 24);
        assert_eq!(size_of::<MountAttr>(), 32);
        assert_eq!(size_of::<CapHeader>(), 8);
        assert_eq!(size_of::<[CapData; 2]>(), 24);
    }

    #[test]
    fn securebits_already_set_or_locked_are_kept() {
        assert_eq!(securebits_to_set(0), LOCKED_SECUREBITS);
        // `keep-caps` and its lock, as systemd can leave them.
        assert_eq!(securebits_to_set(0x30), 0xff);
        // A bit this code does not know yet (`SECBIT_EXEC_RESTRICT_FILE`).
        assert_eq!(securebits_to_set(0x100), 0x1ef);
    }

    #[test]
    fn the_securebits_locked_are_the_ones_the_kernel_names() {
        // SECBIT_NOROOT, _NO_SETUID_FIXUP, _NO_CAP_AMBIENT_RAISE and their locks, and the lock
        // of SECBIT_KEEP_CAPS (0x10) without it.
        assert_eq!(LOCKED_SECUREBITS, 0xef);
    }
}
