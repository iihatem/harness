//! The processes sandboxed commands leave running, for the Linux basic tier of
//! git-metadata protection (design D5, "Background processes, basic tier").
//!
//! A command can leave a process running that changes protected git metadata
//! after the command ends. In the basic tier there are no mounts to stop it,
//! so harness keeps track of such processes: it registers as a child
//! subreaper ([`track_orphans`]), so that a process whose parent exits,
//! detached (`setsid` and a double fork) or not, is reparented to harness and
//! stays visible as one of its descendants.
//!
//! A *survivor* is a live process that descends from harness, following
//! `ppid` in `/proc/<pid>/stat`, and whose session differs from harness's
//! own. Every sandboxed command starts its own session (`setsid` in
//! `pre_exec`), and what it starts inherits that session or makes a new one;
//! harness's own helpers (`git` for checkpoints, `$EDITOR`) stay in harness's
//! session, as do unsandboxed re-runs, which the bash tool starts with
//! `process_group(0)`. A process whose main thread exited while its other
//! threads run on reads as a zombie in `stat`, and counts as live.
//! [`look_and_reap`] is the guard's survivor probe
//! ([`crate::guard::GuardSession::set_survivor_probe`]). It errs toward
//! finding survivors: when a scan cannot read all of `/proc`, when passes
//! keep finding new zombies ([`look`]), and when the kernel refused to make
//! harness a subreaper ([`subreaper_active`]).
//!
//! As a subreaper, harness must reap the orphans that exit, or they stay
//! zombies. [`look_and_reap`] waits, without blocking, for each zombie child
//! of harness in another session whose pid is not managed ([`may_reap`]),
//! through a pidfd, after checking again what has the pid: the pid may have
//! been reaped by something else and given to another process since the scan
//! (see `reap`). It never waits for "any child" (`waitpid(-1)`), which would
//! take exit statuses that tokio and std wait for.
//!
//! **Invariant.** Any other child harness spawns must stay in harness's
//! session, or be registered ([`Registration`]) from before it is spawned
//! until it has been waited for. A sandboxed command's registration is
//! *pending* from `prepare` until the bash tool reports its pid
//! ([`Registration::started`]), and nothing is reaped while any is pending:
//! the command may already have exited. From then until its guard finishes,
//! its pid is *managed*, and never reaped here.
//!
//! The pure parts (parsing, the process tree, what counts as a survivor, what
//! may be reaped, the registry) are platform-neutral, and tested on every
//! host.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};

/// How many `/proc` entries one scan reads, at most. Past it, the scan is
/// incomplete, and survivors are assumed.
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
const MAX_PROCESSES: usize = 100_000;

/// How much of `/proc/<pid>/stat` is read: the fields used come first.
const STAT_BYTES: usize = 4096;

/// How many scans one look takes at most: see [`look`].
const MAX_PASSES: usize = 3;

/// What `/proc/<pid>/stat` says about one process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Proc {
    pub(crate) pid: i32,
    pub(crate) ppid: i32,
    /// The session id.
    pub(crate) sid: i32,
    /// The state letter of the thread-group leader: `R`, `S`, `D`, `Z`
    /// (zombie), `X` (dead), ...
    pub(crate) state: u8,
    /// The leader is a zombie, but other threads of the process run on: its
    /// main thread exited, and the rest of it did not.
    pub(crate) other_threads: bool,
}

impl Proc {
    /// Exited as a whole, and not yet reaped.
    fn zombie(&self) -> bool {
        self.state == b'Z' && !self.other_threads
    }

    fn live(&self) -> bool {
        !matches!(self.state, b'Z' | b'X' | b'x') || self.other_threads
    }
}

/// Parses a `/proc/<pid>/stat` line: `pid (comm) state ppid pgrp session …`.
/// The command name can hold any character, spaces and parentheses included,
/// so it ends at the last `)`: the fields after it are numbers.
pub(crate) fn parse_stat(stat: &[u8]) -> Option<Proc> {
    let open = stat.iter().position(|&b| b == b'(')?;
    let close = stat.iter().rposition(|&b| b == b')')?;
    if close < open {
        return None;
    }
    let pid = number(stat[..open].strip_suffix(b" ")?)?;
    let mut fields = stat[close + 1..].strip_prefix(b" ")?.split(|&b| b == b' ');
    let state = match fields.next()? {
        [state] => *state,
        _ => return None,
    };
    let ppid = number(fields.next()?)?;
    let _pgrp = number(fields.next()?)?;
    let sid = number(fields.next()?)?;
    Some(Proc {
        pid,
        ppid,
        sid,
        state,
        other_threads: false,
    })
}

fn number(field: &[u8]) -> Option<i32> {
    std::str::from_utf8(field.trim_ascii()).ok()?.parse().ok()
}

/// The processes below `root`, following each one's parent. Terminates
/// whatever the parents say: a scan that raced with pids being reused can
/// make them point in a circle.
pub(crate) fn descendants(procs: &[Proc], root: i32) -> BTreeSet<i32> {
    let mut children: BTreeMap<i32, Vec<i32>> = BTreeMap::new();
    for proc in procs {
        children.entry(proc.ppid).or_default().push(proc.pid);
    }
    let mut found = BTreeSet::new();
    let mut pending = vec![root];
    while let Some(parent) = pending.pop() {
        for &child in children.get(&parent).into_iter().flatten() {
            if child != root && found.insert(child) {
                pending.push(child);
            }
        }
    }
    found
}

