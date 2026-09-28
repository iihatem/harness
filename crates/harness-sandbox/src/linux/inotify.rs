//! The kernel side of the git-metadata watcher (`crate::watch`): raw
//! `inotify(7)` through `libc`, which this crate already uses for its other
//! syscalls, with one watch per directory on a non-blocking descriptor, read
//! by a thread of its own in the harness process, and an `eventfd(2)` to stop
//! that thread.
//!
//! Each directory is opened through the guard's no-follow [`Tree`], from the
//! workspace, so no symlink on the way is followed, and the watch is added on
//! that descriptor (`/proc/self/fd/<n>`): it is pinned to the directory that
//! was looked at, which is what [`Source::add`] says it is.
//!
//! A watcher that cannot start (no inotify instance left, say) is counted,
//! and why is kept, for `harness sandbox doctor` ([`watcher_failures`]).

use std::ffi::CString;
use std::fmt;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::guard::nofollow::{Kind, Tree};
use crate::watch::{
    self, End, IN_IGNORED, IN_MOVE_SELF, IN_Q_OVERFLOW, Id, Lifetime, Source, Target, Wake, Watch,
};

// The bits `crate::watch` acts on, as it spells them.
const _: () = assert!(
    IN_MOVE_SELF == libc::IN_MOVE_SELF
        && IN_Q_OVERFLOW == libc::IN_Q_OVERFLOW
        && IN_IGNORED == libc::IN_IGNORED
);

/// What each watch reports: every way an entry in the directory, or the
/// directory itself, can be created, written, changed, moved or removed. The
/// watch is added through `/proc/self/fd/<n>`, a link the kernel follows to
/// the directory the descriptor is for.
const MASK: u32 = libc::IN_CREATE
    | libc::IN_MODIFY
    | libc::IN_CLOSE_WRITE
    | libc::IN_ATTRIB
    | libc::IN_MOVED_FROM
    | libc::IN_MOVED_TO
    | libc::IN_DELETE
    | libc::IN_DELETE_SELF
    | libc::IN_MOVE_SELF
    | libc::IN_ONLYDIR;

/// How many watchers could not start in this process.
static FAILURES: AtomicU64 = AtomicU64::new(0);

/// Why the last one that could not start did not.
static LAST_FAILURE: Mutex<Option<String>> = Mutex::new(None);

/// How many watchers of git metadata could not start in this process, and
/// why the last of them did not: the guard then checked only before and
/// after each command. For `harness sandbox doctor`.
pub fn watcher_failures() -> (u64, Option<String>) {
    let last = LAST_FAILURE
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    (FAILURES.load(Ordering::Relaxed), last)
}

fn record_failure(err: &io::Error) {
    *LAST_FAILURE.lock().unwrap_or_else(PoisonError::into_inner) = Some(err.to_string());
    FAILURES.fetch_add(1, Ordering::Relaxed);
}

#[cfg(test)]
thread_local! {
    /// The error the next watcher started on this thread fails with.
    static FAIL_NEXT: std::cell::Cell<Option<i32>> = const { std::cell::Cell::new(None) };
}

/// Makes the next watcher started on this thread fail with `errno`.
#[cfg(test)]
pub(super) fn fail_next_start(errno: i32) {
    FAIL_NEXT.with(|next| next.set(Some(errno)));
}

/// Room for many events: one takes 16 bytes plus a name of up to 256.
const BUF_BYTES: usize = 16 * 1024;

/// A watcher running on a thread of its own. Stopped by
/// [`stop`](Self::stop), or when dropped; between commands it can also end by
/// itself ([`ended`](Self::ended)).
pub(super) struct Watcher {
    /// Written to once, to stop the thread.
    stop: Arc<OwnedFd>,
    thread: Option<JoinHandle<End>>,
}

impl fmt::Debug for Watcher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Watcher")
            .field("ended", &self.ended())
            .finish_non_exhaustive()
    }
}

