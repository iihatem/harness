//! Linux sandbox backend: a Landlock ruleset (filesystem) plus seccomp-BPF
//! programs (network, mounts, namespaces) installed from `pre_exec`, in the
//! forked child, before `execve`; in the full tier, a user and mount
//! namespace with self-binds over git metadata first (`mountns.rs`); and
//! around every workspace-write command, the git-metadata guard
//! (`crate::guard`) with an inotify watcher (`crate::watch`, `inotify.rs`).
//!
//! ## Split between parent and child
//!
//! Everything that can allocate, open files, or otherwise take locks (the
//! Landlock ruleset with its `path_beneath` rules, the compiled seccomp-BPF
//! programs, and in the full tier the mount plan and its setup pipe) is
//! built in the **parent**, by [`fs::build_ruleset_fd`],
//! [`seccomp::build_deny_filter`], [`seccomp::build_clone3_filter`] and
//! [`mountplan::plan`]. [`linux_sandbox_command`] hands the results to
//! [`preexec::apply`], which is the only code that runs in the forked
//! child's `pre_exec` closure. That function is restricted to
//! async-signal-safe operations: raw syscalls (`setsid`, `unshare`, the
//! mount calls, `prctl`, `landlock_restrict_self`, `seccomp`) and reads of
//! the already-prepared data. See `preexec.rs` for the full rationale.
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
//! 3. Full tier only: unshare a user and mount namespace, make the
//!    self-binds, then lock the securebits and drop every capability
//!    (`mountns.rs`). Before Landlock and seccomp, which refuse mount
//!    changes and `unshare`.
//! 4. `prctl(PR_SET_NO_NEW_PRIVS)` — required before `seccomp(2)` will
//!    install a filter, and applied before Landlock too so nothing between
//!    here and `execve` could regain privileges via a setuid/setgid binary.
//! 5. `landlock_restrict_self` on the ruleset fd built in the parent.
//! 6. Install the seccomp-BPF programs. Last, so none of the syscalls above
//!    can be filtered by them.
//!
//! ## The two tiers of git-metadata protection
//!
//! [`linux_git_protection`] probes once per process whether a child can set
//! up a user and mount namespace with a read-only bind (`tier.rs`), and
//! [`LinuxSandbox::new`] takes the tier it gives. In the **full** tier each
//! workspace-write command gets its own namespace, in which the gitdirs are
//! pinned and the protected entries bound read-only (`crate::mounts` says
//! what), so writes to them fail. In the **basic** tier there are no
//! mounts. In both tiers the guard saves every protected file before each
//! command, so it can undo changes after the fact, and covers what mounts
//! cannot: in the full tier, a name that does not exist yet, and an entry
//! whose bind came off because it was renamed or removed from outside the
//! command's namespace (git rewrites `.git/config` that way), which detaches
//! the bind in every other namespace.
//!
//! If a full-tier command's setup fails, the command does not run: the
//! child writes the step that failed to a pipe, which the command's guard
//! reads when it finishes, and says what happened. When the kernel or the
//! host refused a step (user namespaces blocked after the probe, a mount
//! call not permitted), the session drops to the basic tier for every later
//! command. When a path changed between the plan and the setup, or a
//! resource limit was hit, only that command is stopped, and the session
//! keeps the full tier ([`crate::mounts::Failure::drops_tier`]): nothing
//! outside a command can downgrade it.
//!
//! ## Processes a command leaves running
//!
//! In both tiers, [`LinuxSandbox`] makes harness a child subreaper when it
//! is created, so what a command leaves running stays among harness's
//! descendants, and its guard session asks at every check whether any such
//! process is still alive, reaping the ones that exited (`crate::procs`,
//! whose docs state the invariant every other child of harness must keep;
//! the tier probe's child keeps it by staying in harness's session). In the
//! full tier such a process stays in its command's namespace, under that
//! command's mounts, but a protected entry that appeared since is not
//! mounted there: so while any is alive, the guard's checks restore the
//! protected files in both tiers alike.
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
mod mountns;
mod mountplan;
mod preexec;
mod seccomp;
mod tier;

