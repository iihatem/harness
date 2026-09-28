//! Linux sandbox backend: a Landlock ruleset (filesystem) plus seccomp-BPF
//! programs (network, mounts, namespaces) installed from `pre_exec`, in the
//! forked child, before `execve`; and around every workspace-write
//! command, the git-metadata guard (`crate::guard`) with an inotify watcher
//! (`crate::watch`, `inotify.rs`).
//!
//! ## Split between parent and child
//!
//! Everything that can allocate, open files, or otherwise take locks (the
//! Landlock ruleset with its `path_beneath` rules, and the compiled seccomp-BPF
//! programs) is built in the **parent**, by [`fs::build_ruleset_fd`],
//! [`seccomp::build_deny_filter`] and [`seccomp::build_clone3_filter`]. [`linux_sandbox_command`] hands
//! the results to [`preexec::apply`], which is the only code that runs in
//! the forked child's `pre_exec` closure. That function is restricted to
//! async-signal-safe operations: raw syscalls (`setsid`, `prctl`,
//! `landlock_restrict_self`, `seccomp`) and reads of the already-prepared
//! data. See `preexec.rs` for the full rationale.
//!
//! ## Ordering inside `pre_exec`
//!
//! 1. `setsid()` — the child becomes its own session/process-group leader,
//!    so `killpg(child_pid)` reaches grandchildren too. We deliberately do
//!    *not* also call `setpgid(0, 0)`: once `setsid()` has run, the process
//!    is already its own group leader and a subsequent `setpgid` targeting
//!    it fails with `EPERM`.
//! 2. Mark every inherited fd above stderr close-on-exec, so a writable or
//!    connectable fd cannot leak into the sandboxed program through
//!    inheritance. This runs before Landlock is restricted because it needs
//!    to open `/proc/self/fd`.
//! 3. `prctl(PR_SET_NO_NEW_PRIVS)` — required before `seccomp(2)` will
//!    install a filter, and applied before Landlock too so nothing between
//!    here and `execve` could regain privileges via a setuid/setgid binary.
//! 4. `landlock_restrict_self` on the ruleset fd built in the parent.
//! 5. Install the seccomp-BPF programs. Last, so none of the syscalls above
//!    can be filtered by them.
//!
//! ## Processes a command leaves running
//!
//! In the basic tier, [`LinuxSandbox`] makes harness a child subreaper when
//! it is created, so what a command leaves running stays among harness's
//! descendants, and its guard session asks at every check whether any such
//! process is still alive, reaping the ones that exited (`crate::procs`,
//! whose docs state the invariant every other child of harness must keep).
//! Each command's pid is registered with [`CommandGuard::started`] until its
//! guard finishes, so harness never reaps the process tokio waits for.
//!
//! ## The watcher
//!
//! While a workspace-write command runs, a watcher runs the guard's checks as
//! soon as a protected name in the workspace changes; it is stopped before
//! the guard's final checks. When the guard finishes while processes the
//! command left are running, a watcher between commands takes over for that
//! workspace, until they are gone or the next workspace-write command there
//! begins. What it does is reported with the next command in that workspace,
//! a read-only one included, without blocking it. A watcher that cannot start
//! is skipped, and the next report says so once: the checks around each
//! command still run.

mod detect;
mod fdcleanup;
mod fs;
mod inotify;
mod preexec;
mod seccomp;

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use harness_core::tool::{
    CommandGuard, CommandSandbox, GitProtection, GuardReport, SandboxedCommand,
};
use tokio::process::Command;

use crate::guard::{GitGuard, GuardSession};
use crate::procs::{self, Registration};
use crate::watch::{Lifetime, Target};
use crate::{FsAccess, SandboxPolicy, SandboxSettings};
use inotify::Watcher;
use preexec::PreparedSandbox;

pub use detect::{landlock_abi, linux_sandbox_available};
pub use inotify::watcher_failures;

