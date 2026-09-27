//! Finds the gitdir a `.git` leads to when it is a symlink or a gitfile, so
//! the Seatbelt profile (for the workspace's own `.git`) and the Linux guard
//! (for every `.git`) can protect it like a `.git` directory.
//!
//! Git follows a `.git` symlink, and reads a `.git` file (a gitfile, also
//! when reached through a symlink) as `gitdir: <path>`, relative to the
//! directory that holds `.git` (setup.c `read_gitfile_gently`). In that
//! gitdir it reads `commondir`, relative to the gitdir, and takes config and
//! hooks from the dir it names (`get_common_dir_noenv`). Seatbelt matches
//! resolved paths, so none of these gitdirs has a `.git` component the
//! profile's `.git` rules would see.
//!
//! Symlinks are resolved with `readlink` alone, and gitfiles and `commondir`
//! files are read with [`read_regular`]: whatever the workspace holds, looking
//! never blocks.

use std::collections::{HashSet, VecDeque};
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use super::read::{missing, read_regular};

/// Symlinks followed on one path before giving up, as the kernel does for a
/// loop.
const MAX_SYMLINKS: usize = 40;

/// The most of a gitfile or `commondir` file read. A path is shorter.
const MAX_POINTER_BYTES: u64 = 4096;

/// What to protect for a `.git` that is a symlink or a gitfile. Only paths
/// inside the workspace are listed; nothing outside it is writable from the
/// workspace-write sandbox except the temp and cache roots.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct LinkedGitdirs {
    /// Gitdirs git uses in this workspace: the one `.git` leads to, and the
    /// one its `commondir` file names.
    pub gitdirs: Vec<PathBuf>,
    /// Entries below the workspace that the way to those gitdirs passes
    /// through: each symlink and directory, the gitfile, and the gitdirs
    /// themselves. `.git` itself is left out, and so is the directory that
    /// holds it and everything above that: the profile and the guard protect
    /// every `.git` entry already.
    pub entries: Vec<PathBuf>,
    /// Whether a gitfile or `commondir` file on the way is there but could
    /// not be read, so a gitdir may be missing. (The profile has no use for
    /// it; the Linux guard reports it.)
    pub unreadable: bool,
}

/// What resolving symlinks may look at: `readlink` calls, and the length of
/// the path being resolved. Past either, a resolution gives up, as it does
/// after [`MAX_SYMLINKS`] symlinks, and [`Allowance::ran_out`] says so.
#[derive(Debug)]
pub(crate) struct Allowance {
    lookups_left: usize,
    max_path: usize,
    /// `readlink` calls made.
    pub(crate) spent: usize,
    /// Whether a resolution gave up for want of it.
    pub(crate) ran_out: bool,
}

impl Allowance {
    /// No bound: the macOS profile resolves whatever git would.
    #[cfg(any(target_os = "macos", test))]
    pub(crate) fn unlimited() -> Allowance {
        Allowance::new(usize::MAX, usize::MAX)
    }

    /// At most `lookups` lookups, of paths at most `max_path` bytes long.
    pub(crate) fn new(lookups: usize, max_path: usize) -> Allowance {
        Allowance {
            lookups_left: lookups,
            max_path,
            spent: 0,
            ran_out: false,
        }
    }

    /// Takes one lookup of `path`: `false` when none is left or the path is
    /// too long.
    fn take(&mut self, path: &Path) -> bool {
        if self.lookups_left == 0 || path.as_os_str().len() > self.max_path {
            self.ran_out = true;
            return false;
        }
        self.lookups_left -= 1;
        self.spent += 1;
        true
    }
}

/// A gitfile or `commondir` file that is there but cannot be read through
/// [`read_regular`]: not a regular file, not readable, or behind a symlink
/// loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Unreadable;

