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
use std::fs::{DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

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

/// `<temp>/harness-userns-probe-<random>/pin/file`: a directory made anew
/// (mode 0700) under a random name in the canonical temp directory, never
/// one that was there already, with `pin` and `pin/file` created in it
/// exclusively, without following a symlink. On drop it removes what it
/// made, entry by entry, and nothing else.
struct ProbeDir {
    path: PathBuf,
    pin: bool,
    file: bool,
}

/// How many random names [`ProbeDir::create`] tries.
const PROBE_NAMES: usize = 16;

impl ProbeDir {
    fn create() -> io::Result<ProbeDir> {
        let base = std::env::temp_dir()
            .canonicalize()
            .or_else(|_| Path::new("/tmp").canonicalize())?;
        let names = (0..PROBE_NAMES)
            .map(|_| random_name())
            .collect::<io::Result<Vec<String>>>()?;
        ProbeDir::create_in(&base, names)
    }

    /// In the canonical `base`, under the first of `names` where nothing is:
    /// `mkdir` never reuses an entry, a symlink included.
    fn create_in(base: &Path, names: impl IntoIterator<Item = String>) -> io::Result<ProbeDir> {
        for name in names {
            let path = base.join(name);
            match DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => return ProbeDir::fill(path),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "every name the probe tried was taken",
        ))
    }

    /// Makes `pin` and `pin/file` in the new directory at `path`.
    fn fill(path: PathBuf) -> io::Result<ProbeDir> {
        // Removed on drop from here on, as far as it is made.
        let mut dir = ProbeDir {
            path,
            pin: false,
            file: false,
        };
        DirBuilder::new().mode(0o700).create(dir.path.join("pin"))?;
        dir.pin = true;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(dir.path.join("pin/file"))?;
        dir.file = true;
        file.write_all(b"probe")?;
        Ok(dir)
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ProbeDir {
    fn drop(&mut self) {
        // `unlink` and `rmdir`: neither follows a symlink at the name, and
        // `rmdir` leaves a directory that holds anything it did not make.
        if self.file {
            let _ = std::fs::remove_file(self.path.join("pin/file"));
        }
        if self.pin {
            let _ = std::fs::remove_dir(self.path.join("pin"));
        }
        let _ = std::fs::remove_dir(&self.path);
    }
}

/// `harness-userns-probe-` and 16 random hex digits.
fn random_name() -> io::Result<String> {
    let mut bytes = [0u8; 8];
    // SAFETY: `getrandom` writes at most `bytes.len()` bytes into `bytes`.
    let n = unsafe { libc::getrandom(bytes.as_mut_ptr().cast(), bytes.len(), 0) };
    if n != bytes.len() as isize {
        return Err(if n < 0 {
            io::Error::last_os_error()
        } else {
            io::Error::new(io::ErrorKind::UnexpectedEof, "getrandom gave too few bytes")
        });
    }
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!("harness-userns-probe-{hex}"))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

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
        // `EROFS` is the outcome the probe looks for; every other errno is itself.
        assert_eq!(errno_code(libc::EROFS), REFUSED);
        for errno in (1..=libc::EHWPOISON).filter(|errno| *errno != libc::EROFS) {
            assert_eq!(errno_code(errno), errno);
            assert!(
                ![REFUSED, ALLOWED, NO_SETUP, NO_DIR].contains(&errno_code(errno)),
                "{errno}"
            );
        }
        assert_eq!(errno_code(10_000), ALLOWED - 1);
    }

    /// `base`, with a directory and a symlink planted where the probe looks first.
    fn planted() -> (tempfile::TempDir, PathBuf, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(base.join("probe-a")).unwrap();
        std::fs::write(base.join("probe-a/theirs"), b"x").unwrap();
        std::os::unix::fs::symlink(outside.path(), base.join("probe-b")).unwrap();
        (dir, base, outside)
    }

    #[test]
    fn the_probe_directory_is_made_anew_and_private_never_where_something_was_planted() {
        let (_dir, base, outside) = planted();
        let names = ["probe-a", "probe-b", "probe-c"].map(String::from);
        let probe = ProbeDir::create_in(&base, names).unwrap();
        assert_eq!(probe.path(), base.join("probe-c"));
        let mode = std::fs::metadata(probe.path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
        assert!(probe.path().join("pin").is_dir());
        assert_eq!(
            std::fs::read(probe.path().join("pin/file")).unwrap(),
            b"probe"
        );
        // What was planted is untouched: nothing was made in it, or through it.
        assert_eq!(
            std::fs::read_dir(base.join("probe-a")).unwrap().count(),
            1,
            "only its own file"
        );
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
        drop(probe);
        assert!(!base.join("probe-c").exists());
        assert!(base.join("probe-a/theirs").exists());
        assert!(
            std::fs::symlink_metadata(base.join("probe-b"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn with_every_name_taken_there_is_no_probe_directory() {
        let (_dir, base, outside) = planted();
        let names = ["probe-a", "probe-b"].map(String::from);
        assert!(ProbeDir::create_in(&base, names).is_err());
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[test]
    fn the_probe_removes_only_what_it_made() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let probe = ProbeDir::create_in(&base, ["probe".to_string()]).unwrap();
        std::fs::write(probe.path().join("pin/theirs"), b"x").unwrap();
        drop(probe);
        assert!(!base.join("probe/pin/file").exists());
        assert!(base.join("probe/pin/theirs").exists());
    }

    #[test]
    fn probe_directory_names_are_random() {
        let names: std::collections::BTreeSet<String> =
            (0..32).map(|_| random_name().unwrap()).collect();
        assert_eq!(names.len(), 32);
        assert!(
            names
                .iter()
                .all(|name| name.starts_with("harness-userns-probe-"))
        );
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
