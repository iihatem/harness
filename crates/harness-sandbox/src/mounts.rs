//! What the Linux full tier mounts over git metadata, decided in the parent
//! from the guard's index (`crate::gitmeta`), and how the child says which
//! setup step failed. Platform-neutral, so it is unit-tested on every host;
//! the syscalls are in `linux/mountplan.rs` (the parent) and
//! `linux/mountns.rs` (the child).
//!
//! What [`mounts`] covers, each with a self-bind in the command's own mount
//! namespace:
//!
//! - Every gitdir is pinned with a read-write bind: a mount point cannot be
//!   renamed, removed or replaced from inside the namespace (`EBUSY`), while
//!   what git writes in a gitdir's root (`index.lock`, `COMMIT_EDITMSG`,
//!   `ORIG_HEAD`, refs, objects) still works, as a read-only bind of the
//!   whole gitdir would not allow. The gitdirs are the index's and, when its
//!   scan was incomplete, the linked-worktree and submodule gitdirs of those
//!   it found, as the guard records them ([`gitdirs`]).
//! - So is every directory on the way to a gitdir: each one the index lists
//!   on the way from a gitfile or a symlinked `.git` to its gitdir, and each
//!   one between a gitdir and a gitdir nested in it (`modules/`,
//!   `worktrees/`). Renaming one of those would otherwise carry a pinned
//!   gitdir away, and let a new one take the path a gitfile names.
//! - Every protected entry that exists in a pinned gitdir (`config`,
//!   `config.worktree`, `commondir`, `hooks/`, `gitweb/`, `pid`) is bound
//!   read-only, and so are every gitfile (each `.git` file, and each file on
//!   the way to a gitdir), and `.harness/` and `HEAD` at the top of the
//!   workspace. A pin beneath a read-only entry is read-only as well.
//!
//! Symlinks cannot be mounted over, so an entry that is a symlink, or that is
//! reached through one, is left to the guard. So is every name that does not
//! exist yet, such as a new `commondir`, and anything outside the workspace,
//! where the index lists nothing.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::Metadata;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::gitmeta::{GITDIR_PROTECTED, GitIndex, WORKSPACE_PROTECTED, nested_gitdirs};

/// How many linked-worktree and submodule gitdirs [`gitdirs`] adds beyond
/// the ones the scan found: the guard's own bound.
const MAX_NESTED: usize = 10_000;

/// One self-bind, in the order [`mounts`] gives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Mount {
    /// Absolute and canonical, strictly inside the workspace.
    pub(crate) path: PathBuf,
    /// Read-only, or a read-write pin.
    pub(crate) read_only: bool,
    /// What was at `path` when the plan was made; anything else there
    /// fails the setup.
    pub(crate) dev: u64,
    pub(crate) ino: u64,
}

/// The gitdirs to pin: the index's and, when its scan was incomplete, the
/// linked-worktree and submodule gitdirs in those, and in those, and so on,
/// which the guard also records as known (a complete scan lists them all).
pub(crate) fn gitdirs(index: &GitIndex) -> BTreeSet<PathBuf> {
    let mut gitdirs = index.gitdirs.clone();
    if !index.incomplete {
        return gitdirs;
    }
    let mut added = 0;
    let mut pending: Vec<PathBuf> = index.gitdirs.iter().cloned().collect();
    while let Some(gitdir) = pending.pop() {
        for found in nested_gitdirs(&gitdir) {
            if added == MAX_NESTED {
                return gitdirs;
            }
            if gitdirs.insert(found.clone()) {
                added += 1;
                pending.push(found);
            }
        }
    }
    gitdirs
}