/// Builds a [`tokio::process::Command`] for `program`/`args` with `policy`'s
/// Landlock + seccomp sandbox installed via `pre_exec`.
///
/// The Landlock ruleset and the seccomp-BPF programs are all compiled here,
/// in the caller's process, before the child ever exists; `pre_exec` only
/// has to hand already-prepared data to the kernel. See the [`linux`
/// module docs](self) for the full ordering rationale.
///
/// Returns `Err` for setup failures in *this* process (e.g. a seccomp rule
/// that failed to validate) **and** whenever the running kernel cannot
/// fully enforce the Landlock ABI-3 floor this crate requires, including a
/// kernel with no Landlock support at all — see [`fs::build_ruleset_fd`].
/// This function never hands back a command that merely *looks* sandboxed;
/// call [`linux_sandbox_available`] first if the caller wants to know
/// ahead of time whether that floor is met.
///
/// The child runs in a session of its own. While a [`LinuxSandbox`] reaps
/// orphans, spawn through [`CommandSandbox::prepare`] instead, whose guard
/// registers the child, or make sure no guard check runs until the child has
/// been waited for: see `crate::procs`.
pub fn linux_sandbox_command(
    policy: &SandboxPolicy,
    program: &str,
    args: &[&str],
) -> io::Result<Command> {
    let landlock_ruleset_fd = fs::build_ruleset_fd(policy)?;
    let seccomp_program = seccomp::build_deny_filter()?;
    let clone3_program = seccomp::build_clone3_filter()?;

    let prepared = PreparedSandbox {
        landlock_ruleset_fd,
        seccomp_program,
        clone3_program,
    };

    let mut command = Command::new(program);
    command.args(args);

    // A rejected `TMPDIR` (see `fs::tmpdir_override`) is excluded from the
    // Landlock ruleset above, but the child process would otherwise still
    // see the original, now-unwritable value in its environment; override
    // it to `/tmp`, which the ruleset always makes writable, so `mktemp`
    // and friends keep working inside the sandbox.
    if let Some(tmpdir) = fs::tmpdir_override(
        policy.access,
        std::env::var_os("TMPDIR").as_deref(),
        crate::roots::home_dir().as_deref(),
    ) {
        command.env("TMPDIR", tmpdir);
    }

    // SAFETY: `preexec::apply` performs only the async-signal-safe
    // operations documented on it (raw syscalls plus reads of `prepared`,
    // which was fully built above, in the parent, before this closure was
    // constructed). `prepared` is moved into the closure and so stays alive
    // — keeping the Landlock ruleset fd open — for as long as `command`
    // does, which is at least until `fork()` happens inside `spawn()`.
    unsafe {
        command.pre_exec(move || preexec::apply(&prepared));
    }

    Ok(command)
}

/// [`CommandSandbox`] backed by Landlock + seccomp, with the git-metadata
/// guard around every workspace-write command.
#[derive(Debug)]
pub struct LinuxSandbox {
    settings: SandboxSettings,
    guards: Arc<GuardSession>,
    tier: GitProtection,
    watching: Watching,
}

impl LinuxSandbox {
    /// A sandbox in the basic tier: this build has no read-only mounts yet.
    pub fn new(settings: SandboxSettings) -> Self {
        Self::with_git_protection(
            settings,
            GitProtection::Basic {
                reason: "this build of harness has no read-only mounts over git metadata".into(),
            },
        )
    }

    /// A sandbox that reports `tier` from [`CommandSandbox::git_protection`].
    /// In the basic tier, this process becomes a child subreaper, so the
    /// processes commands leave running stay its descendants: see the
    /// [module docs](self).
    pub fn with_git_protection(settings: SandboxSettings, tier: GitProtection) -> Self {
        let quarantine = settings
            .quarantine_dir
            .clone()
            .unwrap_or_else(|| std::env::temp_dir().join("harness-quarantine"));
        let guards = GuardSession::new(&quarantine);
        guards.set_survivor_probe(Arc::new(procs::look_and_reap));
        if matches!(tier, GitProtection::Basic { .. }) {
            procs::track_orphans();
        }
        LinuxSandbox {
            settings,
            guards,
            tier,
            watching: Watching::default(),
        }
    }
}

