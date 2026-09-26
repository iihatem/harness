//! OS sandboxes for shell commands: Seatbelt (`sandbox-exec`) on macOS and Landlock + seccomp on Linux.
//! Both implement [`harness_core::tool::CommandSandbox`]; [`detect`] picks the one this host supports.

mod denial;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
mod policy;
mod roots;

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use harness_core::tool::CommandSandbox;

pub use denial::looks_like_sandbox_denial;
#[cfg(target_os = "linux")]
pub use linux::{LinuxSandbox, landlock_abi, linux_sandbox_available, linux_sandbox_command};
#[cfg(target_os = "macos")]
pub use macos::{Seatbelt, seatbelt_available, seatbelt_command};
pub use policy::{FsAccess, SandboxPolicy};

/// User settings that apply to every sandboxed command.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SandboxSettings {
    /// Extra writable directories (`sandbox.writable_roots` in config).
    pub extra_writable: Vec<PathBuf>,
    /// Allow loopback networking (`sandbox.allow_localhost`; macOS only).
    pub allow_localhost: bool,
}

impl SandboxSettings {
    /// The policy for one command with these settings.
    pub fn policy(&self, access: FsAccess, workspace: &Path) -> SandboxPolicy {
        SandboxPolicy {
            access,
            workspace: workspace.to_path_buf(),
            extra_writable: self.extra_writable.clone(),
            allow_localhost: self.allow_localhost,
        }
    }
}

/// The OS sandbox this host supports, or `None` when none is usable (the caller must then ask before
/// every shell command).
pub fn detect(settings: SandboxSettings) -> Option<Arc<dyn CommandSandbox>> {
    #[cfg(target_os = "macos")]
    if seatbelt_available() {
        return Some(Arc::new(Seatbelt::new(settings)));
    }
    #[cfg(target_os = "linux")]
    if linux_sandbox_available() {
        return Some(Arc::new(LinuxSandbox::new(settings)));
    }
    let _ = settings;
    None
}