impl Watcher {
    /// Watches what `target` names in the workspace `root`, and starts the
    /// thread. Its watches are in place when this returns. A watcher that
    /// cannot start is counted: see [`watcher_failures`].
    pub(super) fn start(
        root: &Path,
        target: impl Target,
        lifetime: Lifetime,
    ) -> io::Result<Watcher> {
        let started = Watcher::spawn(root, target, lifetime);
        if let Err(err) = &started {
            record_failure(err);
        }
        started
    }

    fn spawn(root: &Path, target: impl Target, lifetime: Lifetime) -> io::Result<Watcher> {
        let name = match lifetime {
            Lifetime::Command => "harness-watch",
            Lifetime::Between { .. } => "harness-between",
        };
        let source = Inotify::new(root)?;
        let stop = Arc::clone(&source.stop);
        let watch = Watch::new(source, target, lifetime);
        let thread = std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                let end = watch::run_caught(watch);
                if matches!(end, End::Failed | End::Panicked) {
                    eprintln!(
                        "harness: the git-metadata watcher stopped ({end:?}); the checks \
                         around each command still run"
                    );
                }
                end
            })?;
        Ok(Watcher {
            stop,
            thread: Some(thread),
        })
    }

    /// Stops the thread and waits for it: no check of this watcher runs after
    /// this returns.
    pub(super) fn stop(mut self) {
        self.shut_down();
    }

    /// Whether the thread has ended.
    pub(super) fn ended(&self) -> bool {
        self.thread.as_ref().is_none_or(JoinHandle::is_finished)
    }

    fn shut_down(&mut self) {
        let Some(thread) = self.thread.take() else {
            return;
        };
        let one: u64 = 1;
        // SAFETY: writes the 8 bytes of `one` to the eventfd this watcher
        // owns; an eventfd write never blocks here, nor raises a signal.
        unsafe {
            libc::write(
                self.stop.as_raw_fd(),
                (&raw const one).cast(),
                std::mem::size_of::<u64>(),
            );
        }
        let _ = thread.join();
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.shut_down();
    }
}

/// An inotify descriptor, the eventfd that says stop, and the workspace the
/// watched directories are reached from.
struct Inotify {
    fd: OwnedFd,
    stop: Arc<OwnedFd>,
    tree: Tree,
    buf: Vec<u8>,
}

