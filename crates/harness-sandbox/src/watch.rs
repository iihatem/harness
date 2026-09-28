//! The file watcher that runs the git-metadata guard's checks the moment a
//! protected name appears or changes (design D5): while a sandboxed command
//! runs, and in the Linux basic tier between commands, while processes a
//! command left running are alive.
//!
//! The kernel side, `inotify(7)`, is in `linux/inotify.rs`. This is the part
//! that does not depend on it, so it is tested on every host: reading inotify
//! events, which of them call for a check, and how often checks run.
//!
//! The watcher only shortens the time a planted file exists. The guard's
//! checks before and after each command are what the protection rests on, and
//! they, not the watcher, decide what to undo, through descriptors that never
//! follow a symlink: an event only says "check now". So:
//!
//! - It watches what [`Target::dirs`] names: each known gitdir, its
//!   `worktrees` and `modules`, the workspace root, and in the basic tier the
//!   directories inside protected entries. A directory it cannot watch (the
//!   per-user watch limit, say) is skipped. After each check it watches what
//!   has appeared since (a new `hooks/`, say), and a directory deleted or
//!   moved away (`IN_IGNORED`, `IN_MOVE_SELF`) is dropped, and whatever is
//!   at its path now watched instead.
//! - An event runs a check when [`Target::relevant`] says so, except for the
//!   names the guard itself gives files for a moment ([`guards_own`]), so
//!   that a restore cannot set off check after check. A lost event
//!   (`IN_Q_OVERFLOW`) always runs one.
//! - Checks are debounced. The first change is checked at once; while changes
//!   keep coming, the next check waits until [`DEBOUNCE`] after the last one
//!   ended, so a burst is checked as it starts and once after it. One watcher
//!   runs at most [`MAX_CHECKS`] checks, so a process that plants a name again
//!   and again cannot make the guard's findings grow without bound; the
//!   checks around each command still run.
//! - Between commands ([`Lifetime::Between`]) it checks once as soon as its
//!   watches are in place, since the previous command's guard finished before
//!   they were. Every [`TICK`] it asks whether any process the command left
//!   is still running, which also reaps those that exited; once none is, it
//!   checks one last time and ends.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::guard::WatchHandle;

/// `inotify(7)` event bits this module acts on, as the kernel defines them on
/// every architecture. `linux/inotify.rs` checks them against `libc`.
pub(crate) const IN_MOVE_SELF: u32 = 0x0000_0800;
pub(crate) const IN_Q_OVERFLOW: u32 = 0x0000_4000;
pub(crate) const IN_IGNORED: u32 = 0x0000_8000;

/// How long after a check ends the next one waits, at least.
pub(crate) const DEBOUNCE: Duration = Duration::from_millis(50);

/// How many checks one watcher runs, at most.
pub(crate) const MAX_CHECKS: usize = 1_000;

/// How often a watcher between commands asks whether any process the last
/// command left is still running.
pub(crate) const TICK: Duration = Duration::from_secs(2);

/// What the watcher needs from the guard: [`WatchHandle`], or a test double.
pub(crate) trait Target: Send + 'static {
    /// The directories to watch.
    fn dirs(&self) -> Vec<PathBuf>;
    /// Whether a change to `name` in the watched `dir`, or to `dir` itself
    /// when `name` is `None`, calls for a check.
    fn relevant(&self, dir: &Path, name: Option<&OsStr>) -> bool;
    /// Runs the checks, and undoes what they find.
    fn check(&self);
}

impl Target for WatchHandle {
    fn dirs(&self) -> Vec<PathBuf> {
        WatchHandle::dirs(self)
    }

    fn relevant(&self, dir: &Path, name: Option<&OsStr>) -> bool {
        WatchHandle::relevant(self, dir, name)
    }

    fn check(&self) {
        WatchHandle::check(self);
    }
}

/// One `struct inotify_event`: a change to `name` in the directory watched as
/// `wd`, or to that directory itself when `name` is `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Event {
    pub(crate) wd: i32,
    pub(crate) mask: u32,
    pub(crate) name: Option<OsString>,
}

