//! Builds the Landlock ruleset fd, in the parent process.
//!
//! Everything here runs *before* `fork()`, so it is free to allocate, open
//! files (`path_beneath_rules` opens each root with `O_PATH` to resolve it)
//! and return `Result`s normally. Only the resulting ruleset fd crosses into
//! `pre_exec`; see `preexec.rs`.
//!
//! ## Enforcement is a hard requirement, not best-effort
//!
//! [`super::detect::linux_sandbox_available`] requires Landlock ABI >= 3, so
//! by the time this module is reached the caller has already decided ABI 3
//! is available — or has bypassed that check by calling
//! [`super::linux_sandbox_command`] directly. Either way, this module must
//! never silently hand back a command that runs with weaker filesystem
//! restriction than ABI 3 grants (or none at all): the ABI-3 access rights
//! (every `AccessFs` bit through [`ABI::V3`], which includes
//! [`AccessFs::Truncate`]) are requested under
//! [`CompatLevel::HardRequirement`], so [`build_ruleset_fd`] returns an
//! `Err` — rather than a ruleset that quietly enforces less — on any kernel
//! that cannot fully satisfy them, including one with no Landlock support at
//! all. Anything *above* that floor (ABI 4/5 rights) is still requested
//! best-effort, so newer kernels get the extra restriction but older
//! ABI-3/4 kernels within our supported range are not penalized for lacking
//! it.
//!
//! ## Hard links are not covered
//!
//! Landlock's `path_beneath` rules check the path used to open a file, not
//! the underlying inode. A hard link that already exists inside a writable
//! root, whose *other* name lives outside every root (or under a path this
//! sandbox denies), stays writable through the in-root name: Landlock has
//! no rule type that can express "deny writes to this inode regardless of
//! which of its names is used," so it cannot reject that the way the macOS
//! Seatbelt backend's `has-multiple-names` rule does (see
//! `macos::profile`). Creating a *new* hard link out of a writable root
//! still requires write access at the target, so this only affects links
//! that were already present before the sandbox was applied.

use std::ffi::OsStr;
use std::io;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};

use landlock::{
    ABI, Access, AccessFs, CompatLevel, Compatible, PathBeneath, PathFd, PathFdError, Ruleset,
    RulesetAttr, RulesetCreatedAttr,
};

use crate::policy::{FsAccess, SandboxPolicy};
use crate::roots::{home_dir, safe_root};

/// Landlock ABI this crate targets on kernels that support it. `BestEffort`
/// compatibility for the rights above the ABI-3 floor (set below) degrades
/// gracefully on kernels that only implement ABI 3 or 4, so picking a
/// specific version here just pins which *extra* access-right bits
/// `AccessFs::from_all`/`from_read` request; it does not lower the minimum
/// kernel this crate can run on (that floor is [`MIN_ABI`], enforced as a
/// hard requirement below).
const TARGET_ABI: ABI = ABI::V5;

/// The minimum Landlock ABI this crate supports — mirrors
/// [`super::detect::MIN_SUPPORTED_ABI`], but as a `landlock::ABI` so it can
/// be requested as a [`CompatLevel::HardRequirement`] floor. Kept in sync
/// manually: there is no shared constant because `detect`'s probe is a raw
/// syscall (the `landlock` crate exposes no ABI-returning query) while this
/// one feeds the `landlock` crate's own compatibility API.
const MIN_ABI: ABI = ABI::V3;

/// Devices writable in *every* mode (including [`FsAccess::ReadOnly`]) so
/// ordinary shell pipelines keep working: piping to `/dev/null`, `/dev/zero`
/// or `/dev/full`, and allocating a *new* pty via `/dev/ptmx`. Deliberately
/// excludes `/dev/pts`: `pre_exec` calls `setsid()` before Landlock is
/// restricted (see `preexec.rs`), so the sandboxed command has no
/// controlling terminal of its own and its stdio is always pipes — it has
/// no legitimate reason to write to an *existing* pty slave under
/// `/dev/pts/*`, which could belong to another session on the same host.
/// `/dev/tty` (the calling process's own controlling terminal, if any) stays
/// writable for interactive-style tools; it is a different device from
/// anything under `/dev/pts`.
const ALWAYS_WRITABLE_DEVICES: &[&str] = &[
    "/dev/null",
    "/dev/zero",
    "/dev/full",
    "/dev/tty",
    "/dev/ptmx",
];

/// Additional devices writable only under [`FsAccess::WorkspaceWrite`],
/// alongside the workspace and the standard scratch directories:
/// `/dev/shm`, needed by e.g. Python's `multiprocessing` for shared-memory
/// segments. Not writable under [`FsAccess::ReadOnly`].
const WORKSPACE_WRITE_DEVICES: &[&str] = &["/dev/shm"];