/// What to mount in the canonical `workspace` to protect `gitdirs` (see
/// [`gitdirs`]) and what else `index` found, parents before children. Empty
/// when there is nothing to protect.
pub(crate) fn mounts(
    workspace: &Path,
    gitdirs: &BTreeSet<PathBuf>,
    index: &GitIndex,
) -> Vec<Mount> {
    let mut wanted = Wanted::new(workspace);
    let pinned: BTreeSet<&Path> = gitdirs
        .iter()
        .map(PathBuf::as_path)
        .filter(|gitdir| wanted.pin(gitdir))
        .collect();
    for gitdir in &pinned {
        for name in GITDIR_PROTECTED {
            wanted.read_only(&gitdir.join(name), |_| true);
        }
        // The directories between this gitdir and the pinned one it is in.
        let mut between = Vec::new();
        for ancestor in gitdir.ancestors().skip(1) {
            if !ancestor.starts_with(workspace) || ancestor == workspace {
                between.clear();
                break;
            }
            if pinned.contains(ancestor) {
                break;
            }
            between.push(ancestor);
        }
        for dir in between {
            wanted.pin(dir);
        }
    }
    for link in &index.links {
        if !wanted.pin(link) {
            wanted.read_only(link, Metadata::is_file);
        }
    }
    for dot_git in &index.dot_gits {
        wanted.read_only(dot_git, Metadata::is_file);
    }
    for name in WORKSPACE_PROTECTED {
        wanted.read_only(&workspace.join(name), |_| true);
    }
    // Parents come first, so a read-only entry is known before what is below
    // it: a pin there is read-only too, since a read-write bind would open
    // that part of it again.
    let mut read_only: Vec<PathBuf> = Vec::new();
    wanted
        .0
        .into_iter()
        .map(|(path, (own, dev, ino))| {
            let beneath = read_only.iter().any(|above| path.starts_with(above));
            if own && !beneath {
                read_only.push(path.clone());
            }
            Mount {
                path,
                read_only: own || beneath,
                dev,
                ino,
            }
        })
        .collect()
}

/// The mounts wanted so far, by path: read-only or not, and the identity.
struct Wanted<'a>(BTreeMap<PathBuf, (bool, u64, u64)>, &'a Path);

impl<'a> Wanted<'a> {
    fn new(workspace: &'a Path) -> Self {
        Wanted(BTreeMap::new(), workspace)
    }

    /// Pins `path` if it is a directory that can be mounted; says whether it
    /// is (or already was) pinned.
    fn pin(&mut self, path: &Path) -> bool {
        if self.0.contains_key(path) {
            return true;
        }
        match self.mountable(path) {
            Some(meta) if meta.is_dir() => {
                self.0
                    .insert(path.to_path_buf(), (false, meta.dev(), meta.ino()));
                true
            }
            _ => false,
        }
    }

    /// Makes `path` read-only if it can be mounted and `kind` accepts it. A
    /// pin there becomes read-only too.
    fn read_only(&mut self, path: &Path, kind: impl Fn(&Metadata) -> bool) {
        if let Some(meta) = self.mountable(path).filter(|meta| kind(meta)) {
            self.0
                .insert(path.to_path_buf(), (true, meta.dev(), meta.ino()));
        }
    }

    /// What is at `path` when it is strictly inside the workspace, not a
    /// symlink, and reached without one.
    fn mountable(&self, path: &Path) -> Option<Metadata> {
        if !path.starts_with(self.1) || path == self.1 {
            return None;
        }
        let meta = std::fs::symlink_metadata(path).ok()?;
        if meta.file_type().is_symlink() {
            return None;
        }
        (std::fs::canonicalize(path).ok()? == path).then_some(meta)
    }
}

/// A setup step, as reported through the setup pipe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Step {
    Unshare = 1,
    Setgroups,
    UidMap,
    GidMap,
    Private,
    Workspace,
    Open,
    Identity,
    OpenTree,
    ReadOnly,
    MoveMount,
    Chdir,
    Securebits,
    Capabilities,
}

impl Step {
    const ALL: [Step; 14] = [
        Step::Unshare,
        Step::Setgroups,
        Step::UidMap,
        Step::GidMap,
        Step::Private,
        Step::Workspace,
        Step::Open,
        Step::Identity,
        Step::OpenTree,
        Step::ReadOnly,
        Step::MoveMount,
        Step::Chdir,
        Step::Securebits,
        Step::Capabilities,
    ];

    fn describe(self) -> &'static str {
        match self {
            Step::Unshare => "creating a user and mount namespace",
            Step::Setgroups => "writing /proc/self/setgroups",
            Step::UidMap => "writing /proc/self/uid_map",
            Step::GidMap => "writing /proc/self/gid_map",
            Step::Private => "making mounts private",
            Step::Workspace => "opening the workspace",
            Step::Open => "opening",
            Step::Identity => "checking that nothing replaced",
            Step::OpenTree => "cloning a mount of",
            Step::ReadOnly => "making read-only",
            Step::MoveMount => "mounting",
            Step::Chdir => "changing into the working directory again",
            Step::Securebits => "locking the securebits",
            Step::Capabilities => "dropping capabilities",
        }
    }
}

/// Which step failed, for which op, with which `errno`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Failure {
    pub(crate) step: Step,
    pub(crate) op: Option<u16>,
    pub(crate) errno: i32,
}

