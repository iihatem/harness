//! Linux sandbox backend: a Landlock ruleset (filesystem) plus seccomp-BPF
//! programs (network, mounts, namespaces) installed from `pre_exec`, in the
//! forked child, before `execve`; and around every workspace-write
//! command, the git-metadata guard (`crate::guard`).
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

mod detect;
mod fdcleanup;
mod fs;
mod preexec;
mod seccomp;

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use harness_core::tool::{
    CommandGuard, CommandSandbox, GitProtection, GuardReport, SandboxedCommand,
};
use tokio::process::Command;

use crate::guard::{GitGuard, GuardSession};
use crate::procs::{self, Registration};
use crate::{FsAccess, SandboxPolicy, SandboxSettings};
use preexec::PreparedSandbox;

pub use detect::{landlock_abi, linux_sandbox_available};

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

    /// Reaps what commands left behind, starts the guard for a
    /// workspace-write command, saving every protected file so it can be
    /// restored, registers the command as about to be spawned, then builds
    /// it. A read-only command has no git metadata to guard, but it runs in
    /// a session of its own all the same, so it is registered too.
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
                guard: Some(Box::new(Registered(registration))),
            });
        }
        let workspace = canonical(workspace);
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
                return Err(match guard.finish() {
                    Some(report) => {
                        io::Error::new(err.kind(), format!("{err}\n{}", report.message))
                    }
                    None => err,
                });
            }
        };
        Ok(SandboxedCommand {
            command,
            guard: Some(Box::new(LinuxGuard {
                guard,
                registration,
            })),
        })
    }

    fn git_protection(&self) -> GitProtection {
        self.tier.clone()
    }
}

fn canonical(workspace: &Path) -> PathBuf {
    std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf())
}

/// A read-only command's place in the registry of the processes harness
/// waits for: there is nothing to check.
struct Registered(Registration);

impl CommandGuard for Registered {
    fn started(&mut self, pid: u32) {
        self.0.started(pid);
    }

    fn finish(self: Box<Self>) -> Option<GuardReport> {
        None
    }
}

/// The guard for one command, and its place in the registry of the
/// processes harness waits for.
struct LinuxGuard {
    guard: GitGuard,
    registration: Registration,
}

impl CommandGuard for LinuxGuard {
    fn started(&mut self, pid: u32) {
        self.registration.started(pid);
    }

    /// The command has been waited for: it is no longer registered, and the
    /// guard's checks, which ask the survivor probe, reap what it left.
    ///
    /// While the probe says processes it left are alive,
    /// [`GuardSession::between_commands`] hands a file watcher the checks to
    /// run until the next command begins; each asks the probe again, which
    /// reaps on the same tick.
    fn finish(self: Box<Self>) -> Option<GuardReport> {
        let LinuxGuard {
            guard,
            registration,
        } = *self;
        drop(registration);
        guard.finish()
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
