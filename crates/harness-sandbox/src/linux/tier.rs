//! Chooses the git-protection tier: a throwaway child tries the full tier's
//! exact setup (`mountns.rs`) on a temporary directory, then a write through
//! the read-only bind it made.
//!
//! The child is forked, never exec'd, and tests the write with a raw `open`:
//! no shell decides what a refused write means. (A shell did, once:
//! `: > pin/file` under `/bin/sh`, which is dash on Debian and Ubuntu, exits
//! the whole shell with status 2 when the redirection fails, because `:` is
//! a special builtin, so a working full tier read as a failed probe.)

use std::ffi::{CStr, CString};
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use harness_core::tool::GitProtection;

use super::mountns::{self, MountPlan};
use super::mountplan;

/// The probe child's exit status when the write through the read-only bind
/// failed with `EROFS`, as the full tier needs.
const REFUSED: i32 = 0;
/// ... when the write went through.
const ALLOWED: i32 = 200;
/// ... when the setup failed; the setup pipe says at which step.
const NO_SETUP: i32 = 201;
/// ... when it could not change into the probe's directory.
const NO_DIR: i32 = 202;
// Any other status is the `errno` the write failed with (see `errno_code`).

/// The git-protection tier this host supports, probed once per process: the
/// full tier where a child can set up a user and mount namespace with a
/// read-only bind in it, the basic tier (saying why) where it cannot.
pub fn linux_git_protection() -> GitProtection {
    static TIER: OnceLock<GitProtection> = OnceLock::new();
    TIER.get_or_init(|| match probe() {
        Ok(()) => GitProtection::Full,
        Err(reason) => GitProtection::Basic { reason },
    })
    .clone()
}

/// Forks a child that sets up a user and mount namespace with a pinned
/// directory and a read-only file in it, then tries to open the file for
/// writing. `Err` says why the full tier is unavailable.
///
/// The child stays in harness's session and is waited for here, by pid, so
/// the basic tier's reaper (`crate::procs`) never takes it.
fn probe() -> Result<(), String> {
    let dir =
        ProbeDir::create().map_err(|e| format!("the probe could not create a directory: {e}"))?;
    let plan = mountplan::probe(dir.path())
        .ok_or_else(|| "the probe could not describe its directory".to_string())?;
    let path = CString::new(dir.path().as_os_str().as_bytes())
        .map_err(|_| "the probe's directory has a NUL in its path".to_string())?;
    let (reader, writer) =
        mountplan::setup_pipe().map_err(|e| format!("the probe could not make a pipe: {e}"))?;
    // SAFETY: the child runs only `child`, which is async-signal-safe (raw
    // syscalls on data prepared above, no allocation, no locks; see
    // `mountns`), then `_exit`s without running any destructor, so it never
    // removes the probe's directory.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(format!(
            "the probe could not fork: {}",
            io::Error::last_os_error()
        ));
    }
    if pid == 0 {
        let code = child(&path, &plan, writer.as_raw_fd());
        // SAFETY: ends the forked child at once.
        unsafe { libc::_exit(code) };
    }
    drop(writer);
    let exit = wait_for(pid).map_err(|e| format!("the probe could not wait for its child: {e}"))?;
    verdict(exit, || {
        mountplan::read_failure(&reader)
            .map(|failure| failure.describe(&[dir.path().join("pin"), dir.path().join("pin/file")]))
    })
}

/// The forked child: changes into the probe's directory `dir`, sets up the
/// namespace and mounts of `plan` (a failed step is written to `report`),
/// and tests the write. Returns the status to exit with.
fn child(dir: &CStr, plan: &MountPlan, report: RawFd) -> i32 {
    // SAFETY: `chdir` on a NUL-terminated path.
    if unsafe { libc::chdir(dir.as_ptr()) } != 0 {
        return NO_DIR;
    }
    if mountns::enter(plan, Some(report)).is_err() {
        return NO_SETUP;
    }
    write_test(c"pin/file")
}

/// Opens `file` for writing, without creating or truncating it:
/// [`REFUSED`] when that fails with `EROFS`, [`ALLOWED`] when it succeeds,
/// and otherwise the `errno` ([`errno_code`]). Async-signal-safe.
fn write_test(file: &CStr) -> i32 {
    // SAFETY: `open` on a NUL-terminated path; the flags create nothing.
    let fd = unsafe {
        libc::open(
            file.as_ptr(),
            libc::O_WRONLY | libc::O_NOCTTY | libc::O_CLOEXEC,
        )
    };
    if fd >= 0 {
        // SAFETY: closes the fd just opened.
        unsafe { libc::close(fd) };
        return ALLOWED;
    }
    errno_code(
        io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EIO),
    )
}

/// The exit status for a write that failed with `errno`: [`REFUSED`] for
/// `EROFS`, otherwise the `errno` itself, kept below the statuses that mean
/// something else (Linux's are below 134).
fn errno_code(errno: i32) -> i32 {
    if errno == libc::EROFS {
        REFUSED
    } else {
        errno.clamp(1, ALLOWED - 1)
    }
}