/// The survivors among `procs`: live descendants of `me` in another session
/// than `my_sid`. See the module docs.
pub(crate) fn survivors(procs: &[Proc], me: i32, my_sid: i32) -> Vec<i32> {
    let below = descendants(procs, me);
    procs
        .iter()
        .filter(|proc| below.contains(&proc.pid) && proc.live() && proc.sid != my_sid)
        .map(|proc| proc.pid)
        .collect()
}

/// The zombies `me` may reap: its own children, in another session than
/// `my_sid`, that `registry` does not manage. None while a registered command
/// may be spawning.
pub(crate) fn zombies_to_reap(
    procs: &[Proc],
    me: i32,
    my_sid: i32,
    registry: &Registry,
) -> Vec<i32> {
    if registry.pending > 0 {
        return Vec::new();
    }
    procs
        .iter()
        .filter(|proc| may_reap(proc, me, my_sid, registry))
        .map(|proc| proc.pid)
        .collect()
}

/// Whether `me` may reap `proc`: a zombie child of it, in another session
/// than `my_sid`, whose pid `registry` does not manage. Asked of what a scan
/// found, and again of what has that pid once a pidfd holds it.
pub(crate) fn may_reap(proc: &Proc, me: i32, my_sid: i32, registry: &Registry) -> bool {
    proc.pid > 1
        && proc.ppid == me
        && proc.zombie()
        && proc.sid != my_sid
        && !registry.managed.contains_key(&proc.pid)
}

/// The processes one scan found.
#[derive(Debug)]
pub(crate) struct Scan {
    pub(crate) procs: Vec<Proc>,
    /// Every entry of the directory was looked at.
    pub(crate) complete: bool,
}

/// Reads `<root>/<pid>/stat` for each process directory in `root` (`/proc`),
/// looking at `limit` entries at most. A process that exits while the scan
/// runs, or whose stat line cannot be read, is left out. Each is read as soon
/// as it is listed, so a process that forks and exits is still likely to be
/// seen: pids mostly grow, and the listing goes up by pid.
pub(crate) fn scan(root: &Path, limit: usize) -> Scan {
    let incomplete = |procs| Scan {
        procs,
        complete: false,
    };
    let Ok(entries) = std::fs::read_dir(root) else {
        return incomplete(Vec::new());
    };
    let mut procs = Vec::new();
    for (seen, entry) in entries.enumerate() {
        if seen == limit {
            return incomplete(procs);
        }
        let Ok(entry) = entry else {
            return incomplete(procs);
        };
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|name| name.parse::<i32>().ok()) else {
            continue;
        };
        if let Some(proc) = read_proc(&entry.path(), pid) {
            procs.push(proc);
        }
    }
    Scan {
        procs,
        complete: true,
    }
}

/// What `dir` (`/proc/<pid>`) says about process `pid`: its stat line, and
/// for a zombie leader whether other threads of it run on. `None` when it
/// cannot be read, or is not about `pid`.
fn read_proc(dir: &Path, pid: i32) -> Option<Proc> {
    let mut proc = read_stat(dir).filter(|proc| proc.pid == pid)?;
    if proc.state == b'Z' {
        proc.other_threads = other_threads(dir, pid);
    }
    Some(proc)
}

/// Whether the process in `dir` (`/proc/<pid>`), whose thread-group leader
/// is a zombie, has other threads that run on: `stat` shows the leader's
/// state, so a process whose main thread exited reads as a zombie while the
/// rest of it runs. The first two entries of `task` tell. A `task` that cannot
/// be listed is taken to run on, unless it is gone: the process was reaped
/// meanwhile.
fn other_threads(dir: &Path, pid: i32) -> bool {
    let entries = match std::fs::read_dir(dir.join("task")) {
        Ok(entries) => entries,
        Err(err) => return err.kind() != std::io::ErrorKind::NotFound,
    };
    let leader = pid.to_string();
    for entry in entries.take(2) {
        match entry {
            Ok(entry) if entry.file_name() != leader.as_str() => return true,
            Ok(_) => {}
            Err(_) => return true,
        }
    }
    false
}