impl CommandSandbox for LinuxSandbox {
    fn name(&self) -> &'static str {
        "landlock+seccomp"
    }

    /// Callers must not call [`tokio::process::Command::process_group`] on
    /// the returned command: `pre_exec` calls `setsid()` (see the [`linux`
    /// module docs](self)), which fails with `EPERM` if something has
    /// already changed this process's process-group membership before it
    /// runs. `setsid()` alone already makes the child lead its own process
    /// group, which is what `process_group(0)` would otherwise be for.
    ///
    /// This is the command without the guard: use
    /// [`prepare`](CommandSandbox::prepare) for that.
    fn command(
        &self,
        access: FsAccess,
        workspace: &Path,
        program: &str,
        args: &[&str],
    ) -> io::Result<Command> {
        linux_sandbox_command(&self.settings.policy(access, workspace), program, args)
    }

    fn is_denial(&self, exit_code: Option<i32>, output: &str) -> bool {
        if crate::looks_like_sandbox_denial(exit_code, output, true) {
            return true;
        }
        if exit_code == Some(0) {
            return false;
        }
        // Landlock's `Refer` right denies a rename/link that would cross a
        // rule boundary with `EXDEV`, which the kernel reports through this
        // exact message (glibc's `strerror(EXDEV)` on Linux) — a denial
        // signal `looks_like_sandbox_denial`'s shared keyword list does not
        // cover, since it is Linux-specific wording for a Linux-specific
        // Landlock behavior.
        output.to_lowercase().contains("invalid cross-device link")
    }

    /// Reads the workspace's ignore rules for the rest of the session, before
    /// any command or tool can have written a `.gitignore` that hides a
    /// repository it makes.
    fn start_session(&self, workspace: &Path) {
        self.guards.prime(&canonical(workspace));
    }

    /// Stops the workspace's watcher between commands, reaps what commands
    /// left behind, starts the guard for a workspace-write command, saving
    /// every protected file so it can be restored, registers the command as
    /// about to be spawned, builds it, and starts its watcher. A read-only
    /// command has no git metadata to guard, but it runs in a session of its
    /// own all the same, so it is registered too; it cannot write to the
    /// workspace, so the watcher between commands goes on meanwhile, and
    /// what it found so far is reported with it.
    fn prepare(
        &self,
        access: FsAccess,
        workspace: &Path,
        program: &str,
        args: &[&str],
    ) -> io::Result<SandboxedCommand> {
        if access == FsAccess::ReadOnly {
            let registration = Registration::new();
            return Ok(SandboxedCommand {
                command: self.command(access, workspace, program, args)?,
                guard: Some(Box::new(Registered {
                    registration,
                    after: self.after(canonical(workspace)),
                })),
            });
        }
        let workspace = canonical(workspace);
        // What it would check, `begin` checks, and their checks must not
        // overlap.
        self.watching.stop_between(&workspace);
        // `begin` asks the probe only when an earlier command left something
        // to check, so orphans are reaped here as well.
        procs::look_and_reap();
        let guard = self.guards.begin(&workspace, true, |_| {});
        // From here until its guard finishes, the command is registered, so
        // the reaper leaves it to tokio even if it exits before its pid is
        // known.
        let registration = Registration::new();
        let command = match self.command(access, &workspace, program, args) {
            Ok(command) => command,
            Err(err) => {
                drop(registration);
                // Nothing ran; finishing records where things stand for the
                // next command, and what the checks before this one found is
                // said with the error.
                let report = self.after(workspace).finished(guard.finish());
                return Err(match report {
                    Some(report) => {
                        io::Error::new(err.kind(), format!("{err}\n{}", report.message))
                    }
                    None => err,
                });
            }
        };
        let watcher = self
            .watching
            .start(&workspace, guard.watch_handle(), Lifetime::Command);
        Ok(SandboxedCommand {
            command,
            guard: Some(Box::new(LinuxGuard {
                guard,
                registration,
                watcher,
                after: self.after(workspace),
            })),
        })
    }

    fn git_protection(&self) -> GitProtection {
        self.tier.clone()
    }
}