/// `found`, noting in `unreadable` when it is [`Unreadable`].
pub(super) fn noting<T>(found: Result<Option<T>, Unreadable>, unreadable: &mut bool) -> Option<T> {
    found.unwrap_or_else(|Unreadable| {
        *unreadable = true;
        None
    })
}

/// The gitdirs `<workspace>/.git` leads to when it is a symlink or a gitfile.
/// `workspace` must be canonical. A plain `.git` directory needs nothing
/// extra: the profile's `.git` rules cover it.
///
/// A path that does not exist yet still counts (a dangling `.git` symlink
/// names where git would look once it is created). The entries on the way
/// are listed even when no gitdir is found, so a symlinked gitfile git
/// rejects today cannot be rewritten into one it accepts.
#[cfg(target_os = "macos")]
pub(crate) fn linked_gitdirs(workspace: &Path) -> LinkedGitdirs {
    linked_gitdirs_at(workspace, workspace, &mut Allowance::unlimited())
}

/// [`linked_gitdirs`] for the `.git` in `holder`, a directory in the
/// canonical `workspace` (or the workspace itself), within `allowance`. A
/// gitfile's path is relative to `holder`.
pub(crate) fn linked_gitdirs_at(
    holder: &Path,
    workspace: &Path,
    allowance: &mut Allowance,
) -> LinkedGitdirs {
    let dot_git = holder.join(".git");
    let Ok(meta) = std::fs::symlink_metadata(&dot_git) else {
        return LinkedGitdirs::default();
    };
    if !(meta.file_type().is_symlink() || meta.is_file()) {
        return LinkedGitdirs::default();
    }

    let mut visited = Vec::new();
    let mut gitdirs = Vec::new();
    let mut unreadable = false;
    let gitdir = gitdir_of(holder, &dot_git, &mut visited, allowance);
    if let Some(gitdir) = noting(gitdir, &mut unreadable) {
        let common = noting(
            common_dir(&gitdir, &mut visited, allowance),
            &mut unreadable,
        );
        gitdirs.push(gitdir);
        gitdirs.extend(common);
    }

    let mut entries: Vec<PathBuf> = Vec::new();
    let mut seen = HashSet::new();
    for entry in visited {
        if !holder.starts_with(&entry)
            && entry != dot_git
            && within(&entry, workspace)
            && seen.insert(entry.clone())
        {
            entries.push(entry);
        }
    }
    gitdirs.retain(|gitdir| within(gitdir, workspace));
    LinkedGitdirs {
        gitdirs,
        entries,
        unreadable,
    }
}

/// Where the `.git` in `holder` leads, inside the workspace or not: the
/// directory it is or resolves to, or the path its gitfile names.
pub(super) fn gitdir_at(
    holder: &Path,
    allowance: &mut Allowance,
) -> Result<Option<PathBuf>, Unreadable> {
    gitdir_of(holder, &holder.join(".git"), &mut Vec::new(), allowance)
}

/// The directory `gitdir`'s `commondir` file names, relative to `gitdir`, as
/// git resolves it. Every entry on the way is pushed to `visited`.
pub(super) fn common_dir(
    gitdir: &Path,
    visited: &mut Vec<PathBuf>,
    allowance: &mut Allowance,
) -> Result<Option<PathBuf>, Unreadable> {
    Ok(pointer(&gitdir.join("commondir"), b"", allowance)?
        .and_then(|common| follow(&gitdir.join(common), visited, allowance)))
}

/// Where git finds the gitdir through `dot_git`: the directory it resolves
/// to, or, when it resolves to a regular file, the path that gitfile names.
fn gitdir_of(
    holder: &Path,
    dot_git: &Path,
    visited: &mut Vec<PathBuf>,
    allowance: &mut Allowance,
) -> Result<Option<PathBuf>, Unreadable> {
    let Some(target) = follow(dot_git, visited, allowance) else {
        return Ok(None);
    };
    if !std::fs::metadata(&target).is_ok_and(|m| m.is_file()) {
        // A directory, a path that does not exist, or something git cannot
        // read a gitfile from (a FIFO is never opened).
        return Ok(Some(target));
    }
    let Some(named) = pointer(&target, b"gitdir: ", allowance)? else {
        return Ok(None);
    };
    // Relative to the directory holding `.git`, even through a symlink.
    Ok(follow(&holder.join(named), visited, allowance))
}

