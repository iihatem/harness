//! Builds the Landlock ruleset fd, in the parent process.
//!
//! Everything here runs *before* `fork()`, so it is free to allocate, open
//! files (`path_beneath_rules` opens each root with `O_PATH` to resolve it)
//! and return `Result`s normally. Only the resulting ruleset fd crosses into
//! `pre_exec`; see `preexec.rs`.

use std::ffi::OsStr;
use std::io;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};

use landlock::{
    ABI, Access, AccessFs, CompatLevel, Compatible, Ruleset, RulesetAttr, RulesetCreatedAttr,
};

use crate::policy::{FsAccess, SandboxPolicy};
use crate::roots::{home_dir, safe_root};

/// Landlock ABI this crate targets. `BestEffort` compatibility (set below)
/// degrades gracefully on kernels that only implement an earlier ABI, so
/// picking a specific version here just pins which access-right bits
/// `AccessFs::from_all`/`from_read` request; it does not raise the minimum
/// kernel this crate can run on (that is [`super::detect::landlock_abi`]'s
/// job, checked separately by [`super::linux_sandbox_available`]).
const TARGET_ABI: ABI = ABI::V5;

/// Devices that must stay writable in *every* mode (including
/// [`FsAccess::ReadOnly`]) so ordinary shell pipelines keep working: piping
/// to `/dev/null`, allocating a pty, `/dev/shm` scratch space, etc.
const ALWAYS_WRITABLE_DEVICES: &[&str] = &[
    "/dev/null",
    "/dev/zero",
    "/dev/full",
    "/dev/tty",
    "/dev/ptmx",
    "/dev/pts",
    "/dev/shm",
];

/// Builds the Landlock ruleset described by `policy` and returns its
/// underlying ruleset fd, ready to be handed to `landlock_restrict_self`
/// from `pre_exec`.
///
/// Returns `Ok(None)` when the kernel has no Landlock support at all — in
/// that case there is nothing to restrict, and the caller should fall back
/// to running the child unrestricted (network denial via seccomp is
/// unaffected). Returns `Err` only for genuine setup failures, such as a
/// rule that could not be constructed.
pub fn build_ruleset_fd(policy: &SandboxPolicy) -> io::Result<Option<OwnedFd>> {
    let access_rw = AccessFs::from_all(TARGET_ABI);
    let access_ro = AccessFs::from_read(TARGET_ABI);

    let ruleset = Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(access_rw)
        .map_err(io::Error::other)?
        .create()
        .map_err(io::Error::other)?;

    // Read access to the whole filesystem. `path_beneath_rules` silently
    // skips any path it cannot open (e.g. missing on this system), which is
    // exactly what we want for "/" always being present.
    let ruleset = ruleset
        .add_rules(landlock::path_beneath_rules(["/"], access_ro))
        .map_err(io::Error::other)?;

    // The always-writable devices, regardless of `policy.access`.
    let ruleset = ruleset
        .add_rules(landlock::path_beneath_rules(
            ALWAYS_WRITABLE_DEVICES,
            access_rw,
        ))
        .map_err(io::Error::other)?;

    let ruleset = match policy.access {
        FsAccess::ReadOnly => ruleset,
        FsAccess::WorkspaceWrite => ruleset
            .add_rules(landlock::path_beneath_rules(
                writable_roots(
                    policy,
                    std::env::var_os("TMPDIR").as_deref(),
                    home_dir().as_deref(),
                ),
                access_rw,
            ))
            .map_err(io::Error::other)?,
    };

    Ok(ruleset.into())
}

