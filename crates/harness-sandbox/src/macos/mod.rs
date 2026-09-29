//! macOS backend: `/usr/bin/sandbox-exec` with a generated Seatbelt profile.

mod availability;
mod command;
mod profile;

use std::{io, path::Path};

use harness_core::tool::CommandSandbox;

pub use availability::seatbelt_available;
pub use command::seatbelt_command;
pub(crate) use profile::user_writable_roots;

use crate::{FsAccess, SandboxSettings, looks_like_sandbox_denial};

/// Absolute path, so a `sandbox-exec` earlier on PATH can never be picked up.
pub(crate) const SANDBOX_EXEC_PATH: &str = "/usr/bin/sandbox-exec";

/// [`CommandSandbox`] backed by Seatbelt.
#[derive(Debug)]
pub struct Seatbelt {
    settings: SandboxSettings,
}

impl Seatbelt {
    pub fn new(settings: SandboxSettings) -> Self {
        Seatbelt { settings }
    }
}

impl CommandSandbox for Seatbelt {
    fn name(&self) -> &'static str {
        "seatbelt"
    }

    fn command(
        &self,
        access: FsAccess,
        workspace: &Path,
        program: &str,
        args: &[&str],
    ) -> io::Result<tokio::process::Command> {
        let mut cmd = seatbelt_command(&self.settings.policy(access, workspace), program, args)?;
        cmd.process_group(0);
        Ok(cmd)
    }

    fn is_denial(&self, exit_code: Option<i32>, output: &str) -> bool {
        // Network-failure text points at the sandbox only when all networking is off. With
        // `allow_localhost`, a failed connection to a local server that isn't running is an
        // ordinary failure, not a denial.
        looks_like_sandbox_denial(exit_code, output, !self.settings.allow_localhost)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REFUSED: &str = "curl: (7) Failed to connect to 127.0.0.1 port 9: Connection refused\n";

    #[test]
    fn network_failure_text_is_a_denial_only_when_localhost_is_off() {
        let off = Seatbelt::new(SandboxSettings::default());
        assert!(off.is_denial(Some(7), REFUSED));

        let on = Seatbelt::new(SandboxSettings {
            allow_localhost: true,
            ..SandboxSettings::default()
        });
        assert!(!on.is_denial(Some(7), REFUSED));
        // File-write denials count either way.
        assert!(on.is_denial(Some(1), "touch: a: Operation not permitted\n"));
    }
}