impl Inotify {
    fn new(root: &Path) -> io::Result<Inotify> {
        #[cfg(test)]
        if let Some(errno) = FAIL_NEXT.with(std::cell::Cell::take) {
            return Err(io::Error::from_raw_os_error(errno));
        }
        // SAFETY: takes only flags; returns a new descriptor or -1.
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` was just created, and nothing else owns it.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: takes an initial count and flags; returns a new descriptor
        // or -1.
        let stop = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        if stop < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `stop` was just created, and nothing else owns it.
        let stop = Arc::new(unsafe { OwnedFd::from_raw_fd(stop) });
        Ok(Inotify {
            fd,
            stop,
            tree: Tree::new(root),
            buf: vec![0; BUF_BYTES],
        })
    }
}

impl Source for Inotify {
    fn add(&mut self, dir: &Path, known: &dyn Fn(Id) -> bool) -> Option<(i32, Id)> {
        let pinned = self.tree.dir(dir).ok()?;
        let stat = pinned.stat_self().ok()?;
        let id = (stat.dev, stat.ino);
        if stat.kind != Kind::Dir || known(id) {
            return None;
        }
        let path = CString::new(format!("/proc/self/fd/{}", pinned.raw())).ok()?;
        // SAFETY: `path` is NUL-terminated and outlives the call, and
        // `pinned` keeps the descriptor it names open until it returns.
        let wd = unsafe { libc::inotify_add_watch(self.fd.as_raw_fd(), path.as_ptr(), MASK) };
        (wd >= 0).then_some((wd, id))
    }

    fn remove(&mut self, wd: i32) {
        // SAFETY: takes two integers; a watch already gone is an error, and
        // nothing else.
        unsafe { libc::inotify_rm_watch(self.fd.as_raw_fd(), wd) };
    }

    fn wait(&mut self, timeout: Option<Duration>) -> io::Result<Wake> {
        let mut fds = [
            libc::pollfd {
                fd: self.fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: self.stop.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // Rounded up, so it never wakes before the time given.
        let ms = timeout.map_or(-1, |timeout| {
            i32::try_from(timeout.as_nanos().div_ceil(1_000_000)).unwrap_or(i32::MAX)
        });
        // SAFETY: `fds` is an array of two `pollfd`s, which outlives the call.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), 2, ms) };
        if ready < 0 {
            let err = io::Error::last_os_error();
            return match err.kind() {
                io::ErrorKind::Interrupted => Ok(Wake::Timeout),
                _ => Err(err),
            };
        }
        if fds[1].revents != 0 {
            return Ok(Wake::Stop);
        }
        if fds[0].revents & libc::POLLIN == 0 {
            if fds[0].revents != 0 {
                return Err(io::Error::other("the inotify descriptor failed"));
            }
            return Ok(Wake::Timeout);
        }
        // SAFETY: reads at most `self.buf.len()` bytes into `self.buf`.
        let read = unsafe {
            libc::read(
                self.fd.as_raw_fd(),
                self.buf.as_mut_ptr().cast(),
                self.buf.len(),
            )
        };
        match usize::try_from(read) {
            Ok(read) => Ok(Wake::Events(watch::events(&self.buf[..read]))),
            Err(_) => {
                let err = io::Error::last_os_error();
                match err.kind() {
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => Ok(Wake::Timeout),
                    _ => Err(err),
                }
            }
        }
    }

    fn now(&self) -> Instant {
        Instant::now()
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::*;
    use crate::guard::{GuardSession, WatchHandle};

    fn count(checks: &AtomicUsize) -> usize {
        checks.load(Ordering::SeqCst)
    }

    /// Polls `done` every 5 ms for up to 5 s.
    fn wait_until(what: &str, done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting until {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Counts its checks, and apart those that followed a change it found
    /// relevant (`heeded`): the check of a tick, every two seconds, follows
    /// none unless one came, so what `heeded` counts does not depend on when
    /// ticks fall. A change to one of `names`, or to a watched directory
    /// itself, is relevant; with `everything`, any change is. Each check runs
    /// `on_check` in the first directory. Records how many checks had run
    /// when it was told no process is left.
    struct Counting {
        dirs: Vec<PathBuf>,
        names: &'static [&'static str],
        everything: bool,
        checks: Arc<AtomicUsize>,
        heeded: Arc<AtomicUsize>,
        /// A relevant change came since the last check.
        armed: AtomicBool,
        on_check: fn(&Path, usize),
        gone: Arc<Mutex<Option<usize>>>,
    }

    impl Counting {
        fn new(dirs: &[&Path]) -> (Counting, Arc<AtomicUsize>) {
            let checks = Arc::new(AtomicUsize::new(0));
            let counting = Counting {
                dirs: dirs.iter().map(|dir| dir.to_path_buf()).collect(),
                names: &["config"],
                everything: false,
                checks: Arc::clone(&checks),
                heeded: Arc::default(),
                armed: AtomicBool::new(false),
                on_check: |_, _| {},
                gone: Arc::default(),
            };
            (counting, checks)
        }
    }

    impl Target for Counting {
        fn dirs(&self) -> Vec<PathBuf> {
            self.dirs.clone()
        }

        fn relevant(&self, _dir: &Path, name: Option<&OsStr>) -> bool {
            let relevant =
                self.everything || name.is_none_or(|name| self.names.iter().any(|n| name == *n));
            if relevant {
                self.armed.store(true, Ordering::SeqCst);
            }
            relevant
        }

        fn check(&self) {
            if self.armed.swap(false, Ordering::SeqCst) {
                self.heeded.fetch_add(1, Ordering::SeqCst);
            }
            let n = self.checks.fetch_add(1, Ordering::SeqCst);
            (self.on_check)(&self.dirs[0], n);
        }

        fn survivors_gone(&self) {
            *self.gone.lock().unwrap() = Some(count(&self.checks));
        }
    }

    /// The directory a test watches from.
    fn root(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().to_path_buf()
    }

    #[test]
    fn a_relevant_change_runs_a_check_and_others_do_not() {
        let dir = tempfile::tempdir().unwrap();
        let (target, checks) = Counting::new(&[dir.path()]);
        let heeded = Arc::clone(&target.heeded);
        let watcher = Watcher::start(&root(&dir), target, Lifetime::Command).unwrap();
        std::fs::write(dir.path().join("index.lock"), "x").unwrap();
        std::thread::sleep(Duration::from_millis(200));
        // At most the check of a tick.
        assert!(count(&checks) <= 1, "{} checks", count(&checks));
        assert_eq!(count(&heeded), 0);
        std::fs::write(dir.path().join("config"), "x").unwrap();
        wait_until("the change is checked", || count(&heeded) > 0);
        watcher.stop();
    }

    #[test]
    fn a_restore_that_keeps_failing_sets_off_no_checks_of_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let (mut target, checks) = Counting::new(&[dir.path()]);
        target.everything = true;
        // As a failing restore does: a temporary file written, then removed.
        target.on_check = |dir, n| {
            let temp = dir.join(format!(".harness-restore-{}-{n}", std::process::id()));
            std::fs::write(&temp, "saved\n").unwrap();
            std::fs::remove_file(&temp).unwrap();
        };
        let heeded = Arc::clone(&target.heeded);
        let watcher = Watcher::start(&root(&dir), target, Lifetime::Command).unwrap();
        std::fs::write(dir.path().join("pre-commit"), "echo pwned\n").unwrap();
        wait_until("the change is checked", || count(&heeded) > 0);
        std::thread::sleep(Duration::from_millis(500));
        // The planted file's own events may come in two reads; the check of
        // a tick may come besides.
        assert!(count(&heeded) <= 2, "{} checks heeded", count(&heeded));
        assert!(count(&checks) <= 3, "{} checks", count(&checks));
        watcher.stop();
    }

    /// Counts the checks of a real guard's handle.
    struct CountingHandle(WatchHandle, Arc<AtomicUsize>);

    impl Target for CountingHandle {
        fn dirs(&self) -> Vec<PathBuf> {
            self.0.dirs()
        }

        fn relevant(&self, dir: &Path, name: Option<&OsStr>) -> bool {
            self.0.relevant(dir, name)
        }

        fn check(&self) {
            self.1.fetch_add(1, Ordering::SeqCst);
            self.0.check();
        }
    }

    #[test]
    fn a_restore_by_the_guard_does_not_set_off_checks_of_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let ws = base.join("ws");
        std::fs::create_dir_all(ws.join(".git/hooks")).unwrap();
        std::fs::write(ws.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(ws.join(".git/config"), "[core]\n\tbare = false\n").unwrap();
        std::fs::write(ws.join(".git/hooks/pre-commit"), "exit 0\n").unwrap();
        let session = GuardSession::new(&base.join("quarantine"));
        let guard = session.begin(&ws, true, |_| {});
        let checks = Arc::new(AtomicUsize::new(0));
        let target = CountingHandle(guard.watch_handle(), Arc::clone(&checks));
        let watcher = Watcher::start(&ws, target, Lifetime::Command).unwrap();
        std::fs::write(ws.join(".git/hooks/pre-commit"), "echo pwned\n").unwrap();
        wait_until("the hook is restored", || {
            std::fs::read_to_string(ws.join(".git/hooks/pre-commit")).ok()
                == Some("exit 0\n".into())
        });
        std::thread::sleep(Duration::from_millis(300));
        let settled = count(&checks);
        // The change, its write in a second read, and what the restore did.
        assert!(settled <= 3, "{settled} checks");
        std::thread::sleep(Duration::from_millis(500));
        // At most the check of a tick.
        assert!(
            count(&checks) <= settled + 1,
            "the checks set each other off: {settled}, then {}",
            count(&checks)
        );
        watcher.stop();
        let report = guard.finish().expect("a report");
        assert!(
            report
                .message
                .contains("- .git/hooks/pre-commit: changed; restored the earlier version"),
            "{}",
            report.message
        );
    }

    #[test]
    fn a_directory_moved_away_and_made_again_is_watched_at_its_path() {
        let dir = tempfile::tempdir().unwrap();
        let watched = dir.path().join("hooks");
        std::fs::create_dir(&watched).unwrap();
        // As the guard's handle does: a protected directory made again is a
        // relevant change in the directory above it.
        let (mut target, checks) = Counting::new(&[&watched, dir.path()]);
        target.names = &["config", "hooks"];
        let heeded = Arc::clone(&target.heeded);
        let watcher = Watcher::start(&root(&dir), target, Lifetime::Command).unwrap();
        let old = dir.path().join("elsewhere");
        std::fs::rename(&watched, &old).unwrap();
        std::fs::create_dir(&watched).unwrap();
        wait_until("the move is checked", || count(&checks) > 0);
        std::thread::sleep(Duration::from_millis(200));
        let before = count(&heeded);
        std::fs::write(old.join("config"), "x").unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(count(&heeded), before, "a change where it went was checked");
        std::fs::write(watched.join("config"), "x").unwrap();
        wait_until("a change at its path is checked", || {
            count(&heeded) > before
        });
        watcher.stop();
    }

    #[test]
    fn a_watcher_between_commands_ends_by_itself_once_no_process_is_left() {
        let dir = tempfile::tempdir().unwrap();
        let (target, checks) = Counting::new(&[dir.path()]);
        let gone = Arc::clone(&target.gone);
        let alive = Arc::new(AtomicBool::new(true));
        let lifetime = Lifetime::Between {
            alive: Box::new({
                let alive = Arc::clone(&alive);
                move || alive.load(Ordering::SeqCst)
            }),
        };
        let watcher = Watcher::start(&root(&dir), target, lifetime).unwrap();
        wait_until("it checks as it starts", || count(&checks) >= 1);
        let before = count(&checks);
        std::fs::write(dir.path().join("config"), "x").unwrap();
        wait_until("a change is checked", || count(&checks) > before);
        assert!(!watcher.ended());
        let before_the_end = count(&checks);
        alive.store(false, Ordering::SeqCst);
        let deadline = Instant::now() + watch::TICK + Duration::from_secs(2);
        while !watcher.ended() {
            assert!(Instant::now() < deadline, "the watcher is still running");
            std::thread::sleep(Duration::from_millis(10));
        }
        let last = count(&checks);
        assert!(last > before_the_end, "no last check");
        assert_eq!(
            *gone.lock().unwrap(),
            Some(last),
            "told after the last check"
        );
    }

    #[test]
    fn a_directory_reached_through_a_symlink_is_not_watched() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        std::fs::create_dir(elsewhere.path().join("hooks")).unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), dir.path().join(".git")).unwrap();
        let (mut target, checks) = Counting::new(&[&dir.path().join(".git/hooks")]);
        target.everything = true;
        let heeded = Arc::clone(&target.heeded);
        let watcher = Watcher::start(&root(&dir), target, Lifetime::Command).unwrap();
        std::fs::write(elsewhere.path().join("hooks/config"), "x").unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(count(&heeded), 0, "it watched where the symlink points");
        // At most the check of a tick.
        assert!(count(&checks) <= 1, "{} checks", count(&checks));
        watcher.stop();
    }

    #[test]
    fn a_watcher_that_cannot_start_is_counted_with_why() {
        let dir = tempfile::tempdir().unwrap();
        let (target, _) = Counting::new(&[dir.path()]);
        let (failed, _) = watcher_failures();
        fail_next_start(libc::EMFILE);
        let err = Watcher::start(&root(&dir), target, Lifetime::Command).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EMFILE));
        let (now, why) = watcher_failures();
        assert!(now > failed);
        assert!(why.is_some());
    }

    #[test]
    fn stopping_a_watcher_waits_for_its_thread() {
        let dir = tempfile::tempdir().unwrap();
        let (target, checks) = Counting::new(&[dir.path()]);
        let watcher = Watcher::start(&root(&dir), target, Lifetime::Command).unwrap();
        watcher.stop();
        std::fs::write(dir.path().join("config"), "x").unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(count(&checks), 0);
    }
}