impl LinuxSandbox {
    fn after(&self, workspace: PathBuf) -> After {
        After {
            guards: Arc::clone(&self.guards),
            workspace,
            watching: self.watching.clone(),
        }
    }
}

fn canonical(workspace: &Path) -> PathBuf {
    std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf())
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The watchers of a sandbox: at most one per workspace between commands,
/// while processes the last command there left are running; and whether one
/// could not start, which the next report says once.
#[derive(Debug, Clone, Default)]
struct Watching(Arc<WatchingState>);

#[derive(Debug, Default)]
struct WatchingState {
    between: Mutex<BTreeMap<PathBuf, Watcher>>,
    note: Mutex<Note>,
}

/// That a watcher could not start, and why, until a report says so.
#[derive(Debug, Default)]
struct Note {
    unsaid: Option<String>,
    /// A failure was said since a watcher last started: another is not.
    said: bool,
}

impl Watching {
    /// Watches `target` in `workspace`. `None` when that cannot start, which
    /// the next report says, once until a watcher starts again.
    fn start(&self, workspace: &Path, target: impl Target, lifetime: Lifetime) -> Option<Watcher> {
        match Watcher::start(workspace, target, lifetime) {
            Ok(watcher) => {
                lock(&self.0.note).said = false;
                Some(watcher)
            }
            Err(err) => {
                let mut note = lock(&self.0.note);
                if !std::mem::replace(&mut note.said, true) {
                    note.unsaid = Some(format!(
                        "[harness could not watch git metadata as it changes ({err}), so for now it \
                         checks it only before and after each command.]\n"
                    ));
                }
                None
            }
        }
    }

    /// What the next report is to say, if anything.
    fn take_note(&self) -> Option<String> {
        lock(&self.0.note).unsaid.take()
    }

    /// Stops the watcher between commands in `workspace`, if there is one,
    /// and waits for it.
    fn stop_between(&self, workspace: &Path) {
        let watcher = lock(&self.0.between).remove(workspace);
        if let Some(watcher) = watcher {
            watcher.stop();
        }
    }

    /// Watches `workspace` until its next command while processes the last
    /// one left are running, if the guard says any are. One that cannot start
    /// is skipped: the checks before the next command still run.
    fn start_between(&self, guards: &Arc<GuardSession>, workspace: &Path) {
        let Some(handle) = guards.between_commands(workspace) else {
            return;
        };
        let lifetime = Lifetime::Between {
            alive: Box::new(procs::look_and_reap),
        };
        let Some(watcher) = self.start(workspace, handle, lifetime) else {
            return;
        };
        let done: Vec<Watcher> = {
            let mut watchers = lock(&self.0.between);
            let ended: Vec<PathBuf> = watchers
                .iter()
                .filter(|(_, watcher)| watcher.ended())
                .map(|(path, _)| path.clone())
                .collect();
            let mut done: Vec<Watcher> = ended
                .iter()
                .filter_map(|path| watchers.remove(path))
                .collect();
            done.extend(watchers.insert(workspace.to_path_buf(), watcher));
            done
        };
        // Joined once the lock is released.
        drop(done);
    }
}

/// What a command's guard does once it has finished.
struct After {
    guards: Arc<GuardSession>,
    workspace: PathBuf,
    watching: Watching,
}

impl After {
    /// `report`, the guard's, with whatever else is to be said; then, while
    /// processes the command left are running, the watcher between commands
    /// starts.
    fn finished(&self, report: Option<GuardReport>) -> Option<GuardReport> {
        let report = with_note(report, self.watching.take_note());
        self.watching.start_between(&self.guards, &self.workspace);
        report
    }
}