/// The first [`STAT_BYTES`] of `<dir>/stat`, parsed.
fn read_stat(dir: &Path) -> Option<Proc> {
    let mut file = std::fs::File::open(dir.join("stat")).ok()?;
    let mut buf = [0u8; STAT_BYTES];
    let mut len = 0;
    while len < buf.len() {
        match file.read(&mut buf[len..]) {
            Ok(0) => break,
            Ok(read) => len += read,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
    parse_stat(&buf[..len])
}

/// Scans, reaps what `me` may reap (`reap` says whether it did), and says
/// whether survivors exist. A scan that could not read everything assumes
/// them. While a pass reaped anything, it scans again, up to [`MAX_PASSES`]
/// in all: a chain of processes that fork and exit to slip past a scan (pids
/// wrap, so a child may be listed before its parent) leaves zombies that
/// only harness reaps. It is either seen by a later pass, or keeps the passes
/// reaping until they run out, and then survivors are assumed.
pub(crate) fn look(
    me: i32,
    my_sid: i32,
    registry: &Registry,
    mut scan: impl FnMut() -> Scan,
    mut reap: impl FnMut(i32) -> bool,
) -> bool {
    for _ in 0..MAX_PASSES {
        let found = scan();
        let mut reaped = false;
        for pid in zombies_to_reap(&found.procs, me, my_sid, registry) {
            reaped |= reap(pid);
        }
        if !found.complete || !survivors(&found.procs, me, my_sid).is_empty() {
            return true;
        }
        if !reaped {
            return false;
        }
    }
    true
}

/// Whether this process became a child subreaper.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Subreaper {
    /// Nothing asked it to (the full tier).
    NotAsked,
    Active,
    /// The kernel refused: orphans go elsewhere, and detached processes a
    /// command leaves running cannot be seen.
    Refused,
}

/// The survivor probe's answer, given what [`look`] found: with the
/// subreaper refused, survivors are always assumed.
fn verdict(looked: bool, subreaper: Subreaper) -> bool {
    looked || subreaper == Subreaper::Refused
}

/// The commands harness waits for: see the module docs.
#[derive(Debug, Default)]
pub(crate) struct Registry {
    /// Pids that tokio waits for, with how many registrations hold each.
    managed: BTreeMap<i32, usize>,
    /// Registrations whose command may be spawning.
    pending: usize,
}

/// The registry of this process.
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
static REGISTRY: Mutex<Registry> = Mutex::new(Registry {
    managed: BTreeMap::new(),
    pending: 0,
});

/// One command's place in the registry: pending from its creation, before
/// the command is spawned, until [`started`](Self::started); then its pid is
/// managed until the registration is dropped, once the command has been
/// waited for.
#[derive(Debug)]
pub(crate) struct Registration {
    registry: &'static Mutex<Registry>,
    pending: bool,
    pid: Option<i32>,
}

impl Registration {
    /// A pending registration in this process's registry.
    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    pub(crate) fn new() -> Registration {
        Registration::new_in(&REGISTRY)
    }

    fn new_in(registry: &'static Mutex<Registry>) -> Registration {
        lock(registry).pending += 1;
        Registration {
            registry,
            pending: true,
            pid: None,
        }
    }

    /// The command was spawned as `pid`: it is managed from now on.
    pub(crate) fn started(&mut self, pid: u32) {
        // Not a pid Linux gives out: it stays pending, so nothing is reaped
        // until the registration goes.
        let Ok(pid) = i32::try_from(pid) else {
            return;
        };
        let mut registry = lock(self.registry);
        *registry.managed.entry(pid).or_default() += 1;
        if let Some(earlier) = self.pid.replace(pid) {
            release(&mut registry, earlier);
        }
        if std::mem::take(&mut self.pending) {
            registry.pending -= 1;
        }
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        let mut registry = lock(self.registry);
        if self.pending {
            registry.pending -= 1;
        }
        if let Some(pid) = self.pid {
            release(&mut registry, pid);
        }
    }
}

fn release(registry: &mut Registry, pid: i32) {
    if let Entry::Occupied(mut held) = registry.managed.entry(pid) {
        *held.get_mut() -= 1;
        if *held.get() == 0 {
            held.remove();
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Whether [`track_orphans`] made this process a child subreaper: 0 not
/// asked, 1 active, 2 refused.
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
static SUBREAPER: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
fn subreaper() -> Subreaper {
    match SUBREAPER.load(std::sync::atomic::Ordering::Acquire) {
        1 => Subreaper::Active,
        2 => Subreaper::Refused,
        _ => Subreaper::NotAsked,
    }
}

/// Whether this process is a child subreaper, so that processes sandboxed
/// commands leave running stay its descendants, which the Linux basic tier
/// relies on. `false` until a basic-tier [`crate::LinuxSandbox`] asks for it,
/// and when the kernel refused; then survivors are always assumed, and
/// protected files are restored before every command. For `harness sandbox
/// doctor`.
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
pub fn subreaper_active() -> bool {
    subreaper() == Subreaper::Active
}

/// Makes harness a child subreaper, once: from then on, orphans of the
/// processes it starts are reparented to it rather than to init. Kernels
/// since 3.4 support it, and the sandbox needs 6.2. A refusal is recorded:
/// see [`subreaper_active`].
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
pub(crate) fn track_orphans() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // SAFETY: `prctl(PR_SET_CHILD_SUBREAPER, 1)` takes only integers and
        // changes only an attribute of this process.
        let rc = unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) };
        let state = if rc == 0 { 1 } else { 2 };
        SUBREAPER.store(state, std::sync::atomic::Ordering::Release);
    });
}

/// Reaps the zombies harness may reap, then says whether survivors exist:
/// the guard's survivor probe. Up to [`MAX_PASSES`] bounded scans of `/proc`
/// ([`look`]); a scan that could not read it all, or a refused subreaper,
/// assumes survivors.
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
pub(crate) fn look_and_reap() -> bool {
    let me = i32::try_from(std::process::id()).unwrap_or(i32::MAX);
    // SAFETY: `getsid(0)` asks for this process's own session id.
    let my_sid = unsafe { libc::getsid(0) };
    if my_sid < 0 {
        // Without its own session id, harness cannot tell its helpers from
        // survivors: it reaps nothing and assumes survivors.
        return true;
    }
    // Held while scanning and reaping, so no command is spawned meanwhile
    // that a scan might take for an orphan.
    let registry = lock(&REGISTRY);
    let looked = look(
        me,
        my_sid,
        &registry,
        || scan(Path::new("/proc"), MAX_PROCESSES),
        |pid| reap(pid, me, my_sid, &registry),
    );
    drop(registry);
    verdict(looked, subreaper())
}