/// The path a gitfile (`prefix` `gitdir: `) or a `commondir` file (no prefix)
/// holds, as git reads it: up to the first NUL, without trailing newlines.
/// `None` if there is no such file (git follows a symlinked `commondir`), or
/// it lacks the prefix or names nothing.
fn pointer(
    file: &Path,
    prefix: &[u8],
    allowance: &mut Allowance,
) -> Result<Option<PathBuf>, Unreadable> {
    let file = follow(file, &mut Vec::new(), allowance).ok_or(Unreadable)?;
    let bytes = match read_regular(&file, MAX_POINTER_BYTES) {
        Ok(bytes) => bytes,
        Err(err) if missing(&err) => return Ok(None),
        Err(_) => return Err(Unreadable),
    };
    let text = bytes.split(|&b| b == 0).next().unwrap_or_default();
    let Some(mut path) = text.strip_prefix(prefix) else {
        return Ok(None);
    };
    while let [rest @ .., b'\n' | b'\r'] = path {
        path = rest;
    }
    Ok((!path.is_empty()).then(|| PathBuf::from(OsStr::from_bytes(path))))
}

/// Resolves the absolute `path` as the kernel would, following every symlink
/// on the way, and returns where it ends up. A component that does not exist
/// is taken as written, and so is everything after it. Every entry looked at
/// is pushed to `visited`, each symlink before it is followed. `None` after
/// [`MAX_SYMLINKS`] symlinks, or when `allowance` runs out: each entry looked
/// at takes one lookup.
fn follow(path: &Path, visited: &mut Vec<PathBuf>, allowance: &mut Allowance) -> Option<PathBuf> {
    let mut resolved = PathBuf::from("/");
    let mut pending = VecDeque::new();
    prepend(&mut pending, path);
    let mut symlinks = 0;
    while let Some(name) = pending.pop_front() {
        let Some(name) = name else {
            resolved.pop();
            continue;
        };
        let next = resolved.join(&name);
        if !allowance.take(&next) {
            return None;
        }
        visited.push(next.clone());
        // `readlink` fails for anything but a symlink.
        match std::fs::read_link(&next) {
            Ok(target) => {
                symlinks += 1;
                if symlinks > MAX_SYMLINKS {
                    return None;
                }
                if target.is_absolute() {
                    resolved = PathBuf::from("/");
                }
                prepend(&mut pending, &target);
            }
            _ => resolved = next,
        }
    }
    Some(resolved)
}

/// Puts `path`'s components at the front of `pending`: a name, or `None` for
/// `..`. The root and `.` add nothing.
fn prepend(pending: &mut VecDeque<Option<OsString>>, path: &Path) {
    for component in path.components().rev() {
        match component {
            Component::Normal(name) => pending.push_front(Some(name.to_owned())),
            Component::ParentDir => pending.push_front(None),
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
        }
    }
}

/// Whether `path` is `base` or below it. On macOS names are compared ASCII
/// case-insensitively: on the default case-insensitive volume a symlink may
/// spell the workspace in another case, and Seatbelt's rules match either.
/// Elsewhere names must match exactly.
pub(crate) fn within(path: &Path, base: &Path) -> bool {
    let mut components = path.components();
    base.components().all(|b| {
        components
            .next()
            .is_some_and(|c| same_name(c.as_os_str(), b.as_os_str()))
    })
}

fn same_name(a: &OsStr, b: &OsStr) -> bool {
    if cfg!(target_os = "macos") {
        a.as_bytes().eq_ignore_ascii_case(b.as_bytes())
    } else {
        a == b
    }
}

