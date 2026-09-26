use std::io;

use tokio::process::Command;

use super::SANDBOX_EXEC_PATH;
use super::profile::build_profile;
use crate::policy::SandboxPolicy;

/// Builds a `sandbox-exec` [`Command`] that runs `program args…` under the
/// given policy: `/usr/bin/sandbox-exec -p <profile> -D k=v… -- <program>
/// <args…>`.
///
/// `WORKSPACE`, `TMPDIR` (the `TMPDIR` env var, else `getconf
/// DARWIN_USER_TEMP_DIR`), `USER_CACHE_DIR` (`getconf DARWIN_USER_CACHE_DIR`)
/// and every existing `extra_writable` path are canonicalized before being
/// passed in as profile params; non-existent `extra_writable` paths are
/// silently skipped. A temp or cache dir that is `/`, `$HOME` or an ancestor
/// of `$HOME` is rejected, and that root is then left out. The returned
/// command has no `cwd`, stdio, or process group set — the caller is expected
/// to configure those before spawning.
pub fn seatbelt_command(
    policy: &SandboxPolicy,
    program: &str,
    args: &[&str],
) -> io::Result<Command> {
    let (profile, params) = build_profile(policy, program)?;

    let mut cmd = Command::new(SANDBOX_EXEC_PATH);
    cmd.arg("-p").arg(profile);
    for (key, value) in &params {
        cmd.arg("-D").arg(format!("{key}={value}"));
    }
    cmd.arg("--").arg(program).args(args);
    Ok(cmd)
}