/// `report` with `note` added, which blocks nothing.
fn with_note(report: Option<GuardReport>, note: Option<String>) -> Option<GuardReport> {
    let Some(note) = note else {
        return report;
    };
    Some(match report {
        Some(report) => GuardReport {
            message: format!("{}{note}", report.message),
            ..report
        },
        None => GuardReport {
            message: note,
            blocked: false,
        },
    })
}

/// A read-only command's place in the registry of the processes harness
/// waits for. It has nothing to check, but what the watcher between commands
/// found so far is reported with it.
struct Registered {
    registration: Registration,
    after: After,
}

impl CommandGuard for Registered {
    fn started(&mut self, pid: u32) {
        self.registration.started(pid);
    }

    fn finish(self: Box<Self>) -> Option<GuardReport> {
        let Registered {
            registration,
            after,
        } = *self;
        drop(registration);
        let found = after.guards.found_between(&after.workspace);
        with_note(found, after.watching.take_note())
    }
}

/// The guard for one command, its place in the registry of the processes
/// harness waits for, and its watcher.
struct LinuxGuard {
    guard: GitGuard,
    registration: Registration,
    watcher: Option<Watcher>,
    after: After,
}

impl CommandGuard for LinuxGuard {
    fn started(&mut self, pid: u32) {
        self.registration.started(pid);
    }

    /// The command has been waited for: its watcher stops, so none of its
    /// checks runs once the final ones start; it is no longer registered; and
    /// the guard's checks, which ask the survivor probe, reap what it left.
    ///
    /// While the probe says processes it left are alive,
    /// [`GuardSession::between_commands`] hands a watcher between commands
    /// the checks to run until the next command begins; each asks the probe
    /// again, which reaps on the same tick, and so does the watcher every
    /// couple of seconds, until none is left.
    fn finish(self: Box<Self>) -> Option<GuardReport> {
        let LinuxGuard {
            guard,
            registration,
            watcher,
            after,
        } = *self;
        if let Some(watcher) = watcher {
            watcher.stop();
        }
        drop(registration);
        after.finished(guard.finish())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::tool::CommandSandbox;

    fn sandbox() -> LinuxSandbox {
        LinuxSandbox::new(crate::SandboxSettings::default())
    }

    #[test]
    fn a_watcher_that_cannot_start_is_said_once_in_the_next_report() {
        if !linux_sandbox_available() {
            eprintln!("skipping: linux sandbox unavailable");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws");
        std::fs::create_dir(&ws).unwrap();
        let sandbox = LinuxSandbox::with_git_protection(
            crate::SandboxSettings {
                quarantine_dir: Some(dir.path().join("quarantine")),
                ..crate::SandboxSettings::default()
            },
            GitProtection::Basic {
                reason: "forced by the test".into(),
            },
        );
        let finish = |fail: bool| {
            if fail {
                inotify::fail_next_start(libc::EMFILE);
            }
            let prepared = sandbox
                .prepare(FsAccess::WorkspaceWrite, &ws, "/bin/true", &[])
                .expect("prepare");
            prepared.guard.expect("a guard").finish()
        };
        let report = finish(true).expect("a note");
        assert!(!report.blocked, "{}", report.message);
        assert!(
            report
                .message
                .contains("harness could not watch git metadata as it changes"),
            "{}",
            report.message
        );
        let why = io::Error::from_raw_os_error(libc::EMFILE).to_string();
        assert!(report.message.contains(&why), "{}", report.message);
        assert_eq!(finish(true), None, "said once");
        assert_eq!(finish(false), None);
        assert!(finish(true).is_some(), "said again once one started");
    }

    #[test]
    fn cross_device_link_message_is_a_denial() {
        assert!(sandbox().is_denial(
            Some(1),
            "mv: cannot move 'a' to 'b': Invalid cross-device link\n"
        ));
    }

    #[test]
    fn success_is_never_a_denial_even_with_the_keyword() {
        assert!(!sandbox().is_denial(Some(0), "Invalid cross-device link\n"));
    }

    #[test]
    fn plain_failure_without_any_keyword_is_not_a_denial() {
        assert!(!sandbox().is_denial(Some(1), "some ordinary error\n"));
    }
}