/// The events in `buf`, as `read(2)` on an inotify descriptor returned them.
/// A record cut short is left out.
pub(crate) fn events(buf: &[u8]) -> Vec<Event> {
    // `struct inotify_event`: `int wd; uint32_t mask, cookie, len;`, then
    // `len` bytes of name, NUL-padded.
    const HEADER: usize = 16;
    let field = |header: &[u8; HEADER], at: usize| {
        [header[at], header[at + 1], header[at + 2], header[at + 3]]
    };
    let mut events = Vec::new();
    let mut rest = buf;
    while let Some((header, after)) = rest.split_first_chunk::<HEADER>() {
        let wd = i32::from_ne_bytes(field(header, 0));
        let mask = u32::from_ne_bytes(field(header, 4));
        let Ok(len) = usize::try_from(u32::from_ne_bytes(field(header, 12))) else {
            break;
        };
        let (Some(name), Some(next)) = (after.get(..len), after.get(len..)) else {
            break;
        };
        let name = name
            .split(|&b| b == 0)
            .next()
            .filter(|name| !name.is_empty())
            .map(|name| OsStr::from_bytes(name).to_os_string());
        events.push(Event { wd, mask, name });
        rest = next;
    }
    events
}

/// Whether `name` is one the guard itself gives a file for a moment: the
/// temporary file a restore writes next to what it restores
/// (`.harness-restore-…`), or an entry renamed in place because the
/// quarantine could not be used (`….harness-quarantine-<n>`). Git uses
/// neither name, and a check still looks at what is there.
fn guards_own(name: &OsStr) -> bool {
    const IN_PLACE: &[u8] = b".harness-quarantine-";
    let name = name.as_bytes();
    name.starts_with(b".harness-restore-")
        || name.windows(IN_PLACE.len()).any(|part| part == IN_PLACE)
}

/// What wakes the watcher.
#[derive(Debug)]
pub(crate) enum Wake {
    /// It is to stop.
    Stop,
    Events(Vec<Event>),
    /// The time given ran out.
    Timeout,
}

/// The kernel side of a watcher: `inotify(7)` on Linux, a stand-in in tests.
pub(crate) trait Source {
    /// Starts watching `dir`: its watch descriptor, or `None` when it cannot
    /// be watched.
    fn add(&mut self, dir: &Path) -> Option<i32>;
    /// Stops watching what `wd` is for.
    fn remove(&mut self, wd: i32);
    /// Waits for events or a stop, up to `timeout` (without one, until either
    /// comes).
    fn wait(&mut self, timeout: Option<Duration>) -> io::Result<Wake>;
    fn now(&self) -> Instant;
}

/// How long a watcher runs.
pub(crate) enum Lifetime {
    /// Until it is stopped: while a command runs.
    Command,
    /// Between commands: until it is stopped, or until `alive`, asked every
    /// [`TICK`], says no process the last command left is running.
    Between {
        alive: Box<dyn FnMut() -> bool + Send>,
    },
}

/// Why a watcher ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum End {
    /// It was stopped.
    Stopped,
    /// Between commands, no process the last command left is running.
    NoneLeft,
    /// The handle does nothing any more: it names no directory.
    Inert,
    /// Waiting for events failed.
    Failed,
    /// A check panicked.
    Panicked,
}

/// A watcher: what it watches, and when it checks.
pub(crate) struct Watch<S, T> {
    source: S,
    target: T,
    lifetime: Lifetime,
    /// Each watch descriptor, and the directory it was added for.
    watched: BTreeMap<i32, PathBuf>,
    /// A change that calls for a check came since the last one began.
    pending: bool,
    /// When the last check ended.
    last: Option<Instant>,
    /// How many more checks it may run.
    left: usize,
    /// When to ask next whether processes are left (between commands).
    tick: Option<Instant>,
}

impl<S: Source, T: Target> Watch<S, T> {
    /// A watcher of what `target` names, its watches in place.
    pub(crate) fn new(source: S, target: T, lifetime: Lifetime) -> Watch<S, T> {
        let mut watch = Watch {
            source,
            target,
            lifetime,
            watched: BTreeMap::new(),
            pending: false,
            last: None,
            left: MAX_CHECKS,
            tick: None,
        };
        watch.resync();
        watch
    }

