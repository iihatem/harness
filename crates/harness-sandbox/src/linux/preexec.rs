//! The part of the sandbox that actually runs in the forked child, from
//! `pre_exec`.
//!
//! # Why this is safe to run between `fork()` and `execve()`
//!
//! `pre_exec` runs in a child that is a byte-for-byte copy of the parent's
//! memory, sharing the parent's fd table, but with only the thread that
//! called `fork()` — every other thread (including one that might have been
//! holding the malloc arena lock, or a mutex inside the logging/allocator
//! machinery) simply does not exist in the child. Calling anything that
//! might allocate or take a lock can therefore deadlock the child forever.
//! POSIX documents the safe subset as the "async-signal-safe" function
//! list; every operation below is a member of it:
//!
//! - [`libc::setsid`], [`libc::prctl`], [`libc::syscall`] (used directly for
//!   `landlock_restrict_self` and, inside [`fdcleanup`], for `close_range`
//!   and `getdents64`), [`libc::open`], [`libc::fcntl`], [`libc::close`] are
//!   all thin wrappers around a single `syscall(2)` — no heap allocation, no
//!   userspace locking.
//! - [`seccompiler::apply_filter`] builds a `sock_fprog` on the stack that
//!   just points at the already-allocated [`seccompiler::BpfProgram`] slice
//!   (no allocation of its own) and calls `prctl`/`syscall(SYS_seccomp)`
//!   directly.
//!
//! What is deliberately **not** called here is the `landlock` crate's own
//! high-level `RulesetCreated::restrict_self()`. That method takes `self`
//! by value, which would run `RulesetCreated`'s (and its `Compatibility`
//! state's) `Drop` glue in the child — safe in the vast majority of cases,
//! but not something this crate wants to rely on being allocation-free
//! across every version of the `landlock` crate. Extracting the raw fd with
//! `Option<OwnedFd>::from(ruleset_created)` in the parent (see
//! `fs::build_ruleset_fd`) and calling the raw `landlock_restrict_self(2)`
//! syscall here avoids the question entirely.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd};

use seccompiler::BpfProgram;

use super::fdcleanup;

/// Everything [`apply`] needs, computed in the parent (see `fs.rs` and
/// `seccomp.rs`) before the child is forked.
pub(super) struct PreparedSandbox {
    /// The Landlock ruleset fd, or `None` when the kernel has no Landlock
    /// support at all (in which case there is nothing to restrict).
    pub(super) landlock_ruleset_fd: Option<OwnedFd>,
    /// The compiled network-deny seccomp-BPF program.
    pub(super) seccomp_program: BpfProgram,
}

/// Installs the sandbox in the calling process. Must only be invoked from a
/// `pre_exec` closure, after `fork()` and before `execve()`; see the module
/// docs for why every step here is async-signal-safe.
pub(super) fn apply(prepared: &PreparedSandbox) -> io::Result<()> {
    // 1. New session + process-group leader. Do not also call
    //    `setpgid(0, 0)`: `setsid()` already makes this process its own
    //    group leader, and `setpgid` targeting a process that is already a
    //    group leader fails with `EPERM`.
    setsid()?;

    // 2. Deny inheritance of any fd we did not explicitly wire up as
    //    stdio. Must happen before Landlock is restricted: it opens
    //    `/proc/self/fd` as a fallback path, which a restrictive ruleset
    //    could otherwise deny.
    fdcleanup::mark_inherited_fds_close_on_exec();

    // 3. Required before `seccomp(2)` will install a filter; applied ahead
    //    of Landlock too so nothing between here and `execve` could regain
    //    privileges (e.g. via a setuid/setgid binary) that the sandbox is
    //    about to remove.
    set_no_new_privs()?;

    // 4. Filesystem restriction. `None` means the kernel has no Landlock
    //    support at all, so there is nothing to restrict.
    if let Some(fd) = &prepared.landlock_ruleset_fd {
        landlock_restrict_self(fd.as_raw_fd())?;
    }

    // 5. Network restriction. Installed last so none of the syscalls above
    //    can themselves be filtered.
    seccompiler::apply_filter(&prepared.seccomp_program).map_err(io::Error::other)?;

    Ok(())
}

fn setsid() -> io::Result<()> {
    // SAFETY: takes no arguments; only touches this process's own
    // session/process-group bookkeeping in the kernel.
    if unsafe { libc::setsid() } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn set_no_new_privs() -> io::Result<()> {
    // SAFETY: `prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0)` takes only integers.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn landlock_restrict_self(ruleset_fd: std::os::fd::RawFd) -> io::Result<()> {
    // SAFETY: raw `landlock_restrict_self(2)` call. `ruleset_fd` was opened
    // in the parent by `fs::build_ruleset_fd` and stays valid across
    // `fork()` (the child inherits a working copy of every fd the parent
    // had open); `flags` is 0, matching every landlock ABI version.
    let rc = unsafe { libc::syscall(libc::SYS_landlock_restrict_self, ruleset_fd, 0) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
