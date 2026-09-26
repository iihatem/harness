//! Linux sandbox backend: a Landlock ruleset (filesystem) plus a
//! seccomp-BPF program (network) installed from `pre_exec`, in the
//! forked child, before `execve`.
//!
//! ## Split between parent and child
//!
//! Everything that can allocate, open files, or otherwise take locks (the
//! Landlock ruleset with its `path_beneath` rules, the compiled seccomp-BPF
//! program) is built in the **parent**, by [`fs::build_ruleset_fd`] and
//! [`seccomp::build_network_deny_filter`]. [`linux_sandbox_command`] hands
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
//! 5. Install the seccomp-BPF program. Last, so none of the syscalls above
//!    can be filtered by it.

mod detect;
mod fdcleanup;
mod fs;
mod preexec;
mod seccomp;

use std::io;

use tokio::process::Command;

use crate::SandboxPolicy;
use preexec::PreparedSandbox;

pub use detect::{landlock_abi, linux_sandbox_available};

/// Builds a [`tokio::process::Command`] for `program`/`args` with `policy`'s
/// Landlock + seccomp sandbox installed via `pre_exec`.
///
/// The Landlock ruleset and the seccomp-BPF program are both compiled here,
/// in the caller's process, before the child ever exists; `pre_exec` only
/// has to hand already-prepared data to the kernel. See the [`linux`
/// module docs](self) for the full ordering rationale.
///
/// Returns `Err` only for setup failures in *this* process (e.g. a seccomp
/// rule that failed to validate); a kernel with no Landlock support at all
/// is not an error here — [`fs::build_ruleset_fd`] returns `None` and the
/// child simply runs without filesystem restriction (network denial via
/// seccomp still applies). Call [`linux_sandbox_available`] first if the
/// caller needs to know whether filesystem restriction will actually be
/// enforced.
pub fn linux_sandbox_command(
    policy: &SandboxPolicy,
    program: &str,
    args: &[&str],
) -> io::Result<Command> {
    let landlock_ruleset_fd = fs::build_ruleset_fd(policy)?;
    let seccomp_program = seccomp::build_network_deny_filter().map_err(io::Error::other)?;

    let prepared = PreparedSandbox {
        landlock_ruleset_fd,
        seccomp_program,
    };

    let mut command = Command::new(program);
    command.args(args);

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

/// [`harness_core::tool::CommandSandbox`] backed by Landlock + seccomp.
#[derive(Debug)]
pub struct LinuxSandbox {
    settings: crate::SandboxSettings,
}

impl LinuxSandbox {
    pub fn new(settings: crate::SandboxSettings) -> Self {
        LinuxSandbox { settings }
    }
}

impl harness_core::tool::CommandSandbox for LinuxSandbox {
    fn name(&self) -> &'static str {
        "landlock+seccomp"
    }

    fn command(
        &self,
        access: crate::FsAccess,
        workspace: &std::path::Path,
        program: &str,
        args: &[&str],
    ) -> io::Result<Command> {
        // `setsid()` in `pre_exec` already makes the child lead its own process group.
        linux_sandbox_command(&self.settings.policy(access, workspace), program, args)
    }

    fn is_denial(&self, exit_code: Option<i32>, output: &str) -> bool {
        crate::looks_like_sandbox_denial(exit_code, output, true)
    }
}