/// Test helper: symlinks `{name}0` to `{name}{links - 1}` in `dir`, each to
/// the next and the last to `end` (as is, when it is absolute). Each goes
/// through 90 missing names of its own and back out with `..`, so resolving
/// the chain looks at about 90 entries per link, all different.
#[cfg(test)]
pub(super) fn long_chain(dir: &Path, name: &str, links: usize, end: &str) {
    for k in 0..links {
        let mut target: String = (0..90).map(|i| format!("{name}{k}.{i}/")).collect();
        target.push_str(&"../".repeat(90));
        if k + 1 < links {
            target.push_str(&format!("{name}{}", k + 1));
        } else if Path::new(end).is_absolute() {
            target = end.to_string();
        } else {
            target.push_str(end);
        }
        std::os::unix::fs::symlink(target, dir.join(format!("{name}{k}"))).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use super::*;

    /// The workspace's own `.git`, as the macOS profile asks for it.
    fn linked_gitdirs(workspace: &Path) -> LinkedGitdirs {
        linked_gitdirs_at(workspace, workspace, &mut Allowance::unlimited())
    }

    fn workspace() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let canon = dir.path().canonicalize().unwrap();
        (dir, canon)
    }

    fn mkdirs(ws: &Path, rel: &str) {
        std::fs::create_dir_all(ws.join(rel)).unwrap();
    }

    /// Asserts the gitdirs found, and that `entries` are among the protected
    /// entries.
    #[track_caller]
    fn check(ws: &Path, gitdirs: &[&str], entries: &[&str]) {
        let found = linked_gitdirs(ws);
        let want: Vec<PathBuf> = gitdirs.iter().map(|g| ws.join(g)).collect();
        assert_eq!(found.gitdirs, want, "{found:?}");
        for entry in entries {
            assert!(
                found.entries.contains(&ws.join(entry)),
                "{entry} is not protected: {found:?}"
            );
        }
        for entry in &found.entries {
            assert!(
                entry.starts_with(ws) && entry != ws && entry != &ws.join(".git"),
                "{entry:?} is not below the workspace, or is `.git`"
            );
        }
    }

    #[test]
    fn a_missing_or_plain_git_dir_needs_nothing_extra() {
        let (_d, ws) = workspace();
        assert_eq!(linked_gitdirs(&ws), LinkedGitdirs::default());
        mkdirs(&ws, ".git/hooks");
        assert_eq!(linked_gitdirs(&ws), LinkedGitdirs::default());
    }

    #[test]
    fn a_symlinked_git_leads_to_its_target() {
        let (_d, ws) = workspace();
        mkdirs(&ws, "gitstuff");
        symlink("gitstuff", ws.join(".git")).unwrap();
        check(&ws, &["gitstuff"], &["gitstuff"]);
    }

    #[test]
    fn every_link_and_directory_on_the_way_is_protected() {
        let (_d, ws) = workspace();
        mkdirs(&ws, "real/gitstuff");
        symlink("real", ws.join("link")).unwrap();
        symlink("link/gitstuff", ws.join(".git")).unwrap();
        check(&ws, &["real/gitstuff"], &["link", "real", "real/gitstuff"]);
    }

    #[test]
    fn a_dotdot_after_a_symlink_leaves_its_target() {
        let (_d, ws) = workspace();
        mkdirs(&ws, "a/b");
        mkdirs(&ws, "a/gitstuff");
        symlink("a/b", ws.join("ab")).unwrap();
        symlink("ab/../gitstuff", ws.join(".git")).unwrap();
        check(&ws, &["a/gitstuff"], &["ab", "a", "a/b", "a/gitstuff"]);
    }

    #[test]
    fn a_gitfile_names_its_gitdir_relative_to_the_workspace() {
        let (_d, ws) = workspace();
        mkdirs(&ws, "meta/repo");
        std::fs::write(ws.join(".git"), "gitdir: meta/repo\r\n").unwrap();
        check(&ws, &["meta/repo"], &["meta", "meta/repo"]);

        let absolute = format!("gitdir: {}\n", ws.join("meta/repo").display());
        std::fs::write(ws.join(".git"), absolute).unwrap();
        check(&ws, &["meta/repo"], &["meta", "meta/repo"]);
    }

    #[test]
    fn a_symlinked_gitfile_is_read_relative_to_the_workspace() {
        let (_d, ws) = workspace();
        mkdirs(&ws, "meta");
        mkdirs(&ws, "repo");
        std::fs::write(ws.join("meta/gitfile"), "gitdir: repo\n").unwrap();
        symlink("meta/gitfile", ws.join(".git")).unwrap();
        check(&ws, &["repo"], &["meta", "meta/gitfile", "repo"]);
    }

    #[test]
    fn a_missing_target_still_counts() {
        let (_d, ws) = workspace();
        symlink("missing/gitstuff", ws.join(".git")).unwrap();
        check(&ws, &["missing/gitstuff"], &["missing", "missing/gitstuff"]);

        std::fs::remove_file(ws.join(".git")).unwrap();
        std::fs::write(ws.join(".git"), "gitdir: gone/repo\n").unwrap();
        check(&ws, &["gone/repo"], &["gone", "gone/repo"]);
    }

    #[test]
    fn the_gitdirs_commondir_is_followed() {
        let (_d, ws) = workspace();
        mkdirs(&ws, "meta/main");
        mkdirs(&ws, "meta/wt");
        std::fs::write(ws.join("meta/wt/commondir"), "../main\n").unwrap();
        std::fs::write(ws.join(".git"), "gitdir: meta/wt\n").unwrap();
        check(
            &ws,
            &["meta/wt", "meta/main"],
            &["meta", "meta/wt", "meta/main"],
        );
    }

    #[test]
    fn a_gitdir_outside_the_workspace_is_left_out() {
        let (_d, ws) = workspace();
        let (_o, outside) = workspace();
        mkdirs(&outside, "gitstuff");
        symlink(outside.join("gitstuff"), ws.join(".git")).unwrap();
        check(&ws, &[], &[]);

        std::fs::remove_file(ws.join(".git")).unwrap();
        std::fs::write(ws.join(".git"), "gitdir: ../elsewhere\n").unwrap();
        check(&ws, &[], &[]);
    }

    #[test]
    fn a_gitdir_spelled_in_another_case_is_still_inside() {
        let (_d, ws) = workspace();
        mkdirs(&ws, "gitstuff");
        let upper = PathBuf::from(ws.to_str().unwrap().to_uppercase());
        if !upper.join("gitstuff").is_dir() {
            eprintln!("case-sensitive volume: skipping");
            return;
        }
        symlink(upper.join("gitstuff"), ws.join(".git")).unwrap();
        let found = linked_gitdirs(&ws);
        assert_eq!(found.gitdirs, vec![upper.join("gitstuff")], "{found:?}");
    }

    #[test]
    fn a_symlink_loop_or_an_invalid_gitfile_leads_nowhere() {
        let (_d, ws) = workspace();
        symlink(".git", ws.join(".git")).unwrap();
        check(&ws, &[], &[]);
        std::fs::remove_file(ws.join(".git")).unwrap();

        // Git wants exactly `gitdir: ` and a path.
        for text in ["gitdir:repo\n", "gitdir: \n", "nonsense\n", ""] {
            std::fs::write(ws.join(".git"), text).unwrap();
            check(&ws, &[], &[]);
        }

        // The gitfile behind a symlink stays protected even when it is invalid.
        std::fs::remove_file(ws.join(".git")).unwrap();
        mkdirs(&ws, "meta");
        std::fs::write(ws.join("meta/gitfile"), "nonsense\n").unwrap();
        symlink("meta/gitfile", ws.join(".git")).unwrap();
        check(&ws, &[], &["meta", "meta/gitfile"]);
    }

    #[test]
    fn a_fifo_is_never_opened() {
        let (_d, ws) = workspace();
        let status = std::process::Command::new("/usr/bin/mkfifo")
            .arg(ws.join("fifo"))
            .status()
            .unwrap();
        assert!(status.success());
        symlink("fifo", ws.join(".git")).unwrap();
        // Reading it would block forever; it is taken as the gitdir's path.
        check(&ws, &["fifo"], &["fifo"]);
    }

    #[test]
    fn a_nested_gitfile_is_read_relative_to_its_own_directory() {
        let (_d, ws) = workspace();
        mkdirs(&ws, ".git/modules/sub");
        mkdirs(&ws, "sub");
        std::fs::write(ws.join("sub/.git"), "gitdir: ../.git/modules/sub\n").unwrap();
        let found = linked_gitdirs_at(&ws.join("sub"), &ws, &mut Allowance::unlimited());
        assert_eq!(
            found.gitdirs,
            vec![ws.join(".git/modules/sub")],
            "{found:?}"
        );
        assert!(found.entries.contains(&ws.join(".git/modules/sub")));
        // The directory holding `.git`, and everything above it, is not "on the way".
        assert!(!found.entries.contains(&ws.join("sub")), "{found:?}");
        assert!(!found.entries.contains(&ws), "{found:?}");
    }

    #[test]
    fn names_are_case_insensitive_only_on_macos() {
        let inside = within(Path::new("/Work/Repo/x"), Path::new("/work/repo"));
        assert_eq!(inside, cfg!(target_os = "macos"));
        assert!(within(Path::new("/work/repo/x"), Path::new("/work/repo")));
        assert!(!within(
            Path::new("/work/repository"),
            Path::new("/work/repo")
        ));
    }

    /// A workspace whose `.git` goes through three chains of `links` long
    /// links each (see [`long_chain`]): `.git` to a gitfile naming the second
    /// chain, to a gitdir whose `commondir` names the third.
    fn chained(links: usize) -> (tempfile::TempDir, PathBuf) {
        let (dir, ws) = workspace();
        mkdirs(&ws, "gd");
        mkdirs(&ws, "common");
        long_chain(&ws, "a", links - 1, "gitfile");
        let gitfile = format!("gitdir: {}\n", ws.join("b0").display());
        std::fs::write(ws.join("gitfile"), gitfile).unwrap();
        long_chain(&ws, "b", links, "gd");
        std::fs::write(ws.join("gd/commondir"), "../c0\n").unwrap();
        long_chain(&ws, "c", links, "common");
        symlink("a0", ws.join(".git")).unwrap();
        (dir, ws)
    }

    /// How long [`linked_gitdirs`] takes on `ws` at best, of three tries, on
    /// a thread that must finish within 10 seconds.
    fn fastest(ws: &Path) -> Duration {
        let dir = ws.to_path_buf();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let times = (0..3).map(|_| {
                let started = Instant::now();
                let found = linked_gitdirs(&dir);
                assert_eq!(found.gitdirs, vec![dir.join("gd"), dir.join("common")]);
                started.elapsed()
            });
            let _ = tx.send(times.min().unwrap());
        });
        rx.recv_timeout(Duration::from_secs(10))
            .expect("took longer than 10s")
    }

    #[test]
    fn the_entries_on_the_way_are_deduplicated_in_linear_time() {
        // About 2,600 and 10,500 entries on the way: four times as many take
        // about four times as long, not sixteen. (A ratio, so it holds on a
        // machine of any speed.)
        let (_s, small) = chained(10);
        let (_l, large) = chained(39);
        assert!(linked_gitdirs(&large).entries.len() > 10_000);
        let (small, large) = (fastest(&small), fastest(&large));
        assert!(large < small * 8, "{large:?} against {small:?}");
    }
}
