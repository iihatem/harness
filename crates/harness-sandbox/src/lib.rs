//! OS sandboxes for shell commands: Seatbelt (`sandbox-exec`) on macOS and Landlock + seccomp on Linux.
//! Both implement [`harness_core::tool::CommandSandbox`]; [`detect`] picks the one this host supports.
//! On Linux, git metadata inside the workspace is protected by read-only mounts where user
//! namespaces work (the full tier) and by the [`guard`] in either tier.

mod denial;
pub mod gitmeta;
pub mod guard;
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
// What the Linux full tier mounts over git metadata. Platform-neutral, so it is unit-tested on
// every host.
#[cfg(any(
    test,
    all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    )
))]
mod mounts;
mod policy;
// The processes sandboxed commands leave running, for Linux git-metadata protection in both
// tiers. Its platform-neutral parts are unit-tested on every host.
#[cfg(any(
    test,
    all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    )
))]
mod procs;
mod roots;
// The watcher that runs the git-metadata guard's checks as protected names change. Its
// platform-neutral parts are unit-tested on every host; its kernel side is in `linux`.
#[cfg(any(
    test,
    all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    )
))]
mod watch;

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
pub use linux::{
    LinuxSandbox, landlock_abi, linux_git_protection, linux_sandbox_available,
    linux_sandbox_command, watcher_failures,
};
#[cfg(target_os = "macos")]
pub use macos::{Seatbelt, seatbelt_available, seatbelt_command};
pub use policy::{FsAccess, SandboxPolicy};
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
pub use procs::{probe_pidfd_support, reaps_through_pidfds, subreaper_active};

/// User settings that apply to every sandboxed command.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SandboxSettings {
    /// Extra writable directories (`sandbox.writable_roots` in config).
    pub extra_writable: Vec<PathBuf>,
    /// Allow loopback networking (`sandbox.allow_localhost`; macOS only).
    pub allow_localhost: bool,
    /// Where the Linux git-metadata guard moves what it takes out of the workspace. The CLI
    /// passes `<data dir>/quarantine`; `None` means `harness-quarantine` in the temp directory.
    pub quarantine_dir: Option<PathBuf>,
    /// `sandbox.linux_git_protection = "required"`: on Linux, refuse to run a workspace-write
    /// command in the basic tier (the CLI then treats the session as having no sandbox).
    pub require_full_git_protection: bool,
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

/// The directories a sandboxed command that may write the workspace can write to, devices aside:
/// the workspace, the temp directories (on macOS, the per-user ones too) and the configured
/// `writable_roots`. Each is canonicalized when it exists, so that a canonical path inside one
/// starts with it. What harness keeps outside the sandbox's reach must not be in any of them.
pub fn writable_roots(settings: &SandboxSettings, workspace: &Path) -> Vec<PathBuf> {
    let home = roots::home_dir();
    let mut out = vec![
        workspace.to_path_buf(),
        PathBuf::from("/tmp"),
        PathBuf::from("/var/tmp"),
    ];
    out.extend(
        std::env::var_os("TMPDIR")
            .and_then(|dir| roots::safe_root(Path::new(&dir), home.as_deref())),
    );
    #[cfg(target_os = "macos")]
    out.extend(macos::user_writable_roots());
    out.extend(settings.extra_writable.iter().cloned());
    out.into_iter()
        .map(|root| std::fs::canonicalize(&root).unwrap_or(root))
        .collect()
}

/// Why [`detect`] finds no sandbox on this host, for `harness sandbox doctor`.
pub fn unavailable_reason() -> String {
    #[cfg(target_os = "macos")]
    {
        format!(
            "{} is missing or cannot apply a profile",
            macos::SANDBOX_EXEC_PATH
        )
    }
    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    {
        match landlock_abi() {
            None => "Landlock is not enabled in this kernel".to_string(),
            Some(abi) if abi < 3 => format!(
                "this kernel has Landlock ABI {abi}; harness needs ABI 3 or later (Linux 6.2+)"
            ),
            Some(_) => "the seccomp filter could not be built for this system".to_string(),
        }
    }
    #[cfg(not(any(
        target_os = "macos",
        all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        )
    )))]
    {
        "harness has no sandbox for this system".to_string()
    }
}

/// The OS sandbox this host supports, or `None` when none is usable (the caller must then ask before
/// every shell command). On Linux this probes the git-protection tier, once per process.
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
    fn writable_roots_cover_the_workspace_the_temp_directories_and_the_configured_ones() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("ws");
        let extra = dir.path().join("extra");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&extra).unwrap();
        let settings = SandboxSettings {
            extra_writable: vec![extra.clone()],
            ..SandboxSettings::default()
        };
        let roots = writable_roots(&settings, &workspace);
        for expected in [&workspace, &extra, Path::new("/tmp")] {
            let expected = std::fs::canonicalize(expected).unwrap();
            assert!(roots.contains(&expected), "{expected:?} not in {roots:?}");
        }
    }

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

    #[test]
    fn there_is_always_a_reason_to_give_for_having_no_sandbox() {
        let reason = unavailable_reason();
        assert!(!reason.is_empty());
        if cfg!(target_os = "macos") {
            assert!(reason.starts_with("/usr/bin/sandbox-exec "), "{reason}");
        }
    }
}