/// Roots that get read-write access under [`FsAccess::WorkspaceWrite`]: the
/// workspace itself, the standard scratch directories, and any
/// caller-supplied extras.
///
/// `tmpdir` is added only when [`safe_root`] accepts it (rejecting `/`,
/// `home`, or an ancestor of `home` — the same check the macOS backend
/// applies to its own `TMPDIR`). There is no fallback here, unlike on
/// macOS: `/tmp` and `/var/tmp` are already unconditionally writable below,
/// so an unsafe or missing `TMPDIR` simply contributes nothing extra.
fn writable_roots(
    policy: &SandboxPolicy,
    tmpdir: Option<&OsStr>,
    home: Option<&Path>,
) -> Vec<PathBuf> {
    let mut roots = vec![
        policy.workspace.clone(),
        PathBuf::from("/tmp"),
        PathBuf::from("/var/tmp"),
    ];
    if let Some(tmpdir) = tmpdir.and_then(|tmpdir| safe_root(Path::new(tmpdir), home)) {
        roots.push(tmpdir);
    }
    roots.extend(policy.extra_writable.iter().cloned());
    roots
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy_for(workspace: &Path) -> SandboxPolicy {
        SandboxPolicy {
            access: FsAccess::WorkspaceWrite,
            workspace: workspace.to_path_buf(),
            extra_writable: Vec::new(),
            allow_localhost: false,
        }
    }

    fn canon_tempdir() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let canon = dir.path().canonicalize().unwrap();
        (dir, canon)
    }

    #[test]
    fn always_includes_workspace_tmp_and_var_tmp() {
        let (_ws, workspace) = canon_tempdir();
        let policy = policy_for(&workspace);
        let roots = writable_roots(&policy, None, None);
        assert!(roots.contains(&workspace));
        assert!(roots.contains(&PathBuf::from("/tmp")));
        assert!(roots.contains(&PathBuf::from("/var/tmp")));
    }

    #[test]
    fn tmpdir_set_to_root_is_excluded() {
        let (_ws, workspace) = canon_tempdir();
        let (_h, home) = canon_tempdir();
        let policy = policy_for(&workspace);
        let roots = writable_roots(&policy, Some(OsStr::new("/")), Some(&home));
        assert!(!roots.contains(&PathBuf::from("/")));
    }

    #[test]
    fn tmpdir_set_to_home_is_excluded() {
        let (_ws, workspace) = canon_tempdir();
        let (_h, home) = canon_tempdir();
        let policy = policy_for(&workspace);
        let roots = writable_roots(&policy, Some(home.as_os_str()), Some(&home));
        assert!(!roots.contains(&home));
    }

    #[test]
    fn tmpdir_set_to_an_ancestor_of_home_is_excluded() {
        let (_ws, workspace) = canon_tempdir();
        let (_h, home_parent_dir) = canon_tempdir();
        let home = home_parent_dir.join("home");
        std::fs::create_dir(&home).unwrap();
        let policy = policy_for(&workspace);
        let roots = writable_roots(&policy, Some(home_parent_dir.as_os_str()), Some(&home));
        assert!(!roots.contains(&home_parent_dir));
    }

    #[test]
    fn tmpdir_relative_is_excluded() {
        let (_ws, workspace) = canon_tempdir();
        let (_h, home) = canon_tempdir();
        let policy = policy_for(&workspace);
        let roots = writable_roots(&policy, Some(OsStr::new("relative/tmp")), Some(&home));
        assert_eq!(roots.len(), 3, "no TMPDIR root should have been added");
    }

    #[test]
    fn tmpdir_missing_path_is_excluded() {
        let (_ws, workspace) = canon_tempdir();
        let (_h, home) = canon_tempdir();
        let policy = policy_for(&workspace);
        let missing = home.join("does-not-exist");
        let roots = writable_roots(&policy, Some(missing.as_os_str()), Some(&home));
        assert_eq!(roots.len(), 3, "no TMPDIR root should have been added");
    }

    #[test]
    fn tmpdir_valid_dir_is_included_canonicalized() {
        let (_ws, workspace) = canon_tempdir();
        let (_h, home) = canon_tempdir();
        let (_t, tmp) = canon_tempdir();
        let policy = policy_for(&workspace);
        let roots = writable_roots(&policy, Some(tmp.as_os_str()), Some(&home));
        assert!(roots.contains(&tmp));
    }
}
