//! macOS backend: `/usr/bin/sandbox-exec` with a generated Seatbelt profile.

mod availability;
mod command;
mod profile;

use std::{io, path::Path};

use harness_core::tool::CommandSandbox;

pub use availability::seatbelt_available;
pub use command::seatbelt_command;

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
        // External networking is always off inside the sandbox, so network-failure text counts.
        looks_like_sandbox_denial(exit_code, output, true)
    }
}
