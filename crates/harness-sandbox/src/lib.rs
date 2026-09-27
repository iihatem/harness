//! OS sandboxes for shell commands: Seatbelt (`sandbox-exec`) on macOS and Landlock + seccomp on Linux.
//! Both implement [`harness_core::tool::CommandSandbox`]; [`detect`] picks the one this host supports.

mod denial;
pub mod gitmeta;
// Only x86_64/aarch64 are supported: `linux::seccomp` only knows how to
// target those two architectures. Any other Linux architecture skips this
// module entirely and compiles as if no sandbox backend were available,
// rather than failing to build.
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
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
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
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

/// Whether `workspace` is too broad to make writable: `/`, `$HOME`, or an ancestor of `$HOME`,
/// where workspace-write access would cover the user's dotfiles. A path that cannot be
/// canonicalized counts as too broad. The same check the sandboxes apply to temp and cache roots.
pub fn workspace_is_too_broad(workspace: &Path) -> bool {
    roots::safe_root(workspace, roots::home_dir().as_deref()).is_none()
}

/// The OS sandbox this host supports, or `None` when none is usable (the caller must then ask before
/// every shell command).
pub fn detect(settings: SandboxSettings) -> Option<Arc<dyn CommandSandbox>> {
    #[cfg(target_os = "macos")]
    if seatbelt_available() {
        return Some(Arc::new(Seatbelt::new(settings)));
    }
    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    if linux_sandbox_available() {
        return Some(Arc::new(LinuxSandbox::new(settings)));
    }
    let _ = settings;
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_home_and_its_ancestors_are_too_broad_for_a_workspace() {
        assert!(workspace_is_too_broad(Path::new("/")));
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            assert!(workspace_is_too_broad(&home));
            if let Some(parent) = home.parent() {
                assert!(workspace_is_too_broad(parent));
            }
        }
        let dir = tempfile::tempdir().unwrap();
        assert!(!workspace_is_too_broad(dir.path()));
        assert!(workspace_is_too_broad(&dir.path().join("missing")));
    }
}