/// Whether `pidfd_open` failing with `err` means pidfds are refused here (a
/// seccomp profile, say), rather than that the process is gone.
pub(crate) fn pidfds_refused(err: &std::io::Error) -> bool {
    matches!(err.raw_os_error(), Some(libc::ENOSYS | libc::EPERM))
}

/// Whether `pidfd_open` was refused in this process: orphans were then
/// reaped by pid, see `reap`.
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
static PIDFDS_REFUSED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether this process reaps the orphans of sandboxed commands through
/// pidfds, which no pid reused meanwhile can mislead: `false` once the kernel
/// refused one (a seccomp profile, say), and it waits by pid instead. For
/// `harness sandbox doctor`.
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
pub fn reaps_through_pidfds() -> bool {
    !PIDFDS_REFUSED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Reaps `pid`, a zombie the scan found that `me` may reap, unless the pid
/// has changed hands since: whether it did.
///
/// Something else can reap it meanwhile (the bash tool waits for what is left
/// of a killed command's process group), and the pid can then go to another
/// process, which `waitpid(pid)` would take the exit status of. So the wait
/// goes through a pidfd: whatever it was opened for is the only process it
/// can wait for, and only until that process is reaped, even once its pid is
/// someone else's. What has the pid is read after the pidfd is opened, and if
/// the process the pidfd is for is still there to be waited for, it had the
/// pid all along, so what was read is about it. Kernels since 5.4 have both
/// calls, and the sandbox needs 6.2.
///
/// Where pidfds are refused, it waits by pid after the same check again,
/// which leaves the window between the check and the wait that pidfds close.
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
fn reap(pid: i32, me: i32, my_sid: i32, registry: &Registry) -> bool {
    reap_with(pid, me, my_sid, registry, pidfd_open)
}

/// A pidfd for `pid`.
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
fn pidfd_open(pid: i32) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::{FromRawFd, OwnedFd};

    // SAFETY: `pidfd_open(2)` takes a pid and flags, and returns a new
    // descriptor or -1.
    let opened = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    match i32::try_from(opened) {
        Ok(fd) if fd >= 0 => {
            // SAFETY: `fd` was just created, and nothing else owns it.
            Ok(unsafe { OwnedFd::from_raw_fd(fd) })
        }
        _ => Err(std::io::Error::last_os_error()),
    }
}