/// How the probe's child ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Exit {
    Code(i32),
    Signal(i32),
}

/// Waits for the child `pid`.
fn wait_for(pid: libc::pid_t) -> io::Result<Exit> {
    let mut status = 0;
    loop {
        // SAFETY: waits for this process's own child `pid`, writing to
        // `status`.
        if unsafe { libc::waitpid(pid, &mut status, 0) } == pid {
            break;
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
    Ok(if libc::WIFEXITED(status) {
        Exit::Code(libc::WEXITSTATUS(status))
    } else {
        Exit::Signal(libc::WTERMSIG(status))
    })
}

/// What `exit` says about the full tier. `failure` reads the step the setup
/// failed at, if the child wrote one.
fn verdict(exit: Exit, failure: impl FnOnce() -> Option<String>) -> Result<(), String> {
    match exit {
        Exit::Code(REFUSED) => Ok(()),
        Exit::Code(ALLOWED) => Err("a read-only bind mount did not stop a write".into()),
        Exit::Code(NO_SETUP) => Err(failure()
            .unwrap_or_else(|| "the probe's setup failed without saying which step".into())),
        Exit::Code(NO_DIR) => Err("the probe could not change into its directory".into()),
        Exit::Code(errno @ 1..ALLOWED) => Err(format!(
            "a write through a read-only bind mount failed with {}, not with EROFS",
            io::Error::from_raw_os_error(errno)
        )),
        Exit::Code(code) => Err(format!(
            "the probe's child exited with the unexpected status {code}"
        )),
        Exit::Signal(signal) => Err(format!("the probe's child was killed by signal {signal}")),
    }
}

/// `<temp>/harness-userns-probe-<pid>-<n>/pin/file`, removed on drop.
struct ProbeDir(PathBuf);

impl ProbeDir {
    fn create() -> std::io::Result<ProbeDir> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        // Removed on drop from here on, whatever fails below.
        let mut dir = ProbeDir(std::env::temp_dir().join(format!(
            "harness-userns-probe-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        )));
        std::fs::create_dir_all(dir.0.join("pin"))?;
        dir.0 = dir.0.canonicalize()?;
        std::fs::write(dir.0.join("pin/file"), b"probe")?;
        Ok(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ProbeDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failure() -> Option<String> {
        Some("writing /proc/self/uid_map failed: Operation not permitted (os error 1)".into())
    }

    #[test]
    fn a_write_refused_with_erofs_is_the_full_tier() {
        assert_eq!(verdict(Exit::Code(REFUSED), failure), Ok(()));
    }

    #[test]
    fn every_other_outcome_says_why_the_full_tier_is_unavailable() {
        assert_eq!(
            verdict(Exit::Code(ALLOWED), failure),
            Err("a read-only bind mount did not stop a write".into())
        );
        assert_eq!(
            verdict(Exit::Code(NO_SETUP), failure),
            Err(failure().unwrap())
        );
        assert_eq!(
            verdict(Exit::Code(NO_SETUP), || None),
            Err("the probe's setup failed without saying which step".into())
        );
        assert_eq!(
            verdict(Exit::Code(NO_DIR), failure),
            Err("the probe could not change into its directory".into())
        );
        let other = verdict(Exit::Code(libc::EACCES), failure).unwrap_err();
        assert!(
            other.starts_with(
                "a write through a read-only bind mount failed with Permission denied"
            ),
            "{other}"
        );
        assert!(other.ends_with("not with EROFS"), "{other}");
        assert_eq!(
            verdict(Exit::Signal(libc::SIGKILL), failure),
            Err("the probe's child was killed by signal 9".into())
        );
        assert_eq!(
            verdict(Exit::Code(250), failure),
            Err("the probe's child exited with the unexpected status 250".into())
        );
    }

    #[test]
    fn no_errno_reads_as_another_outcome() {
        for errno in 1..=libc::EHWPOISON {
            assert!(![REFUSED, ALLOWED, NO_SETUP, NO_DIR].contains(&errno_code(errno)));
        }
        assert_eq!(errno_code(libc::EROFS), REFUSED);
        assert_eq!(errno_code(10_000), ALLOWED - 1);
    }

    /// The write test is raw syscalls: no shell decides what a refused write exits with.
    #[test]
    fn the_write_test_tells_a_write_that_went_through_from_one_that_failed() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        std::fs::write(&file, b"probe").unwrap();
        let path = |p: &Path| CString::new(p.as_os_str().as_bytes()).unwrap();
        assert_eq!(write_test(&path(&file)), ALLOWED);
        assert_eq!(write_test(&path(&dir.path().join("missing"))), libc::ENOENT);
        assert_eq!(write_test(&path(dir.path())), libc::EISDIR);
        assert_eq!(std::fs::read(&file).unwrap(), b"probe", "never truncated");
    }
}