/// Builds the Landlock ruleset described by `policy` and returns its
/// underlying ruleset fd, ready to be handed to `landlock_restrict_self`
/// from `pre_exec`.
///
/// Returns `Err` when the running kernel cannot fully enforce at least the
/// ABI-3 access rights this crate requires (including a kernel with no
/// Landlock support at all) — never a "succeeded but unenforced" fd. See
/// the module docs for why this is a hard requirement rather than a
/// graceful degradation.
pub fn build_ruleset_fd(policy: &SandboxPolicy) -> io::Result<OwnedFd> {
    let access_rw = AccessFs::from_all(TARGET_ABI);
    let access_ro = AccessFs::from_read(TARGET_ABI);

    let ruleset = Ruleset::default()
        // Hard floor: every access right through ABI 3 (including
        // `Truncate`) must be enforceable, or we refuse to build a ruleset
        // at all rather than silently enforce less.
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessFs::from_all(MIN_ABI))
        .map_err(io::Error::other)?
        // Above the floor: nice-to-have rights up through `TARGET_ABI`,
        // degraded gracefully on kernels that only implement ABI 3 or 4.
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
        FsAccess::WorkspaceWrite => {
            // The workspace's rule is not best-effort: without it, every write
            // to the workspace would fail with `EACCES`, and nothing would say
            // why.
            let workspace = PathFd::new(&policy.workspace).map_err(|err| {
                let PathFdError::OpenCall { source, .. } = err else {
                    return io::Error::other(err);
                };
                io::Error::new(
                    source.kind(),
                    format!(
                        "the sandbox cannot open the workspace {} for its rules: {source}",
                        policy.workspace.display()
                    ),
                )
            })?;
            let ruleset = ruleset
                .add_rule(PathBeneath::new(workspace, access_rw))
                .map_err(io::Error::other)?;
            // The other roots are best-effort: `path_beneath_rules` leaves out
            // one it cannot open (a `TMPDIR` or `writable_roots` entry that is
            // gone, say).
            let mut roots = writable_roots(
                policy,
                std::env::var_os("TMPDIR").as_deref(),
                home_dir().as_deref(),
            );
            roots.retain(|root| *root != policy.workspace);
            roots.extend(WORKSPACE_WRITE_DEVICES.iter().map(PathBuf::from));
            ruleset
                .add_rules(landlock::path_beneath_rules(roots, access_rw))
                .map_err(io::Error::other)?
        }
    };

    let fd: Option<OwnedFd> = ruleset.into();
    fd.ok_or_else(|| {
        io::Error::other(
            "Landlock ruleset created without a usable fd despite CompatLevel::HardRequirement \
             on the ABI-3 floor; refusing to run a command believing it is sandboxed",
        )
    })
}

/// Roots that get read-write access under [`FsAccess::WorkspaceWrite`]: the
/// workspace itself, the standard scratch directories, and any
/// caller-supplied extras.
///
/// `tmpdir` is added only when [`safe_root`] accepts it (rejecting `/`,
/// `home`, or an ancestor of `home` — the same check the macOS backend
/// applies to its own `TMPDIR`). There is no fallback here, unlike on
/// macOS: `/tmp` and `/var/tmp` are already unconditionally writable below,
/// so an unsafe or missing `TMPDIR` simply contributes nothing extra (the
/// caller separately overrides the *command's* `TMPDIR` env var to `/tmp`
/// in that case; see [`tmpdir_override`]).
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
    if let Some(tmpdir) = resolve_tmpdir(tmpdir, home) {
        roots.push(tmpdir);
    }
    roots.extend(policy.extra_writable.iter().cloned());
    roots
}

/// The validated `TMPDIR` root, or `None` when it is unset or [`safe_root`]
/// rejects it.
fn resolve_tmpdir(tmpdir: Option<&OsStr>, home: Option<&Path>) -> Option<PathBuf> {
    tmpdir.and_then(|tmpdir| safe_root(Path::new(tmpdir), home))
}