/// [`reap`], with `open` giving the pidfd.
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
fn reap_with(
    pid: i32,
    me: i32,
    my_sid: i32,
    registry: &Registry,
    open: impl FnOnce(i32) -> std::io::Result<std::os::fd::OwnedFd>,
) -> bool {
    use std::os::fd::AsRawFd;

    let pidfd = match open(pid) {
        Ok(pidfd) => Some(pidfd),
        Err(err) if pidfds_refused(&err) => {
            PIDFDS_REFUSED.store(true, std::sync::atomic::Ordering::Relaxed);
            None
        }
        // Gone, most likely: reaped meanwhile.
        Err(_) => return false,
    };
    let Some(now) = read_proc(&Path::new("/proc").join(pid.to_string()), pid) else {
        return false;
    };
    if !may_reap(&now, me, my_sid, registry) {
        return false;
    }
    let Some(pidfd) = pidfd else {
        let mut status = 0;
        // SAFETY: waits, without blocking, for this one zombie child, which
        // nothing else waits for while `may_reap` allows it (see the module
        // docs).
        return unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) } == pid;
    };
    let Ok(id) = libc::id_t::try_from(pidfd.as_raw_fd()) else {
        return false;
    };
    // SAFETY: an all-zero `siginfo_t` is valid; `waitid` fills it in.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: waits, without blocking, for the one process `pidfd` is for,
    // which nothing else waits for while `may_reap` allows it (see the module
    // docs); `info` outlives the call.
    let waited =
        unsafe { libc::waitid(libc::P_PIDFD, id, &mut info, libc::WEXITED | libc::WNOHANG) };
    // SAFETY: `waitid` filled `info` in, or left it zeroed (no child had
    // exited), and `si_pid` reads the pid field either way.
    waited == 0 && unsafe { info.si_pid() } == pid
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(pid: i32, comm: &str, state: char, ppid: i32, sid: i32) -> Vec<u8> {
        format!("{pid} ({comm}) {state} {ppid} {pid} {sid} 34816 1234 4194560 0 0 0 0 0 0 20 0 1 0 12345 1024 256\n")
            .into_bytes()
    }

    fn p(pid: i32, ppid: i32, sid: i32, state: u8) -> Proc {
        Proc {
            pid,
            ppid,
            sid,
            state,
            other_threads: false,
        }
    }

    /// A thread-group leader that exited while other threads of its process run on.
    fn leader_gone(pid: i32, ppid: i32, sid: i32) -> Proc {
        Proc {
            other_threads: true,
            ..p(pid, ppid, sid, b'Z')
        }
    }

    #[test]
    fn a_stat_line_gives_the_pid_state_parent_and_session() {
        assert_eq!(
            parse_stat(&line(4321, "bash", 'S', 17, 99)),
            Some(p(4321, 17, 99, b'S'))
        );
        assert_eq!(
            parse_stat(&line(7, "sleep", 'Z', 1, 7)),
            Some(p(7, 1, 7, b'Z'))
        );
    }

    #[test]
    fn a_command_name_with_spaces_and_parentheses_is_skipped_whole() {
        for comm in [
            "Web Content",
            "a b c",
            "x)",
            "(x",
            ") Z 1 1 1 (",
            "evil) R 1 1 1 (",
            "()()",
            "tab\there",
            "new\nline",
        ] {
            assert_eq!(
                parse_stat(&line(500, comm, 'S', 42, 43)),
                Some(p(500, 42, 43, b'S')),
                "{comm:?}"
            );
        }
    }

    #[test]
    fn a_stat_line_that_is_cut_short_or_garbled_is_nothing() {
        for bad in [
            &b""[..],
            b"12",
            b"12 (sh",
            b"12 (sh) S",
            b"12 (sh) S 1 2",
            b"x (sh) S 1 2 3 4",
            b"12 (sh) S one 2 3 4",
            b"12 (sh) S 1 2 three 4",
            b"12 (sh)S 1 2 3 4",
            b"(sh) S 1 2 3 4",
            b"\xff\xfe (sh) S 1 2 3 4",
        ] {
            assert_eq!(parse_stat(bad), None, "{:?}", String::from_utf8_lossy(bad));
        }
    }

    #[test]
    fn descendants_follow_parents_down_from_the_root_only() {
        let procs = [
            p(1, 0, 1, b'S'),
            p(100, 1, 100, b'S'), // harness
            p(200, 100, 200, b'S'),
            p(201, 200, 200, b'S'),
            p(202, 201, 202, b'Z'),
            p(300, 100, 100, b'S'),
            p(400, 1, 400, b'S'), // unrelated
            p(401, 400, 400, b'S'),
        ];
        assert_eq!(
            descendants(&procs, 100),
            BTreeSet::from([200, 201, 202, 300])
        );
        assert_eq!(descendants(&procs, 999), BTreeSet::new());
    }

    #[test]
    fn a_parent_loop_from_an_inconsistent_scan_ends() {
        // Pids reused mid-scan can make parents point in a circle, the root's included.
        let procs = [
            p(100, 400, 100, b'S'),
            p(400, 100, 400, b'S'),
            p(200, 300, 200, b'S'),
            p(300, 200, 200, b'S'),
        ];
        assert_eq!(descendants(&procs, 100), BTreeSet::from([400]));
        assert_eq!(descendants(&procs, 200), BTreeSet::from([300]));
    }

    #[test]
    fn survivors_are_live_descendants_in_another_session() {
        let me = 100;
        let my_sid = 50;
        let procs = [
            p(me, 49, my_sid, b'S'),
            // A command's background job, its shell gone: reparented to harness.
            p(200, me, 190, b'S'),
            // A detached one: its own session, reparented to harness.
            p(210, me, 210, b'R'),
            // Its child, still in its session.
            p(211, 210, 210, b'D'),
            // harness's own helper and its child, in harness's session.
            p(300, me, my_sid, b'S'),
            p(301, 300, my_sid, b'S'),
            // What a helper started in a session of its own descends from harness too.
            p(302, 301, 302, b'S'),
            // Exited, not yet reaped.
            p(400, me, 400, b'Z'),
            p(401, me, 401, b'X'),
            // Not harness's.
            p(500, 1, 500, b'S'),
        ];
        let mut found = survivors(&procs, me, my_sid);
        found.sort_unstable();
        assert_eq!(found, [200, 210, 211, 302]);
        assert!(
            survivors(
                &[p(me, 49, my_sid, b'S'), p(300, me, my_sid, b'S')],
                me,
                my_sid
            )
            .is_empty()
        );
    }

    fn registry(managed: &[i32], pending: usize) -> Registry {
        Registry {
            managed: managed.iter().map(|pid| (*pid, 1)).collect(),
            pending,
        }
    }

    #[test]
    fn only_unmanaged_zombie_children_in_another_session_are_reaped() {
        let me = 100;
        let my_sid = 50;
        let procs = [
            p(me, 49, my_sid, b'S'),
            p(200, me, 200, b'Z'),    // an orphan that exited: reaped
            p(201, me, 190, b'Z'),    // one from a command's session: reaped
            p(202, me, 202, b'Z'),    // the command tokio waits for: managed
            p(203, me, my_sid, b'Z'), // harness's own child, waited for by std or tokio
            p(204, me, 204, b'S'),    // alive
            p(205, 204, 204, b'Z'),   // not harness's child
            p(206, 1, 206, b'Z'),     // not harness's at all
        ];
        let mut reaped = zombies_to_reap(&procs, me, my_sid, &registry(&[202], 0));
        reaped.sort_unstable();
        assert_eq!(reaped, [200, 201]);
    }

    #[test]
    fn a_pid_that_changed_hands_since_the_scan_is_not_reaped() {
        let me = 100;
        let my_sid = 50;
        let free = registry(&[], 0);
        // Still the orphan the scan found.
        assert!(may_reap(&p(200, me, 200, b'Z'), me, my_sid, &free));
        // Reaped meanwhile, and the pid given to another process:
        for now in [
            p(200, me, my_sid, b'Z'), // harness's own helper, which std or tokio waits for
            p(200, me, 200, b'S'),    // one still running
            p(200, 300, 200, b'Z'),   // another process's child
            leader_gone(200, me, 200),
        ] {
            assert!(!may_reap(&now, me, my_sid, &free), "{now:?}");
        }
        // A command harness spawned and waits for.
        assert!(!may_reap(
            &p(200, me, 200, b'Z'),
            me,
            my_sid,
            &registry(&[200], 0)
        ));
        assert!(!may_reap(&p(1, me, 1, b'Z'), me, my_sid, &free));
    }

    /// Spawns `sh -c 'exit 3'` in a session of its own and waits until it is a zombie.
    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    fn zombie_in_its_own_session() -> std::process::Child {
        use std::os::unix::process::CommandExt;
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-c", "exit 3"]);
        // SAFETY: `setsid` is async-signal-safe, and nothing else runs in the child.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().expect("spawn sh");
        let pid = i32::try_from(child.id()).unwrap();
        let dir = Path::new("/proc").join(pid.to_string());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while read_stat(&dir).is_none_or(|proc| proc.state != b'Z') {
            assert!(std::time::Instant::now() < deadline, "sh did not exit");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        child
    }

    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    #[test]
    fn the_reaper_leaves_a_managed_zombie_to_its_waiter_and_reaps_an_orphan_through_a_pidfd() {
        let mut managed = zombie_in_its_own_session();
        let mut registration = Registration::new();
        registration.started(managed.id());
        look_and_reap();
        let dir = Path::new("/proc").join(managed.id().to_string());
        assert_eq!(
            read_stat(&dir).map(|proc| proc.state),
            Some(b'Z'),
            "the reaper took a zombie harness waits for"
        );
        assert_eq!(managed.wait().expect("its exit status").code(), Some(3));
        drop(registration);

        let mut orphan = zombie_in_its_own_session();
        let dir = Path::new("/proc").join(orphan.id().to_string());
        look_and_reap();
        assert_eq!(read_stat(&dir), None, "the orphan was not reaped");
        assert!(orphan.try_wait().is_err(), "nothing is left to wait for");
    }

    #[test]
    fn only_enosys_and_eperm_mean_pidfds_are_refused() {
        for errno in [libc::ENOSYS, libc::EPERM] {
            assert!(pidfds_refused(&std::io::Error::from_raw_os_error(errno)));
        }
        for errno in [libc::ESRCH, libc::EINVAL, libc::EMFILE] {
            assert!(!pidfds_refused(&std::io::Error::from_raw_os_error(errno)));
        }
    }

    /// This process's pid and session.
    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    fn me() -> (i32, i32) {
        // SAFETY: `getsid(0)` asks for this process's own session id.
        let sid = unsafe { libc::getsid(0) };
        (i32::try_from(std::process::id()).unwrap(), sid)
    }

    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    #[test]
    fn a_zombie_waited_for_already_is_not_reaped_again() {
        let mut child = zombie_in_its_own_session();
        let pid = i32::try_from(child.id()).unwrap();
        assert_eq!(child.wait().expect("its exit status").code(), Some(3));
        let (me, my_sid) = me();
        assert!(!reap(pid, me, my_sid, &Registry::default()));
    }

    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    #[test]
    fn where_pidfds_are_refused_an_orphan_is_reaped_by_its_pid() {
        let (me, my_sid) = me();
        let mut gone = zombie_in_its_own_session();
        let pid = i32::try_from(gone.id()).unwrap();
        let no_such_process = |_| Err(std::io::Error::from_raw_os_error(libc::ESRCH));
        assert!(!reap_with(
            pid,
            me,
            my_sid,
            &Registry::default(),
            no_such_process
        ));
        assert!(!pidfds_refused(&std::io::Error::from_raw_os_error(
            libc::ESRCH
        )));
        let refused = |_| Err(std::io::Error::from_raw_os_error(libc::ENOSYS));
        assert!(reap_with(pid, me, my_sid, &Registry::default(), refused));
        assert!(gone.try_wait().is_err(), "nothing is left to wait for");
        assert!(!reaps_through_pidfds());
        // Waiting by pid still leaves what harness waits for alone.
        let mut managed = zombie_in_its_own_session();
        let pid = i32::try_from(managed.id()).unwrap();
        let registry = registry(&[pid], 0);
        let refused = |_| Err(std::io::Error::from_raw_os_error(libc::EPERM));
        assert!(!reap_with(pid, me, my_sid, &registry, refused));
        assert_eq!(managed.wait().expect("its exit status").code(), Some(3));
    }

    #[test]
    fn nothing_is_reaped_while_a_command_may_be_spawning() {
        let procs = [p(100, 49, 50, b'S'), p(200, 100, 200, b'Z')];
        assert!(zombies_to_reap(&procs, 100, 50, &registry(&[], 1)).is_empty());
        assert_eq!(zombies_to_reap(&procs, 100, 50, &registry(&[], 0)), [200]);
    }

    fn leaked() -> &'static Mutex<Registry> {
        Box::leak(Box::new(Mutex::new(Registry::default())))
    }

    fn snapshot(registry: &Mutex<Registry>) -> (Vec<(i32, usize)>, usize) {
        let registry = registry.lock().unwrap();
        (
            registry.managed.iter().map(|(k, v)| (*k, *v)).collect(),
            registry.pending,
        )
    }

    #[test]
    fn a_command_is_pending_until_it_starts_and_managed_until_its_guard_goes() {
        let registry = leaked();
        let mut registration = Registration::new_in(registry);
        assert_eq!(snapshot(registry), (vec![], 1));
        registration.started(4242);
        assert_eq!(snapshot(registry), (vec![(4242, 1)], 0));
        drop(registration);
        assert_eq!(snapshot(registry), (vec![], 0));
    }

    #[test]
    fn a_command_that_never_starts_stops_being_pending_when_its_guard_goes() {
        let registry = leaked();
        let registration = Registration::new_in(registry);
        assert_eq!(snapshot(registry), (vec![], 1));
        drop(registration);
        assert_eq!(snapshot(registry), (vec![], 0));
    }

    #[test]
    fn a_pid_managed_twice_stays_managed_until_both_are_done() {
        let registry = leaked();
        let mut first = Registration::new_in(registry);
        let mut second = Registration::new_in(registry);
        first.started(7);
        second.started(7);
        assert_eq!(snapshot(registry), (vec![(7, 2)], 0));
        drop(first);
        assert_eq!(snapshot(registry), (vec![(7, 1)], 0));
        drop(second);
        assert_eq!(snapshot(registry), (vec![], 0));
    }

    #[test]
    fn started_again_moves_the_registration_to_the_new_pid() {
        let registry = leaked();
        let mut registration = Registration::new_in(registry);
        registration.started(7);
        registration.started(8);
        assert_eq!(snapshot(registry), (vec![(8, 1)], 0));
        drop(registration);
        assert_eq!(snapshot(registry), (vec![], 0));
    }

    /// A stand-in for `/proc`.
    fn fake_proc(entries: &[(&str, Option<&[u8]>)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, stat) in entries {
            let entry = dir.path().join(name);
            std::fs::create_dir(&entry).unwrap();
            if let Some(stat) = stat {
                std::fs::write(entry.join("stat"), stat).unwrap();
            }
        }
        dir
    }

    #[test]
    fn a_scan_reads_every_process_and_skips_what_vanished_or_is_garbled() {
        let bash = line(10, "ba sh)", 'S', 1, 10);
        let zombie = line(11, "sleep", 'Z', 10, 10);
        let dir = fake_proc(&[
            ("10", Some(&bash)),
            ("11", Some(&zombie)),
            ("12", None),             // exited while the scan ran
            ("13", Some(b"garbage")), // not a stat line
            ("self", Some(&bash)),    // not a process directory
            ("sys", None),
        ]);
        let found = scan(dir.path(), 100);
        assert!(found.complete);
        let mut procs = found.procs;
        procs.sort_by_key(|proc| proc.pid);
        assert_eq!(procs, [p(10, 1, 10, b'S'), p(11, 10, 10, b'Z')]);
    }

    #[test]
    fn a_scan_stops_at_its_limit_and_says_it_is_incomplete() {
        let lines: Vec<(String, Vec<u8>)> = (1..=20)
            .map(|pid| (pid.to_string(), line(pid, "sh", 'S', 1, 1)))
            .collect();
        let entries: Vec<(&str, Option<&[u8]>)> = lines
            .iter()
            .map(|(name, stat)| (name.as_str(), Some(stat.as_slice())))
            .collect();
        let dir = fake_proc(&entries);
        let full = scan(dir.path(), 20);
        assert!(full.complete);
        assert_eq!(full.procs.len(), 20);
        let cut = scan(dir.path(), 5);
        assert!(!cut.complete);
        assert!(cut.procs.len() <= 5, "{}", cut.procs.len());
    }

    #[test]
    fn a_scan_of_nothing_readable_is_incomplete() {
        let dir = tempfile::tempdir().unwrap();
        let found = scan(&dir.path().join("missing"), 100);
        assert!(!found.complete);
        assert!(found.procs.is_empty());
    }

    #[test]
    fn a_zombie_leader_whose_threads_run_on_is_a_survivor_and_is_not_reaped() {
        let me = 100;
        let my_sid = 50;
        let procs = [
            p(me, 49, my_sid, b'S'),
            leader_gone(200, me, 200),
            leader_gone(300, me, my_sid),
        ];
        assert_eq!(survivors(&procs, me, my_sid), [200]);
        assert!(zombies_to_reap(&procs, me, my_sid, &registry(&[], 0)).is_empty());
    }

    /// A stand-in for `/proc` whose entries have `task` directories: `tasks` lists the thread
    /// ids of each, and `None` makes `task` a file, which cannot be listed.
    fn fake_proc_with_tasks(entries: &[(i32, char, Option<&[i32]>)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (pid, state, tasks) in entries {
            let entry = dir.path().join(pid.to_string());
            std::fs::create_dir(&entry).unwrap();
            std::fs::write(entry.join("stat"), line(*pid, "t", *state, 1, *pid)).unwrap();
            match tasks {
                Some(tasks) => {
                    std::fs::create_dir(entry.join("task")).unwrap();
                    for tid in *tasks {
                        std::fs::create_dir(entry.join("task").join(tid.to_string())).unwrap();
                    }
                }
                None => std::fs::write(entry.join("task"), "").unwrap(),
            }
        }
        dir
    }

    #[test]
    fn a_scan_looks_for_other_threads_of_a_zombie_leader() {
        let dir = fake_proc_with_tasks(&[
            (20, 'Z', Some(&[20, 21])), // its threads run on
            (21, 'Z', Some(&[19, 21])), // one with a lower id
            (30, 'Z', Some(&[30])),     // exited whole
            (40, 'Z', Some(&[])),       // being reaped
            (50, 'Z', None),            // cannot be listed: assumed to run on
            (60, 'S', Some(&[60, 61])), // alive anyway
        ]);
        let found = scan(dir.path(), 100);
        assert!(found.complete);
        let mut threads: Vec<(i32, bool)> = found
            .procs
            .iter()
            .map(|proc| (proc.pid, proc.other_threads))
            .collect();
        threads.sort_unstable();
        assert_eq!(
            threads,
            [
                (20, true),
                (21, true),
                (30, false),
                (40, false),
                (50, true),
                (60, false)
            ]
        );
    }

    #[test]
    fn a_zombie_whose_task_directory_is_gone_was_reaped_meanwhile() {
        let dir = fake_proc_with_tasks(&[(30, 'Z', Some(&[30]))]);
        std::fs::remove_dir_all(dir.path().join("30/task")).unwrap();
        let found = scan(dir.path(), 100);
        assert_eq!(found.procs.len(), 1);
        assert!(!found.procs[0].other_threads);
    }

    /// Runs [`look`] over `scans`, one per pass, reaping every pid it is given. What it
    /// decided, how many scans it took, and what it reaped.
    fn look_over(scans: Vec<Scan>) -> (bool, usize, Vec<i32>) {
        let mut scans = scans.into_iter();
        let mut taken = 0;
        let mut reaped = Vec::new();
        let verdict = look(
            100,
            50,
            &registry(&[], 0),
            || {
                taken += 1;
                scans.next().expect("no more scans than prepared")
            },
            |pid| {
                reaped.push(pid);
                true
            },
        );
        (verdict, taken, reaped)
    }

    fn pass(procs: &[Proc]) -> Scan {
        Scan {
            procs: procs.to_vec(),
            complete: true,
        }
    }

    const ME: Proc = Proc {
        pid: 100,
        ppid: 49,
        sid: 50,
        state: b'S',
        other_threads: false,
    };

    #[test]
    fn a_pass_that_reaps_nothing_and_sees_no_survivor_ends_the_look() {
        assert_eq!(look_over(vec![pass(&[ME])]), (false, 1, vec![]));
    }

    #[test]
    fn a_pass_that_reaped_is_followed_by_another() {
        // An orphan exited: reaped, then nothing else is there.
        assert_eq!(
            look_over(vec![pass(&[ME, p(200, 100, 200, b'Z')]), pass(&[ME])]),
            (false, 2, vec![200])
        );
        // The next pass finds what the first one missed.
        assert_eq!(
            look_over(vec![
                pass(&[ME, p(200, 100, 200, b'Z')]),
                pass(&[ME, p(300, 100, 200, b'S')]),
            ]),
            (true, 2, vec![200])
        );
    }

    #[test]
    fn passes_that_keep_reaping_run_out_and_survivors_are_assumed() {
        // A chain of processes that fork and exit, each seen only once it is a zombie.
        let (verdict, taken, reaped) = look_over(vec![
            pass(&[ME, p(200, 100, 200, b'Z')]),
            pass(&[ME, p(201, 100, 200, b'Z')]),
            pass(&[ME, p(202, 100, 200, b'Z')]),
        ]);
        assert!(verdict);
        assert_eq!(taken, MAX_PASSES);
        assert_eq!(reaped, [200, 201, 202]);
    }

    #[test]
    fn a_survivor_or_an_incomplete_scan_ends_the_look_with_survivors() {
        assert_eq!(
            look_over(vec![pass(&[ME, p(300, 100, 300, b'S')])]),
            (true, 1, vec![])
        );
        let cut = Scan {
            procs: vec![ME, p(200, 100, 200, b'Z')],
            complete: false,
        };
        assert_eq!(look_over(vec![cut]), (true, 1, vec![200]));
    }

    #[test]
    fn a_zombie_waitpid_does_not_take_counts_as_nothing_reaped() {
        let mut scans = vec![pass(&[ME, p(200, 100, 200, b'Z')])].into_iter();
        let verdict = look(
            100,
            50,
            &registry(&[], 0),
            || scans.next().unwrap(),
            |_| false,
        );
        assert!(!verdict);
    }

    #[test]
    fn a_refused_subreaper_means_survivors_are_assumed() {
        assert!(verdict(false, Subreaper::Refused));
        assert!(!verdict(false, Subreaper::Active));
        assert!(!verdict(false, Subreaper::NotAsked));
        assert!(verdict(true, Subreaper::Active));
    }
}