use std::collections::BTreeMap;
use std::io;
use std::os::fd::OwnedFd;
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
use mountns::MountPlan;
use mountplan::Planned;
use preexec::PreparedSandbox;

pub use detect::{landlock_abi, linux_sandbox_available};
pub use inotify::watcher_failures;
pub use tier::linux_git_protection;

/// Builds a [`tokio::process::Command`] for `program`/`args` with `policy`'s
/// Landlock + seccomp sandbox installed via `pre_exec`, without the full
/// tier's mounts or the guard (see [`LinuxSandbox`] for those).
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
    sandboxed_command(policy, program, args, None)
}

/// [`linux_sandbox_command`], plus the full tier's namespace and mounts when
/// `mounts` is set, with the write end of the pipe the child reports a
/// failed setup step to. Whoever holds that pipe's read end must keep it
/// open until the command has been spawned: a child writing to a pipe no
/// one reads is killed by `SIGPIPE`.
fn sandboxed_command(
    policy: &SandboxPolicy,
    program: &str,
    args: &[&str],
    mounts: Option<(MountPlan, Option<OwnedFd>)>,
) -> io::Result<Command> {
    let landlock_ruleset_fd = fs::build_ruleset_fd(policy)?;
    let seccomp_program = seccomp::build_deny_filter()?;
    let clone3_program = seccomp::build_clone3_filter()?;

    let prepared = PreparedSandbox {
        landlock_ruleset_fd,
        seccomp_program,
        clone3_program,
        mounts,
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
    // — keeping the Landlock ruleset fd and the setup pipe open — for as
    // long as `command` does, which is at least until `fork()` happens
    // inside `spawn()`.
    unsafe {
        command.pre_exec(move || preexec::apply(&prepared));
    }

    Ok(command)
}

/// The full tier's command: its namespace and mounts when `planned` is set,
/// with a pipe the child reports a failed setup step to, whose read end
/// comes back in the [`SetupReport`]; just Landlock and seccomp otherwise.
fn mounted_command(
    policy: &SandboxPolicy,
    planned: Option<Planned>,
    program: &str,
    args: &[&str],
) -> io::Result<(Command, Option<SetupReport>)> {
    let Some((plan, paths)) = planned else {
        return Ok((sandboxed_command(policy, program, args, None)?, None));
    };
    let (reader, writer) = mountplan::setup_pipe()?;
    let command = sandboxed_command(policy, program, args, Some((plan, Some(writer))))?;
    Ok((command, Some(SetupReport { reader, paths })))
}

/// [`CommandSandbox`] backed by Landlock + seccomp, with the git-metadata
/// guard around every workspace-write command, and git-metadata protection
/// in one of two tiers ([`GitProtection`]): read-only mounts in a user and
/// mount namespace, plus the guard (full), or the guard alone (basic). See
/// the [module docs](self).
#[derive(Debug)]
pub struct LinuxSandbox {
    settings: SandboxSettings,
    /// Where the guard's quarantine is: never walked for git metadata.
    quarantine: PathBuf,
    guards: Arc<GuardSession>,
    /// The session's tier. The full tier drops to the basic tier for good
    /// when the kernel or the host refuses a command's setup.
    tier: Arc<Mutex<GitProtection>>,
    watching: Watching,
}

impl LinuxSandbox {
    /// A sandbox in the tier this host supports ([`linux_git_protection`]).
    pub fn new(settings: SandboxSettings) -> Self {
        Self::with_git_protection(settings, linux_git_protection())
    }

    /// A sandbox in `tier`, which [`CommandSandbox::git_protection`]
    /// reports. The full tier on a host that does not support it fails its
    /// first workspace-write command, which does not run, and then drops to
    /// the basic tier. In either tier, this process becomes a child
    /// subreaper, so the processes commands leave running stay its
    /// descendants: see the [module docs](self).
    pub fn with_git_protection(settings: SandboxSettings, tier: GitProtection) -> Self {
        let quarantine = settings
            .quarantine_dir
            .clone()
            .unwrap_or_else(|| std::env::temp_dir().join("harness-quarantine"));
        let guards = GuardSession::new(&quarantine);
        guards.set_survivor_probe(Arc::new(procs::look_and_reap));
        procs::track_orphans();
        LinuxSandbox {
            settings,
            quarantine: canonical(&quarantine),
            guards,
            tier: Arc::new(Mutex::new(tier)),
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
    /// In the full tier this includes the read-only mounts, but not the
    /// guard: use [`prepare`](CommandSandbox::prepare) for that. A failed
    /// mount setup then shows only as the spawn's error, and leaves the tier
    /// as it is.
    fn command(
        &self,
        access: FsAccess,
        workspace: &Path,
        program: &str,
        args: &[&str],
    ) -> io::Result<Command> {
        if access == FsAccess::ReadOnly || self.git_protection() != GitProtection::Full {
            let policy = self.settings.policy(access, workspace);
            return sandboxed_command(&policy, program, args, None);
        }
        let workspace = canonical(workspace);
        let rules = self.guards.rules(&workspace);
        let index = crate::gitmeta::discover(&workspace, Some(&self.quarantine), &rules);
        let policy = self.settings.policy(access, &workspace);
        // No setup pipe: nothing would read it.
        let mounts = mountplan::plan(&workspace, &index).map(|(plan, _)| (plan, None));
        sandboxed_command(&policy, program, args, mounts)
    }

    fn is_denial(&self, exit_code: Option<i32>, output: &str) -> bool {
        if crate::looks_like_sandbox_denial(exit_code, output, true) {
            return true;
        }
        if exit_code == Some(0) {
            return false;
        }
        let output = output.to_lowercase();
        // Landlock's `Refer` right, and a rename or link across one of the
        // full tier's mounts, fail with `EXDEV`; a mount point (a pinned
        // gitdir, a read-only entry) cannot be renamed or removed: `EBUSY`.
        // glibc's `strerror` wording for both is Linux-specific, so the
        // shared keyword list does not cover them.
        output.contains("invalid cross-device link") || output.contains("device or resource busy")
    }

    /// Reads the workspace's ignore rules for the rest of the session, before
    /// any command or tool can have written a `.gitignore` that hides a
    /// repository it makes.
    fn start_session(&self, workspace: &Path) {
        self.guards.prime(&canonical(workspace));
    }

    /// Stops the workspace's watcher between commands, reaps what commands
    /// left behind, starts the guard for a workspace-write command,
    /// registers the command as about to be spawned, builds it, and starts
    /// its watcher. In both tiers the guard saves every protected file so
    /// it can be restored; in the full tier the command also gets mounts
    /// over what the guard's index found, and a pipe its child reports a
    /// failed setup step to. A read-only
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
        let tier = self.git_protection();
        if let GitProtection::Basic { reason } = &tier
            && self.settings.require_full_git_protection
        {
            return Err(io::Error::other(format!(
                "git metadata protection is required (sandbox.linux_git_protection = \"required\"), but the full tier is unavailable: {reason}; restart harness to have every command ask first"
            )));
        }
        let full = tier == GitProtection::Full;
        let workspace = canonical(workspace);
        // What it would check, `begin` checks, and their checks must not
        // overlap.
        self.watching.stop_between(&workspace);
        // `begin` asks the probe only when an earlier command left something
        // to check, so orphans are reaped here as well.
        procs::look_and_reap();
        // Every protected file is saved in the full tier too: a rename or an
        // unlink from outside the command's namespace detaches its bind
        // there, and a process an earlier command left keeps its own
        // namespace, where what appeared since has no mount.
        let save_all = true;
        // The plan is made from the guard's index, after the scan and before
        // the guard records which protected names exist, so the guard takes
        // the `hooks/` placeholders for existing ones.
        let mut planned = None;
        let guard = self.guards.begin(&workspace, save_all, |index| {
            if full {
                planned = mountplan::plan(&workspace, index);
            }
        });
        // From here until its guard finishes, the command is registered, so
        // the reaper leaves it to tokio even if it exits before its pid is
        // known.
        let registration = Registration::new();
        let policy = self.settings.policy(access, &workspace);
        let (command, setup) = match mounted_command(&policy, planned, program, args) {
            Ok(built) => built,
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
                setup,
                tier: Arc::clone(&self.tier),
                after: self.after(workspace),
            })),
        })
    }

    fn git_protection(&self) -> GitProtection {
        lock(&self.tier).clone()
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

/// The read end of the pipe a full-tier child reports a failed setup step
/// to, and the plan's ops' paths, for the message.
struct SetupReport {
    reader: OwnedFd,
    paths: Vec<PathBuf>,
}

impl SetupReport {
    /// What failed, if the child said a setup step did, and whether that
    /// drops the session to the basic tier. Once the command has been
    /// spawned, or failed to be, the child has written all it will.
    fn failure(&self) -> Option<(String, bool)> {
        mountplan::read_failure(&self.reader)
            .map(|failure| (failure.describe(&self.paths), failure.drops_tier()))
    }
}

/// Drops the session to the basic tier after the kernel or the host refused
/// a full-tier command's setup: from the next command on, there are no
/// mounts (`prepare` reads the tier). The guard saves every protected file,
/// and the subreaper and the survivor probe are on, in both tiers already.
fn drop_to_basic(tier: &Mutex<GitProtection>, failure: &str) {
    *lock(tier) = GitProtection::Basic {
        reason: format!("the full tier's setup failed during the session: {failure}"),
    };
}

/// The guard for one command, its place in the registry of the processes
/// harness waits for, its watcher, and in the full tier the setup pipe's
/// read end.
struct LinuxGuard {
    guard: GitGuard,
    registration: Registration,
    watcher: Option<Watcher>,
    setup: Option<SetupReport>,
    tier: Arc<Mutex<GitProtection>>,
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
    ///
    /// If the child reported a failed setup step, the command did not run,
    /// and the report says so, without blocking. When the kernel or the host
    /// refused the step, the session drops to the basic tier; otherwise (a
    /// path that changed meanwhile, a resource limit) it keeps the full tier.
    fn finish(self: Box<Self>) -> Option<GuardReport> {
        let LinuxGuard {
            guard,
            registration,
            watcher,
            setup,
            tier,
            after,
        } = *self;
        if let Some(watcher) = watcher {
            watcher.stop();
        }
        drop(registration);
        let note = setup
            .as_ref()
            .and_then(SetupReport::failure)
            .map(|(failure, drops)| {
                if drops {
                    drop_to_basic(&tier, &failure);
                    format!(
                        "[the sandbox could not set up its read-only mounts ({failure}), so this command did not run. This session now uses the basic tier, which checks git metadata after each command instead. Run the command again.]\n"
                    )
                } else {
                    format!(
                        "[the sandbox could not set up its read-only mounts ({failure}), so this command did not run: a change made while the sandbox was being set up stopped it, or a system limit was reached. Run the command again.]\n"
                    )
                }
            });
        after.finished(with_note(guard.finish(), note))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::tool::CommandSandbox;

    fn sandbox() -> LinuxSandbox {
        LinuxSandbox::with_git_protection(
            crate::SandboxSettings::default(),
            GitProtection::Basic {
                reason: "test".into(),
            },
        )
    }

    #[test]
    fn a_watcher_that_cannot_start_is_said_once_in_the_next_report() {
        let _serial = procs::serial();
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
    fn a_busy_mount_point_is_a_denial() {
        assert!(sandbox().is_denial(
            Some(1),
            "mv: cannot move '.git' to 'g': Device or resource busy\n"
        ));
    }

    #[test]
    fn plain_failure_without_any_keyword_is_not_a_denial() {
        assert!(!sandbox().is_denial(Some(1), "some ordinary error\n"));
    }

    /// A full-tier sandbox whose guard for one command has `setup`'s pipe,
    /// and the guard, in a workspace of its own.
    fn full_tier_guard(
        setup: Option<SetupReport>,
    ) -> (LinuxSandbox, Box<LinuxGuard>, [tempfile::TempDir; 2]) {
        let ws = tempfile::tempdir().unwrap();
        let quarantine = tempfile::tempdir().unwrap();
        let sandbox = LinuxSandbox::with_git_protection(
            crate::SandboxSettings {
                quarantine_dir: Some(quarantine.path().to_path_buf()),
                ..crate::SandboxSettings::default()
            },
            GitProtection::Full,
        );
        let workspace = ws.path().canonicalize().unwrap();
        let guard = sandbox.guards.begin(&workspace, false, |_| {});
        let guard = Box::new(LinuxGuard {
            guard,
            registration: Registration::new(),
            watcher: None,
            setup,
            tier: Arc::clone(&sandbox.tier),
            after: sandbox.after(workspace),
        });
        (sandbox, guard, [ws, quarantine])
    }

    /// A setup pipe holding the record the child writes when `step` fails
    /// with `errno`, for the plan's two ops.
    fn reported(step: crate::mounts::Step, errno: i32) -> SetupReport {
        let (reader, writer) = mountplan::setup_pipe().unwrap();
        let failure = crate::mounts::Failure {
            step,
            op: Some(1),
            errno,
        }
        .encode();
        // SAFETY: writes `failure` to the pipe just made, as the child would.
        let n = unsafe {
            libc::write(
                std::os::fd::AsRawFd::as_raw_fd(&writer),
                failure.as_ptr().cast(),
                failure.len(),
            )
        };
        assert_eq!(n, failure.len() as isize);
        let paths = vec![PathBuf::from("/ws/.git"), PathBuf::from("/ws/.git/config")];
        SetupReport { reader, paths }
    }

    #[test]
    fn a_refused_setup_step_drops_the_session_to_the_basic_tier_and_says_why() {
        let _serial = procs::serial();
        let setup = reported(crate::mounts::Step::UidMap, libc::EPERM);
        let (sandbox, guard, _dirs) = full_tier_guard(Some(setup));
        let report = guard.finish().expect("a report");
        assert!(!report.blocked, "{}", report.message);
        assert!(
            report.message.starts_with(
                "[the sandbox could not set up its read-only mounts (writing /proc/self/uid_map failed: "
            ),
            "{}",
            report.message
        );
        assert!(
            report
                .message
                .contains("This session now uses the basic tier"),
            "{}",
            report.message
        );
        match sandbox.git_protection() {
            GitProtection::Basic { reason } => assert!(
                reason.starts_with(
                    "the full tier's setup failed during the session: writing /proc/self/uid_map failed: "
                ),
                "{reason}"
            ),
            GitProtection::Full => panic!("the session should have dropped to the basic tier"),
        }
    }

    #[test]
    fn a_path_that_changed_during_the_setup_stops_the_command_and_keeps_the_full_tier() {
        let _serial = procs::serial();
        for (step, errno) in [
            (crate::mounts::Step::Identity, libc::ESTALE),
            (crate::mounts::Step::Open, libc::ENOENT),
            (crate::mounts::Step::MoveMount, libc::ENOSPC),
        ] {
            let (sandbox, guard, _dirs) = full_tier_guard(Some(reported(step, errno)));
            let report = guard.finish().expect("a report");
            assert!(!report.blocked, "{}", report.message);
            assert!(
                report
                    .message
                    .starts_with("[the sandbox could not set up its read-only mounts ("),
                "{}",
                report.message
            );
            assert!(
                report
                    .message
                    .contains("a change made while the sandbox was being set up stopped it"),
                "{}",
                report.message
            );
            assert!(
                report.message.ends_with("Run the command again.]\n"),
                "{}",
                report.message
            );
            assert!(!report.message.contains("basic tier"), "{}", report.message);
            assert_eq!(sandbox.git_protection(), GitProtection::Full, "{step:?}");
        }
    }

    #[test]
    fn a_setup_that_went_well_leaves_the_tier_alone() {
        let _serial = procs::serial();
        let (reader, _writer) = mountplan::setup_pipe().unwrap();
        let (sandbox, guard, _dirs) = full_tier_guard(Some(SetupReport {
            reader,
            paths: Vec::new(),
        }));
        assert_eq!(guard.finish(), None);
        assert_eq!(sandbox.git_protection(), GitProtection::Full);
    }
}