impl Failure {
    pub(crate) const LEN: usize = 8;

    /// The bytes the child writes. Async-signal-safe: no allocation.
    pub(crate) fn encode(self) -> [u8; Failure::LEN] {
        let op = self.op.unwrap_or(u16::MAX);
        let [o0, o1] = op.to_le_bytes();
        let [e0, e1, e2, e3] = self.errno.to_le_bytes();
        [self.step as u8, 0, o0, o1, e0, e1, e2, e3]
    }

    /// Whether the kernel or the host refused the full tier: a namespace, id
    /// map or capability step, or a mount call refused as not permitted or
    /// not supported. The session then drops to the basic tier. Anything
    /// else (a path that changed between the plan and the setup, a resource
    /// limit) stops only the command it happened to, and the session keeps
    /// the full tier, so nothing outside a command can downgrade it.
    pub(crate) fn drops_tier(&self) -> bool {
        if matches!(
            self.errno,
            libc::ENOSPC | libc::ENOMEM | libc::EMFILE | libc::ENFILE
        ) {
            return false;
        }
        match self.step {
            Step::Unshare
            | Step::Setgroups
            | Step::UidMap
            | Step::GidMap
            | Step::Private
            | Step::Securebits
            | Step::Capabilities => true,
            Step::OpenTree | Step::ReadOnly | Step::MoveMount => matches!(
                self.errno,
                libc::EPERM | libc::EACCES | libc::EINVAL | libc::ENOSYS | libc::EOPNOTSUPP
            ),
            Step::Workspace | Step::Open | Step::Identity | Step::Chdir => false,
        }
    }

    /// What [`encode`](Self::encode) wrote, if `bytes` is that.
    pub(crate) fn decode(bytes: &[u8]) -> Option<Failure> {
        let bytes: &[u8; Failure::LEN] = bytes.try_into().ok()?;
        let step = *Step::ALL.iter().find(|s| **s as u8 == bytes[0])?;
        let op = u16::from_le_bytes([bytes[2], bytes[3]]);
        Some(Failure {
            step,
            op: (op != u16::MAX).then_some(op),
            errno: i32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
        })
    }