/// `Some("/tmp")` when `access` is [`FsAccess::WorkspaceWrite`], `tmpdir`
/// (the environment's `TMPDIR`, if any) is present but [`safe_root`] rejects
/// it (`/`, `home`, an ancestor of `home`, relative, or missing) — the
/// caller should then override the sandboxed command's own `TMPDIR` env var
/// to `/tmp`, which this ruleset always makes writable, so `mktemp` and
/// friends keep working inside it. `None` otherwise: either there is
/// nothing to override (`tmpdir` is `None`, or `access` is `ReadOnly` where
/// no scratch root is writable anyway), or `tmpdir` was accepted as-is.
///
/// Takes `tmpdir`/`home` as parameters, like [`writable_roots`], rather than
/// reading the environment itself, so it stays a pure function callers can
/// test without mutating process-global state.
pub(super) fn tmpdir_override(
    access: FsAccess,
    tmpdir: Option<&OsStr>,
    home: Option<&Path>,
) -> Option<&'static str> {
    if access != FsAccess::WorkspaceWrite {
        return None;
    }
    let raw = tmpdir?;
    if resolve_tmpdir(Some(raw), home).is_none() {
        Some("/tmp")
    } else {
        None
    }
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
        assert_eq!(roots.len(), 3);
    }

    #[test]
    fn tmpdir_set_to_root_is_excluded() {
        let (_ws, workspace) = canon_tempdir();
        let (_h, home) = canon_tempdir();
        let policy = policy_for(&workspace);
        let roots = writable_roots(&policy, Some(OsStr::new("/")), Some(&home));
        assert!(!roots.contains(&PathBuf::from("/")));
        assert_eq!(roots.len(), 3, "no TMPDIR root should have been added");
    }

    #[test]
    fn tmpdir_set_to_home_is_excluded() {
        let (_ws, workspace) = canon_tempdir();
        let (_h, home) = canon_tempdir();
        let policy = policy_for(&workspace);
        let roots = writable_roots(&policy, Some(home.as_os_str()), Some(&home));
        assert!(!roots.contains(&home));
        assert_eq!(roots.len(), 3, "no TMPDIR root should have been added");
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
        assert_eq!(roots.len(), 3, "no TMPDIR root should have been added");
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
        assert_eq!(roots.len(), 4);
    }

    #[test]
    fn tmpdir_non_canonical_spelling_is_included_canonicalized() {
        let (_ws, workspace) = canon_tempdir();
        let (_h, home) = canon_tempdir();
        let (_t, tmp) = canon_tempdir();
        std::fs::create_dir(tmp.join("sub")).unwrap();
        let non_canonical = tmp.join("sub").join("..");
        let policy = policy_for(&workspace);

        let roots = writable_roots(&policy, Some(non_canonical.as_os_str()), Some(&home));

        assert!(
            roots.contains(&tmp),
            "expected canonicalized {tmp:?} in {roots:?}"
        );
        assert_eq!(roots.len(), 4);
    }

    #[test]
    fn tmpdir_override_is_none_when_tmpdir_unset() {
        let (_h, home) = canon_tempdir();
        assert_eq!(
            tmpdir_override(FsAccess::WorkspaceWrite, None, Some(&home)),
            None
        );
    }

    #[test]
    fn tmpdir_override_is_tmp_when_tmpdir_is_rejected() {
        let (_h, home) = canon_tempdir();
        assert_eq!(
            tmpdir_override(FsAccess::WorkspaceWrite, Some(OsStr::new("/")), Some(&home)),
            Some("/tmp")
        );
    }

    #[test]
    fn tmpdir_override_is_none_when_tmpdir_is_accepted() {
        let (_h, home) = canon_tempdir();
        let (_t, tmp) = canon_tempdir();
        assert_eq!(
            tmpdir_override(FsAccess::WorkspaceWrite, Some(tmp.as_os_str()), Some(&home)),
            None
        );
    }

    #[test]
    fn tmpdir_override_is_none_under_read_only_even_when_rejected() {
        let (_h, home) = canon_tempdir();
        assert_eq!(
            tmpdir_override(FsAccess::ReadOnly, Some(OsStr::new("/")), Some(&home)),
            None
        );
    }

    #[test]
    fn a_workspace_the_rules_cannot_open_fails_the_ruleset() {
        if !crate::linux::linux_sandbox_available() {
            eprintln!("skipping: linux sandbox unavailable");
            return;
        }
        let (_dir, base) = canon_tempdir();
        let workspace = base.join("gone");
        let err = build_ruleset_fd(&policy_for(&workspace))
            .expect_err("a ruleset without the workspace would refuse every write to it");
        assert_eq!(err.kind(), io::ErrorKind::NotFound, "{err}");
        let text = err.to_string();
        assert!(text.contains(&workspace.display().to_string()), "{text}");
        assert!(text.contains("No such file or directory"), "{text}");
    }

    #[test]
    fn another_writable_root_that_cannot_be_opened_is_left_out() {
        if !crate::linux::linux_sandbox_available() {
            eprintln!("skipping: linux sandbox unavailable");
            return;
        }
        let (_ws, workspace) = canon_tempdir();
        let mut policy = policy_for(&workspace);
        policy.extra_writable.push(workspace.join("gone"));
        assert!(build_ruleset_fd(&policy).is_ok());
    }
}
