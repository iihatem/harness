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
//! `process_group(0)`. [`look_and_reap`] is the guard's survivor probe
//! ([`crate::guard::GuardSession::set_survivor_probe`]).
//!
//! As a subreaper, harness must reap the orphans that exit, or they stay
//! zombies. [`look_and_reap`] waits, with `waitpid(pid, WNOHANG)`, for each
//! zombie child of harness in another session whose pid is not managed. It
//! never calls `waitpid(-1)`, which would take exit statuses that tokio and
//! std wait for.
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

/// What `/proc/<pid>/stat` says about one process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Proc {
    pub(crate) pid: i32,
    pub(crate) ppid: i32,
    /// The session id.
    pub(crate) sid: i32,
    /// The state letter: `R`, `S`, `D`, `Z` (zombie), `X` (dead), ...
    pub(crate) state: u8,
}

impl Proc {
    fn zombie(&self) -> bool {
        self.state == b'Z'
    }

    fn live(&self) -> bool {
        !matches!(self.state, b'Z' | b'X' | b'x')
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
        .filter(|proc| {
            proc.pid > 1
                && proc.ppid == me
                && proc.zombie()
                && proc.sid != my_sid
                && !registry.managed.contains_key(&proc.pid)
        })
        .map(|proc| proc.pid)
        .collect()
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
        if let Some(proc) = read_stat(&entry.path()).filter(|proc| proc.pid == pid) {
            procs.push(proc);
        }
    }
    Scan {
        procs,
        complete: true,
    }
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

/// Makes harness a child subreaper, once: from then on, orphans of the
/// processes it starts are reparented to it rather than to init. Kernels
/// since 3.4 support it, and the sandbox needs 6.2.
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
pub(crate) fn track_orphans() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // SAFETY: `prctl(PR_SET_CHILD_SUBREAPER, 1)` takes only integers and
        // changes only an attribute of this process.
        unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) };
    });
}

/// Reaps the zombies harness may reap, then says whether survivors exist:
/// the guard's survivor probe. One bounded scan of `/proc`; a scan that could
/// not read it all assumes survivors.
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
    // that the scan might take for an orphan.
    let registry = lock(&REGISTRY);
    let found = scan(Path::new("/proc"), MAX_PROCESSES);
    for pid in zombies_to_reap(&found.procs, me, my_sid, &registry) {
        let mut status = 0;
        // SAFETY: waits, without blocking, for this one zombie child, which
        // nothing else waits for (see the module docs).
        unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
    }
    drop(registry);
    !found.complete || !survivors(&found.procs, me, my_sid).is_empty()
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
}