    /// What failed, naming the entry for a per-entry step. `paths` are the
    /// plan's ops' absolute paths.
    pub(crate) fn describe(&self, paths: &[PathBuf]) -> String {
        let err = io::Error::from_raw_os_error(self.errno);
        match self.op.and_then(|op| paths.get(usize::from(op))) {
            Some(path) => format!("{} {} failed: {err}", self.step.describe(), path.display()),
            None => format!("{} failed: {err}", self.step.describe()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt;

    use super::*;

    /// A canonical temporary workspace.
    fn workspace() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().canonicalize().unwrap();
        (dir, ws)
    }

    fn mkdirs(ws: &Path, dirs: &[&str]) {
        for dir in dirs {
            std::fs::create_dir_all(ws.join(dir)).unwrap();
        }
    }

    fn files(ws: &Path, files: &[(&str, &str)]) {
        for (path, text) in files {
            std::fs::write(ws.join(path), text).unwrap();
        }
    }

    fn paths(ws: &Path, rels: &[&str]) -> BTreeSet<PathBuf> {
        rels.iter().map(|rel| ws.join(rel)).collect()
    }

    /// `(path relative to ws, read_only)` for each mount, in order; and each
    /// mount's identity checked against the entry.
    fn plan(ws: &Path, index: &GitIndex) -> Vec<(String, bool)> {
        mounts(ws, &gitdirs(index), index)
            .into_iter()
            .map(|mount| {
                let meta = std::fs::symlink_metadata(&mount.path).unwrap();
                assert_eq!((mount.dev, mount.ino), (meta.dev(), meta.ino()));
                let rel = mount.path.strip_prefix(ws).unwrap();
                (rel.to_string_lossy().into_owned(), mount.read_only)
            })
            .collect()
    }

    fn expect(list: &[(&str, bool)]) -> Vec<(String, bool)> {
        list.iter()
            .map(|(rel, read_only)| (rel.to_string(), *read_only))
            .collect()
    }

    #[test]
    fn a_gitdir_is_pinned_and_what_exists_of_its_protected_entries_is_read_only() {
        let (_dir, ws) = workspace();
        mkdirs(
            &ws,
            &[".git/hooks", ".git/objects", ".git/refs", ".harness"],
        );
        files(
            &ws,
            &[
                (".git/HEAD", "ref: refs/heads/main\n"),
                (".git/config", "[core]\n"),
                (".git/description", "x\n"),
                ("HEAD", "ref: refs/heads/main\n"),
            ],
        );
        let index = GitIndex {
            dot_gits: paths(&ws, &[".git"]),
            gitdirs: paths(&ws, &[".git"]),
            ..GitIndex::default()
        };
        assert_eq!(
            plan(&ws, &index),
            expect(&[
                (".git", false),
                (".git/config", true),
                (".git/hooks", true),
                (".harness", true),
                ("HEAD", true),
            ])
        );
    }

    #[test]
    fn every_protected_name_that_exists_is_covered() {
        let (_dir, ws) = workspace();
        mkdirs(&ws, &[".git/hooks", ".git/gitweb"]);
        files(
            &ws,
            &[
                (".git/config", ""),
                (".git/config.worktree", ""),
                (".git/commondir", ".\n"),
                (".git/pid", "1\n"),
            ],
        );
        let index = GitIndex {
            gitdirs: paths(&ws, &[".git"]),
            ..GitIndex::default()
        };
        assert_eq!(
            plan(&ws, &index),
            expect(&[
                (".git", false),
                (".git/commondir", true),
                (".git/config", true),
                (".git/config.worktree", true),
                (".git/gitweb", true),
                (".git/hooks", true),
                (".git/pid", true),
            ])
        );
    }

    #[test]
    fn a_workspace_with_nothing_to_protect_has_nothing_to_mount() {
        let (_dir, ws) = workspace();
        mkdirs(&ws, &["src"]);
        files(&ws, &[("src/main.rs", "")]);
        assert_eq!(plan(&ws, &GitIndex::default()), vec![]);
    }

    #[test]
    fn symlinks_and_what_is_reached_through_one_are_left_to_the_guard() {
        let (_dir, ws) = workspace();
        mkdirs(&ws, &[".git", "hooks-elsewhere", "real/.git"]);
        files(&ws, &[(".git/config", ""), ("real/.git/config", "")]);
        std::os::unix::fs::symlink("../hooks-elsewhere", ws.join(".git/hooks")).unwrap();
        std::os::unix::fs::symlink("real", ws.join("link")).unwrap();
        std::os::unix::fs::symlink(".git", ws.join("HEAD")).unwrap();
        mkdirs(&ws, &["sub"]);
        std::os::unix::fs::symlink("../real/.git", ws.join("sub/.git")).unwrap();
        let index = GitIndex {
            dot_gits: paths(&ws, &[".git", "sub/.git", "link/.git"]),
            gitdirs: paths(&ws, &[".git", "link/.git"]),
            links: paths(&ws, &["sub/.git"]),
            ..GitIndex::default()
        };
        assert_eq!(
            plan(&ws, &index),
            expect(&[(".git", false), (".git/config", true)])
        );
    }

    #[test]
    fn nothing_outside_the_workspace_is_mounted() {
        let (_dir, ws) = workspace();
        let (_other, outside) = workspace();
        mkdirs(&outside, &[".git/hooks"]);
        files(&outside, &[(".git/config", "")]);
        let index = GitIndex {
            gitdirs: paths(&outside, &[".git"]),
            links: paths(&outside, &[".git"]),
            ..GitIndex::default()
        };
        assert_eq!(plan(&ws, &index), vec![]);
    }

    #[test]
    fn a_gitfile_is_read_only_and_the_gitdir_it_leads_to_is_pinned_with_the_way_there() {
        let (_dir, ws) = workspace();
        mkdirs(&ws, &[".git/modules/sub/hooks", "sub"]);
        files(
            &ws,
            &[
                (".git/config", ""),
                (".git/modules/sub/HEAD", "ref: refs/heads/main\n"),
                (".git/modules/sub/config", ""),
                ("sub/.git", "gitdir: ../.git/modules/sub\n"),
            ],
        );
        let index = GitIndex {
            dot_gits: paths(&ws, &[".git", "sub/.git"]),
            gitdirs: paths(&ws, &[".git", ".git/modules/sub"]),
            ..GitIndex::default()
        };
        // Renaming `.git/modules` would move the pinned gitdir away and let a new one take its
        // path, which `sub/.git` names.
        assert_eq!(
            plan(&ws, &index),
            expect(&[
                (".git", false),
                (".git/config", true),
                (".git/modules", false),
                (".git/modules/sub", false),
                (".git/modules/sub/config", true),
                (".git/modules/sub/hooks", true),
                ("sub/.git", true),
            ])
        );
    }

    #[test]
    fn directories_and_gitfiles_on_the_way_to_a_gitdir_are_covered() {
        let (_dir, ws) = workspace();
        // `.git` is a symlink to a gitfile, which names a gitdir in `.seps/`.
        mkdirs(&ws, &[".seps/main/hooks", "meta"]);
        files(
            &ws,
            &[
                (".seps/main/HEAD", "ref: refs/heads/main\n"),
                ("meta/gitfile", "gitdir: ../.seps/main\n"),
            ],
        );
        std::os::unix::fs::symlink("meta/gitfile", ws.join(".git")).unwrap();
        let index = GitIndex {
            dot_gits: paths(&ws, &[".git"]),
            gitdirs: paths(&ws, &[".seps/main"]),
            links: paths(&ws, &["meta", "meta/gitfile", ".seps", ".seps/main"]),
            ..GitIndex::default()
        };
        assert_eq!(
            plan(&ws, &index),
            expect(&[
                (".seps", false),
                (".seps/main", false),
                (".seps/main/hooks", true),
                ("meta", false),
                ("meta/gitfile", true),
            ])
        );
    }

    #[test]
    fn directories_between_a_gitdir_and_the_ones_nested_in_it_are_pinned() {
        let (_dir, ws) = workspace();
        mkdirs(
            &ws,
            &[
                ".git/worktrees/w",
                ".git/modules/group/sub/modules/inner",
                "vendor/lib/.git",
            ],
        );
        let index = GitIndex {
            gitdirs: paths(
                &ws,
                &[
                    ".git",
                    ".git/worktrees/w",
                    ".git/modules/group/sub",
                    ".git/modules/group/sub/modules/inner",
                    "vendor/lib/.git",
                ],
            ),
            ..GitIndex::default()
        };
        // `vendor/` and `vendor/lib/` are ordinary directories: a repository moved away from
        // there, and a new one made in its place, is the guard's to find.
        assert_eq!(
            plan(&ws, &index),
            expect(&[
                (".git", false),
                (".git/modules", false),
                (".git/modules/group", false),
                (".git/modules/group/sub", false),
                (".git/modules/group/sub/modules", false),
                (".git/modules/group/sub/modules/inner", false),
                (".git/worktrees", false),
                (".git/worktrees/w", false),
                ("vendor/lib/.git", false),
            ])
        );
    }

    #[test]
    fn a_protected_entry_is_read_only_even_on_the_way_to_a_gitdir() {
        let (_dir, ws) = workspace();
        mkdirs(&ws, &[".harness/g/hooks"]);
        let index = GitIndex {
            gitdirs: paths(&ws, &[".harness/g"]),
            links: paths(&ws, &[".harness"]),
            ..GitIndex::default()
        };
        // A pin beneath a read-only entry is read-only too: a read-write one would open that
        // part of it again. It still cannot be renamed.
        assert_eq!(
            plan(&ws, &index),
            expect(&[
                (".harness", true),
                (".harness/g", true),
                (".harness/g/hooks", true)
            ])
        );
    }

    #[test]
    fn after_an_incomplete_scan_the_nested_gitdirs_the_guard_records_are_pinned_too() {
        let (_dir, ws) = workspace();
        mkdirs(&ws, &[".git/worktrees/w", ".git/modules/sub/refs"]);
        files(
            &ws,
            &[
                (".git/modules/sub/HEAD", "ref: refs/heads/main\n"),
                (".git/modules/sub/config", ""),
            ],
        );
        let index = GitIndex {
            gitdirs: paths(&ws, &[".git"]),
            incomplete: true,
            ..GitIndex::default()
        };
        assert_eq!(
            gitdirs(&index),
            paths(&ws, &[".git", ".git/modules/sub", ".git/worktrees/w"])
        );
        assert_eq!(
            plan(&ws, &index),
            expect(&[
                (".git", false),
                (".git/modules", false),
                (".git/modules/sub", false),
                (".git/modules/sub/config", true),
                (".git/worktrees", false),
                (".git/worktrees/w", false),
            ])
        );
        // A complete scan lists every gitdir there is.
        let complete = GitIndex {
            incomplete: false,
            ..index
        };
        assert_eq!(gitdirs(&complete), paths(&ws, &[".git"]));
    }

    #[test]
    fn a_pin_must_be_a_directory() {
        let (_dir, ws) = workspace();
        files(&ws, &[(".git", "gitdir: elsewhere\n")]);
        let index = GitIndex {
            gitdirs: paths(&ws, &[".git"]),
            ..GitIndex::default()
        };
        assert_eq!(plan(&ws, &index), vec![]);
    }

    #[test]
    fn a_failure_survives_the_pipe() {
        let failure = Failure {
            step: Step::UidMap,
            op: None,
            errno: libc::EPERM,
        };
        assert_eq!(Failure::decode(&failure.encode()), Some(failure));
        let failure = Failure {
            step: Step::MoveMount,
            op: Some(3),
            errno: libc::EINVAL,
        };
        assert_eq!(Failure::decode(&failure.encode()), Some(failure));
        assert_eq!(Failure::decode(&[0; 8]), None);
        let unknown = Step::ALL.len() as u8 + 1;
        assert_eq!(Failure::decode(&[unknown; 8]), None);
        assert_eq!(Failure::decode(&[1; 7]), None);
    }

    #[test]
    fn every_step_has_a_distinct_code() {
        for (i, step) in Step::ALL.iter().enumerate() {
            assert_eq!(*step as u8 as usize, i + 1);
        }
    }

    fn failed(step: Step, errno: i32) -> Failure {
        Failure {
            step,
            op: None,
            errno,
        }
    }

    #[test]
    fn a_refused_namespace_or_capability_step_drops_the_tier() {
        for step in [
            Step::Unshare,
            Step::Setgroups,
            Step::UidMap,
            Step::GidMap,
            Step::Private,
            Step::Securebits,
            Step::Capabilities,
        ] {
            for errno in [libc::EPERM, libc::EACCES, libc::EINVAL, libc::ENOSYS] {
                assert!(failed(step, errno).drops_tier(), "{step:?} {errno}");
            }
        }
    }

    #[test]
    fn a_refused_mount_call_drops_the_tier() {
        for step in [Step::OpenTree, Step::ReadOnly, Step::MoveMount] {
            for errno in [
                libc::EPERM,
                libc::EACCES,
                libc::EINVAL,
                libc::ENOSYS,
                libc::EOPNOTSUPP,
            ] {
                assert!(failed(step, errno).drops_tier(), "{step:?} {errno}");
            }
        }
    }

    #[test]
    fn a_path_that_changed_keeps_the_tier() {
        for step in [Step::Workspace, Step::Open, Step::Identity, Step::Chdir] {
            for errno in [
                libc::ENOENT,
                libc::ESTALE,
                libc::ELOOP,
                libc::EXDEV,
                libc::EPERM,
                libc::EACCES,
            ] {
                assert!(!failed(step, errno).drops_tier(), "{step:?} {errno}");
            }
        }
        for step in [Step::OpenTree, Step::MoveMount] {
            assert!(!failed(step, libc::ENOENT).drops_tier(), "{step:?}");
        }
    }

    #[test]
    fn a_resource_limit_keeps_the_tier_at_any_step() {
        for step in Step::ALL {
            for errno in [libc::ENOSPC, libc::ENOMEM, libc::EMFILE, libc::ENFILE] {
                assert!(!failed(step, errno).drops_tier(), "{step:?} {errno}");
            }
        }
    }

    #[test]
    fn a_failure_names_the_step_and_the_entry() {
        let paths = vec![PathBuf::from("/ws/.git"), PathBuf::from("/ws/.git/config")];
        let failure = Failure {
            step: Step::Identity,
            op: Some(1),
            errno: libc::ESTALE,
        };
        let text = failure.describe(&paths);
        assert!(
            text.starts_with("checking that nothing replaced /ws/.git/config failed: "),
            "{text}"
        );
        let failure = Failure {
            step: Step::UidMap,
            op: None,
            errno: libc::EPERM,
        };
        let text = failure.describe(&paths);
        assert!(
            text.starts_with("writing /proc/self/uid_map failed: "),
            "{text}"
        );
        let text = failed(Step::Capabilities, libc::EPERM).describe(&paths);
        assert!(text.starts_with("dropping capabilities failed: "), "{text}");
        let text = failed(Step::Securebits, libc::EPERM).describe(&paths);
        assert!(
            text.starts_with("locking the securebits failed: "),
            "{text}"
        );
        // An op the paths do not reach is not named.
        let failure = Failure {
            step: Step::MoveMount,
            op: Some(9),
            errno: libc::EINVAL,
        };
        assert!(failure.describe(&paths).starts_with("mounting failed: "));
    }
}
