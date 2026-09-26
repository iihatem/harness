//! Async-signal-safe cleanup of inherited file descriptors, for use inside
//! `pre_exec`.
//!
//! Stdin/stdout/stderr (0/1/2) are left untouched — `Command` has already
//! wired those up to whatever the caller asked for by the time `pre_exec`
//! runs. Every fd above that is marked close-on-exec, so it cannot leak into
//! the sandboxed program through inheritance (a duplicated listening
//! socket, a pipe another library opened without `O_CLOEXEC`, ...).
//!
//! This *marks* fds close-on-exec rather than closing them outright. That
//! distinction matters here: `std::process::Command`'s own fork/exec path
//! keeps an internal `O_CLOEXEC` pipe open across `pre_exec` to report an
//! `execve` failure back to the parent. That pipe is already close-on-exec,
//! so re-marking it is a no-op, but its fd number is not exposed to us, so
//! outright *closing* every fd >= 3 risks tearing down that channel and
//! turning a normal exec failure (e.g. "command not found") into a hang or
//! a misleading error. Marking close-on-exec achieves the same isolation —
//! nothing survives past the upcoming `execve` — without that risk.
//!
//! Every syscall here (`close_range`, `open`, `getdents64`, `fcntl`) is on
//! the POSIX async-signal-safe list; nothing in this module allocates.

use std::ffi::CStr;
use std::io;
use std::os::fd::RawFd;

/// Marks every fd above stderr close-on-exec, preferring the one-shot
/// `close_range(2)` syscall (Linux 5.11+) and falling back to walking
/// `/proc/self/fd` on kernels that do not have it.
pub(super) fn mark_inherited_fds_close_on_exec() {
    if close_range_cloexec(3, u32::MAX) {
        return;
    }
    mark_close_on_exec_via_proc();
}

/// Returns `true` on success. `close_range` is only available since Linux
/// 5.11; an older kernel (or a seccomp policy rejecting the syscall) reports
/// `ENOSYS`, in which case the caller falls back to the `/proc` walk.
fn close_range_cloexec(first: u32, last: u32) -> bool {
    // SAFETY: `close_range` takes only integers, no pointers; it can only
    // affect this process's own fd table.
    unsafe {
        libc::syscall(
            libc::SYS_close_range,
            first,
            last,
            libc::CLOSE_RANGE_CLOEXEC,
        ) == 0
    }
}

/// Fallback for kernels without `close_range`: read this process's open fds
/// out of `/proc/self/fd` (via the raw `getdents64` syscall, so no libc
/// directory-reading allocation is involved) and `fcntl(F_SETFD)` each one.
fn mark_close_on_exec_via_proc() {
    // Opened after fork: a directory fd opened in the parent would iterate
    // the parent's (possibly different) descriptor table, not this child's.
    // SAFETY: the path is a static, NUL-terminated C string; `open` performs
    // no allocation.
    let raw = unsafe {
        libc::open(
            c"/proc/self/fd".as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if raw < 0 {
        return;
    }
    let dir_fd = raw;

    let mut buf = [0u8; 4096];
    loop {
        // SAFETY: writes at most `buf.len()` bytes into stack storage owned
        // by this function.
        let count =
            unsafe { libc::syscall(libc::SYS_getdents64, dir_fd, buf.as_mut_ptr(), buf.len()) };
        if count == -1 {
            if io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            break;
        }
        if count == 0 {
            break;
        }

        let mut entries = &buf[..count as usize];
        // `linux_dirent64`: u64 d_ino (offset 0), u64 d_off (offset 8), u16
        // d_reclen (offset 16), u8 d_type (offset 18), then a
        // NUL-terminated d_name starting at offset 19. This layout is a
        // stable kernel ABI, independent of any libc's own `struct dirent`
        // definition. A record needs at least 20 bytes: the 19-byte header
        // plus a 1-byte (NUL-only, for an empty name) minimum name.
        while entries.len() >= 20 {
            let reclen = u16::from_ne_bytes([entries[16], entries[17]]) as usize;
            if reclen < 20 || reclen > entries.len() {
                break;
            }
            if let Ok(name) = CStr::from_bytes_until_nul(&entries[19..reclen])
                && let Ok(name) = name.to_str()
                && let Ok(fd) = name.parse::<RawFd>()
            {
                mark_cloexec(dir_fd, fd);
            }
            entries = &entries[reclen..];
        }
    }

    // SAFETY: closes only the directory fd this function opened above.
    unsafe {
        libc::close(dir_fd);
    }
}

fn mark_cloexec(dir_fd: RawFd, fd: RawFd) {
    if fd <= libc::STDERR_FILENO || fd == dir_fd {
        return;
    }
    // SAFETY: `fcntl(F_GETFD)`/`F_SETFD` on an fd this process owns; no
    // pointers, no allocation.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags == -1 {
        return;
    }
    if flags & libc::FD_CLOEXEC == 0 {
        unsafe {
            libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
        }
    }
}