    /// Watches until the watcher ends: see the module docs.
    pub(crate) fn run(mut self) -> End {
        if matches!(self.lifetime, Lifetime::Between { .. }) {
            self.tick = Some(self.source.now() + TICK);
            // What was changed before the watches were in place.
            self.check();
            if !self.resync() {
                return End::Inert;
            }
        }
        loop {
            let now = self.source.now();
            if self.tick.is_some_and(|tick| now >= tick) {
                if !self.alive() {
                    self.check();
                    return End::NoneLeft;
                }
                self.tick = Some(self.source.now() + TICK);
                // What appeared without an event here, or a handle that does
                // nothing since the next command began.
                if !self.resync() {
                    return End::Inert;
                }
                continue;
            }
            let due = self.due(now);
            if due.is_some_and(|due| now >= due) {
                self.check();
                if !self.resync() {
                    return End::Inert;
                }
                continue;
            }
            let until = due.into_iter().chain(self.tick).min();
            let timeout = until.map(|until| until.saturating_duration_since(now));
            match self.source.wait(timeout) {
                Ok(Wake::Stop) => return End::Stopped,
                Ok(Wake::Timeout) => {}
                Ok(Wake::Events(events)) => {
                    if self.take(&events) && !self.resync() {
                        return End::Inert;
                    }
                }
                Err(_) => return End::Failed,
            }
        }
    }

    /// When the check a change called for may run: at once after a quiet
    /// spell, else [`DEBOUNCE`] after the last check ended. `None` when none
    /// is called for, or none may run any more.
    fn due(&self, now: Instant) -> Option<Instant> {
        (self.pending && self.left > 0).then(|| self.last.map_or(now, |last| last + DEBOUNCE))
    }

    fn check(&mut self) {
        self.pending = false;
        let Some(left) = self.left.checked_sub(1) else {
            return;
        };
        self.left = left;
        self.target.check();
        self.last = Some(self.source.now());
    }

    /// Whether any process the last command left is running; always, while a
    /// command runs.
    fn alive(&mut self) -> bool {
        match &mut self.lifetime {
            Lifetime::Command => true,
            Lifetime::Between { alive } => alive(),
        }
    }

    /// Notes which of `events` call for a check. Whether the watches are to
    /// be brought up to date with what the target names.
    fn take(&mut self, events: &[Event]) -> bool {
        let mut resync = false;
        for event in events {
            if event.mask & IN_Q_OVERFLOW != 0 {
                // Events were lost: check everything, and watch what may have
                // appeared meanwhile.
                self.pending = true;
                resync = true;
                continue;
            }
            if !self.watched.contains_key(&event.wd) {
                // A watch dropped already.
                continue;
            }
            if event.mask & IN_IGNORED != 0 {
                // The directory is gone: watch whatever is at its path now.
                self.watched.remove(&event.wd);
                resync = true;
                continue;
            }
            if event.mask & IN_MOVE_SELF != 0 {
                // The watch would follow the directory wherever it went, and
                // what changes there would pass for changes here.
                self.source.remove(event.wd);
                self.watched.remove(&event.wd);
                self.pending = true;
                resync = true;
                continue;
            }
            let name = event.name.as_deref();
            if name.is_some_and(guards_own) {
                continue;
            }
            if let Some(dir) = self.watched.get(&event.wd)
                && self.target.relevant(dir, name)
            {
                self.pending = true;
            }
        }
        resync
    }

    /// Watches each directory the target names that is not watched yet.
    /// `false` when it names none: its handle does nothing any more.
    fn resync(&mut self) -> bool {
        let dirs = self.target.dirs();
        if dirs.is_empty() {
            return false;
        }
        let watching: BTreeSet<&PathBuf> = self.watched.values().collect();
        let new: Vec<PathBuf> = dirs
            .into_iter()
            .filter(|dir| !watching.contains(dir))
            .collect();
        for dir in new {
            if let Some(wd) = self.source.add(&dir) {
                // The same directory under two paths keeps the first.
                self.watched.entry(wd).or_insert(dir);
            }
        }
        true
    }
}

/// Runs `watch` until it ends. A check that panics ends it too, rather than
/// unwinding out of the watcher's thread.
pub(crate) fn run_caught<S: Source, T: Target>(watch: Watch<S, T>) -> End {
    std::panic::catch_unwind(AssertUnwindSafe(move || watch.run())).unwrap_or(End::Panicked)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

    use super::*;

    const IN_MODIFY: u32 = 0x0000_0002;
    const IN_CLOSE_WRITE: u32 = 0x0000_0008;
    const IN_MOVED_FROM: u32 = 0x0000_0040;
    const IN_MOVED_TO: u32 = 0x0000_0080;
    const IN_CREATE: u32 = 0x0000_0100;
    const IN_DELETE: u32 = 0x0000_0200;
    const IN_DELETE_SELF: u32 = 0x0000_0400;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// One inotify record, its name padded with NULs to `padded` bytes.
    fn record(wd: i32, mask: u32, name: &[u8], padded: usize) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&wd.to_ne_bytes());
        out.extend_from_slice(&mask.to_ne_bytes());
        out.extend_from_slice(&0u32.to_ne_bytes());
        out.extend_from_slice(&u32::try_from(padded).unwrap().to_ne_bytes());
        let mut name = name.to_vec();
        name.resize(padded, 0);
        out.extend_from_slice(&name);
        out
    }

    fn ev(wd: i32, mask: u32, name: &str) -> Event {
        Event {
            wd,
            mask,
            name: (!name.is_empty()).then(|| OsString::from(name)),
        }
    }

    #[test]
    fn events_are_parsed_with_their_names() {
        let mut buf = record(1, IN_CREATE, b"commondir", 16);
        buf.extend(record(2, IN_MOVE_SELF, b"", 0));
        buf.extend(record(-1, IN_Q_OVERFLOW, b"", 0));
        assert_eq!(
            events(&buf),
            [
                ev(1, IN_CREATE, "commondir"),
                ev(2, IN_MOVE_SELF, ""),
                ev(-1, IN_Q_OVERFLOW, ""),
            ]
        );
    }

    #[test]
    fn a_record_cut_short_is_left_out() {
        let buf = record(1, IN_CREATE, b"config", 16);
        assert_eq!(events(&buf[..buf.len() - 1]), []);
        assert_eq!(events(&buf[..10]), []);
        let mut two = record(1, IN_CREATE, b"config", 16);
        two.extend(record(2, IN_CREATE, b"hooks", 16));
        assert_eq!(events(&two[..two.len() - 3]), [ev(1, IN_CREATE, "config")]);
    }

    #[test]
    fn only_the_guards_own_temporary_names_are_its_own() {
        for own in [
            ".harness-restore-4242-0",
            "post-checkout.harness-quarantine-0",
            ".git.harness-quarantine-12",
        ] {
            assert!(guards_own(OsStr::new(own)), "{own}");
        }
        for other in [
            "config",
            "pre-commit",
            "harness-restore-1",
            ".harness",
            "HEAD",
        ] {
            assert!(!guards_own(OsStr::new(other)), "{other}");
        }
    }

    /// What a test's stand-in kernel and target share.
    #[derive(Default)]
    struct World {
        /// Virtual time.
        now: Duration,
        steps: VecDeque<Step>,
        next_wd: i32,
        added: Vec<(i32, PathBuf)>,
        removed: Vec<i32>,
        dirs: Vec<PathBuf>,
        /// When each check ran.
        checks: Vec<Duration>,
        /// What a check does besides: the events its own changes cause, say.
        on_check: Option<OnCheck>,
        panic_on_check: bool,
        waits: usize,
    }

    type OnCheck = Box<dyn FnMut(&mut World) + Send>;

    /// What the stand-in kernel does next.
    enum Step {
        At(Duration, Vec<Event>),
        Fail,
        Stop,
    }

    type Shared = Arc<Mutex<World>>;

    fn lock(world: &Shared) -> MutexGuard<'_, World> {
        world.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Stands in for inotify: delivers the world's steps in virtual time. A
    /// directory named `unwatchable` cannot be watched.
    struct Fake(Shared, Instant);

    impl Source for Fake {
        fn add(&mut self, dir: &Path) -> Option<i32> {
            let mut world = lock(&self.0);
            if dir.ends_with("unwatchable") {
                return None;
            }
            world.next_wd += 1;
            let wd = world.next_wd;
            world.added.push((wd, dir.to_path_buf()));
            Some(wd)
        }

        fn remove(&mut self, wd: i32) {
            lock(&self.0).removed.push(wd);
        }

        fn wait(&mut self, timeout: Option<Duration>) -> io::Result<Wake> {
            let mut world = lock(&self.0);
            world.waits += 1;
            assert!(world.waits < 100_000, "the watcher keeps waking up");
            let now = world.now;
            let next = match world.steps.front() {
                None => None,
                Some(Step::Stop) => return Ok(Wake::Stop),
                Some(Step::Fail) => return Err(io::Error::other("poll failed")),
                Some(Step::At(at, _)) => Some(*at),
            };
            match (next, timeout) {
                (Some(at), Some(timeout)) if now + timeout < at => {
                    world.now = now + timeout;
                    Ok(Wake::Timeout)
                }
                (Some(at), _) => {
                    let Some(Step::At(_, events)) = world.steps.pop_front() else {
                        unreachable!("the front step is an event")
                    };
                    world.now = now.max(at);
                    Ok(Wake::Events(events))
                }
                (None, Some(timeout)) => {
                    world.now = now + timeout;
                    Ok(Wake::Timeout)
                }
                (None, None) => Ok(Wake::Stop),
            }
        }

        fn now(&self) -> Instant {
            self.1 + lock(&self.0).now
        }
    }

    /// Records when it checks; every change is relevant but to `index.lock`.
    struct Recorder(Shared);

    impl Target for Recorder {
        fn dirs(&self) -> Vec<PathBuf> {
            lock(&self.0).dirs.clone()
        }

        fn relevant(&self, _dir: &Path, name: Option<&OsStr>) -> bool {
            name != Some(OsStr::new("index.lock"))
        }

        fn check(&self) {
            let mut world = lock(&self.0);
            assert!(!world.panic_on_check, "a check that fails");
            let now = world.now;
            world.checks.push(now);
            if let Some(mut on_check) = world.on_check.take() {
                on_check(&mut world);
                world.on_check = Some(on_check);
            }
        }
    }

    /// The workspace, its gitdir and `hooks`: watched as 1, 2 and 3.
    const DIRS: [&str; 3] = ["/ws", "/ws/.git", "/ws/.git/hooks"];

    fn world(dirs: &[&str]) -> Shared {
        Arc::new(Mutex::new(World {
            dirs: dirs.iter().map(PathBuf::from).collect(),
            ..World::default()
        }))
    }

    fn at(world: &Shared, when: u64, events: Vec<Event>) {
        lock(world).steps.push_back(Step::At(ms(when), events));
    }

    fn on_check(world: &Shared, f: impl FnMut(&mut World) + Send + 'static) {
        lock(world).on_check = Some(Box::new(f));
    }

    fn watch(world: &Shared, lifetime: Lifetime) -> Watch<Fake, Recorder> {
        Watch::new(
            Fake(Arc::clone(world), Instant::now()),
            Recorder(Arc::clone(world)),
            lifetime,
        )
    }

    fn run(world: &Shared, lifetime: Lifetime) -> End {
        watch(world, lifetime).run()
    }

    fn checks(world: &Shared) -> Vec<Duration> {
        lock(world).checks.clone()
    }

    fn added(world: &Shared) -> Vec<(i32, PathBuf)> {
        lock(world).added.clone()
    }

    #[test]
    fn what_the_target_names_is_watched_and_what_cannot_be_is_skipped() {
        let w = world(&["/ws", "/ws/unwatchable", "/ws/.git"]);
        at(&w, 0, vec![ev(2, IN_CREATE, "config")]);
        assert_eq!(run(&w, Lifetime::Command), End::Stopped);
        assert_eq!(
            added(&w),
            [(1, PathBuf::from("/ws")), (2, PathBuf::from("/ws/.git"))]
        );
        assert_eq!(checks(&w), [ms(0)]);
    }

    #[test]
    fn a_relevant_change_is_checked_at_once_and_others_not_at_all() {
        let w = world(&DIRS);
        at(&w, 0, vec![ev(2, IN_CREATE, "index.lock")]);
        at(&w, 100, vec![ev(2, IN_MOVED_TO, "config")]);
        assert_eq!(run(&w, Lifetime::Command), End::Stopped);
        assert_eq!(checks(&w), [ms(100)]);
    }

    #[test]
    fn the_guards_own_temporary_names_set_off_no_check() {
        // `relevant` says yes to each of these, as it does in a protected
        // directory: only their names keep them out.
        let w = world(&DIRS);
        let temp = ".harness-restore-4242-0";
        at(
            &w,
            0,
            vec![
                ev(3, IN_CREATE, temp),
                ev(3, IN_MODIFY, temp),
                ev(3, IN_CLOSE_WRITE, temp),
                ev(3, IN_DELETE, temp),
                ev(3, IN_MOVED_TO, "post-checkout.harness-quarantine-0"),
                ev(1, IN_MOVED_TO, "HEAD.harness-quarantine-2"),
            ],
        );
        assert_eq!(run(&w, Lifetime::Command), End::Stopped);
        assert_eq!(checks(&w), []);
    }

    #[test]
    fn a_restore_that_keeps_failing_does_not_set_off_checks_of_its_own() {
        // Each check tries to restore a hook: it writes a temporary file next
        // to it, and removes it again when the rename fails.
        let w = world(&DIRS);
        on_check(&w, |world| {
            let temp = ".harness-restore-1-7";
            let now = world.now;
            world.steps.push_front(Step::At(
                now,
                vec![
                    ev(3, IN_CREATE, temp),
                    ev(3, IN_CLOSE_WRITE, temp),
                    ev(3, IN_DELETE, temp),
                ],
            ));
        });
        at(&w, 0, vec![ev(3, IN_MODIFY, "pre-commit")]);
        assert_eq!(run(&w, Lifetime::Command), End::Stopped);
        assert_eq!(checks(&w), [ms(0)]);
    }

    #[test]
    fn a_check_that_undoes_a_change_is_followed_by_one_more_only() {
        // Its own move and restore show up as changes to the name; the next
        // check finds nothing to do, and changes nothing.
        let w = world(&DIRS);
        let mut first = true;
        on_check(&w, move |world| {
            if std::mem::take(&mut first) {
                let now = world.now;
                let temp = ".harness-restore-1-1";
                world.steps.push_front(Step::At(
                    now,
                    vec![
                        ev(2, IN_MOVED_FROM, "config"),
                        ev(2, IN_CREATE, temp),
                        ev(2, IN_MOVED_FROM, temp),
                        ev(2, IN_MOVED_TO, "config"),
                    ],
                ));
            }
        });
        at(&w, 0, vec![ev(2, IN_CLOSE_WRITE, "config")]);
        assert_eq!(run(&w, Lifetime::Command), End::Stopped);
        assert_eq!(checks(&w), [ms(0), ms(50)]);
    }

    #[test]
    fn a_burst_is_checked_as_it_starts_and_once_after_it() {
        let w = world(&DIRS);
        for when in [0, 10, 20, 30] {
            at(&w, when, vec![ev(3, IN_CREATE, "post-checkout")]);
        }
        assert_eq!(run(&w, Lifetime::Command), End::Stopped);
        assert_eq!(checks(&w), [ms(0), ms(50)]);
    }

    #[test]
    fn changes_that_keep_coming_are_checked_at_most_once_per_debounce() {
        let w = world(&DIRS);
        for when in (0..1000).step_by(5) {
            at(&w, when, vec![ev(3, IN_CREATE, "post-checkout")]);
        }
        assert_eq!(run(&w, Lifetime::Command), End::Stopped);
        let checks = checks(&w);
        assert!(
            checks.windows(2).all(|pair| pair[1] - pair[0] >= DEBOUNCE),
            "{checks:?}"
        );
        // One as it starts, one every 50 ms, and the last after it.
        assert_eq!(checks.len(), 21, "{checks:?}");
        assert_eq!(checks.last(), Some(&ms(1000)));
    }

    #[test]
    fn one_watcher_runs_at_most_max_checks() {
        let w = world(&DIRS);
        for n in 0..MAX_CHECKS + 100 {
            let when = u64::try_from(n).unwrap() * 60;
            at(&w, when, vec![ev(3, IN_CREATE, "post-checkout")]);
        }
        assert_eq!(run(&w, Lifetime::Command), End::Stopped);
        assert_eq!(checks(&w).len(), MAX_CHECKS);
    }

    #[test]
    fn a_lost_event_runs_a_check_and_watches_what_appeared_meanwhile() {
        let w = world(&["/ws", "/ws/.git"]);
        let watcher = watch(&w, Lifetime::Command);
        lock(&w).dirs.push(PathBuf::from("/ws/.git/hooks"));
        at(&w, 0, vec![ev(-1, IN_Q_OVERFLOW, "")]);
        assert_eq!(watcher.run(), End::Stopped);
        assert_eq!(checks(&w), [ms(0)]);
        assert!(
            added(&w).contains(&(3, PathBuf::from("/ws/.git/hooks"))),
            "{:?}",
            added(&w)
        );
    }

    #[test]
    fn a_directory_deleted_and_made_again_is_watched_again() {
        let w = world(&DIRS);
        at(
            &w,
            0,
            vec![ev(3, IN_DELETE_SELF, ""), ev(3, IN_IGNORED, "")],
        );
        // The watch it had is gone: whatever still comes for it is not heeded.
        at(&w, 100, vec![ev(3, IN_CREATE, "pre-commit")]);
        at(&w, 200, vec![ev(4, IN_CREATE, "pre-commit")]);
        assert_eq!(run(&w, Lifetime::Command), End::Stopped);
        assert_eq!(checks(&w), [ms(0), ms(200)]);
        assert_eq!(
            added(&w).last(),
            Some(&(4, PathBuf::from("/ws/.git/hooks")))
        );
    }

    #[test]
    fn a_directory_moved_away_is_no_longer_watched_where_it_went() {
        let w = world(&DIRS);
        at(&w, 0, vec![ev(3, IN_MOVE_SELF, "")]);
        // The kernel would go on reporting changes where it went.
        at(&w, 100, vec![ev(3, IN_CREATE, "pre-commit")]);
        assert_eq!(run(&w, Lifetime::Command), End::Stopped);
        assert_eq!(lock(&w).removed, [3]);
        assert_eq!(checks(&w), [ms(0)]);
        assert_eq!(
            added(&w).last(),
            Some(&(4, PathBuf::from("/ws/.git/hooks")))
        );
    }

    #[test]
    fn a_directory_that_appears_is_watched_after_the_next_check() {
        let w = world(&DIRS);
        on_check(&w, |world| {
            world.dirs.push(PathBuf::from("/ws/.git/worktrees"));
        });
        at(&w, 0, vec![ev(2, IN_CREATE, "worktrees")]);
        assert_eq!(run(&w, Lifetime::Command), End::Stopped);
        assert!(
            added(&w).contains(&(4, PathBuf::from("/ws/.git/worktrees"))),
            "{:?}",
            added(&w)
        );
    }

    #[test]
    fn between_commands_it_checks_at_once_then_asks_every_tick_until_none_is_left() {
        let w = world(&DIRS);
        let asked = Arc::new(Mutex::new(Vec::new()));
        let alive = {
            let (w, asked) = (Arc::clone(&w), Arc::clone(&asked));
            move || {
                let mut asked = asked.lock().unwrap();
                asked.push(lock(&w).now);
                asked.len() < 3
            }
        };
        at(&w, 1000, vec![ev(2, IN_MOVED_TO, "config")]);
        let lifetime = Lifetime::Between {
            alive: Box::new(alive),
        };
        assert_eq!(run(&w, lifetime), End::NoneLeft);
        assert_eq!(checks(&w), [ms(0), ms(1000), ms(6000)]);
        assert_eq!(*asked.lock().unwrap(), [ms(2000), ms(4000), ms(6000)]);
    }

    /// Between commands, with `at_tick` run on the world at each tick: whether a process is left.
    fn between(
        world: &Shared,
        mut at_tick: impl FnMut(&mut World, usize) -> bool + Send + 'static,
    ) -> Lifetime {
        let world = Arc::clone(world);
        let mut ticks = 0;
        Lifetime::Between {
            alive: Box::new(move || {
                ticks += 1;
                at_tick(&mut lock(&world), ticks)
            }),
        }
    }

    #[test]
    fn between_commands_a_handle_that_does_nothing_any_more_ends_the_watcher_at_the_next_tick() {
        let w = world(&DIRS);
        // The next command begins without stopping it; processes are still running.
        let lifetime = between(&w, |world, _| {
            world.dirs.clear();
            true
        });
        assert_eq!(run(&w, lifetime), End::Inert);
        assert_eq!(checks(&w), [ms(0)]);
    }

    #[test]
    fn between_commands_a_directory_that_appeared_quietly_is_watched_at_the_next_tick() {
        let w = world(&DIRS);
        let lifetime = between(&w, |world, tick| {
            world.dirs.push(PathBuf::from("/ws/.git/worktrees"));
            tick < 2
        });
        assert_eq!(run(&w, lifetime), End::NoneLeft);
        assert!(
            added(&w).contains(&(4, PathBuf::from("/ws/.git/worktrees"))),
            "{:?}",
            added(&w)
        );
    }

    #[test]
    fn a_handle_that_does_nothing_any_more_ends_the_watcher() {
        let w = world(&DIRS);
        on_check(&w, |world| world.dirs.clear());
        at(&w, 0, vec![ev(2, IN_MOVED_TO, "config")]);
        at(&w, 500, vec![ev(2, IN_MOVED_TO, "config")]);
        assert_eq!(run(&w, Lifetime::Command), End::Inert);
        assert_eq!(checks(&w), [ms(0)]);
    }

    #[test]
    fn a_wait_that_fails_ends_the_watcher() {
        let w = world(&DIRS);
        lock(&w).steps.push_back(Step::Fail);
        assert_eq!(run(&w, Lifetime::Command), End::Failed);
    }

    #[test]
    fn a_stop_ends_the_watcher() {
        let w = world(&DIRS);
        at(&w, 0, vec![ev(2, IN_MOVED_TO, "config")]);
        lock(&w).steps.push_back(Step::Stop);
        at(&w, 100, vec![ev(2, IN_MOVED_TO, "config")]);
        assert_eq!(run(&w, Lifetime::Command), End::Stopped);
        assert_eq!(checks(&w), [ms(0)]);
    }

    #[test]
    fn a_check_that_panics_ends_the_watcher_without_unwinding_out_of_it() {
        let w = world(&DIRS);
        lock(&w).panic_on_check = true;
        at(&w, 0, vec![ev(2, IN_MOVED_TO, "config")]);
        assert_eq!(run_caught(watch(&w, Lifetime::Command)), End::Panicked);
    }

    #[test]
    fn a_hook_planted_under_a_real_guard_is_moved_by_the_check_its_event_sets_off() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let ws = base.join("ws");
        std::fs::create_dir_all(ws.join(".git/hooks")).unwrap();
        std::fs::write(ws.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(ws.join(".git/config"), "[core]\n\tbare = false\n").unwrap();
        std::fs::write(ws.join(".git/hooks/pre-commit"), "exit 0\n").unwrap();
        let session = crate::guard::GuardSession::new(&base.join("quarantine"));
        let guard = session.begin(&ws, true, |_| {});
        let w = world(&[]);
        let watcher = Watch::new(
            Fake(Arc::clone(&w), Instant::now()),
            guard.watch_handle(),
            Lifetime::Command,
        );
        let hooks = added(&w)
            .into_iter()
            .find(|(_, dir)| dir.ends_with(".git/hooks"))
            .map(|(wd, _)| wd)
            .expect("the hooks directory is watched");
        std::fs::write(ws.join(".git/hooks/post-checkout"), "echo pwned\n").unwrap();
        at(&w, 0, vec![ev(hooks, IN_CREATE, "post-checkout")]);
        assert_eq!(watcher.run(), End::Stopped);
        assert!(
            !ws.join(".git/hooks/post-checkout").exists(),
            "moved by the check the event set off"
        );
        let report = guard.finish().expect("a report");
        assert!(
            report
                .message
                .contains("- .git/hooks/post-checkout: new in a protected directory; moved to "),
            "{}",
            report.message
        );
    }
}
