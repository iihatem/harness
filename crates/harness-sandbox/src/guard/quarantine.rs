//! Moves protected git metadata out of the workspace without deleting it.
//!
//! What is moved is reached through [`nofollow`](super::nofollow), so a
//! directory swapped for a symlink on the way makes the move fail instead of
//! moving something outside the workspace. Across filesystems an entry is
//! copied, then removed one entry at a time through the same directory
//! descriptors the copy read, and only while each entry is still the one
//! copied: an entry that is new, replaced or written since stops the removal,
//! and what is left is renamed in place so git no longer uses it.
//!
//! The quarantine directory itself is trusted: outside the workspace, and
//! private to the user.

use std::ffi::{OsStr, OsString};
use std::io::{self, Read};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use super::nofollow::{Dir, Kind, Stat, Tree, stat_file};

/// A copy across filesystems stops (and the entry is renamed in place
/// instead) after this many entries,
const MAX_COPY_ENTRIES: usize = 100_000;
/// this many bytes,
const MAX_COPY_BYTES: u64 = 256 << 20;
/// or this many directories deep.
const MAX_COPY_DEPTH: usize = 32;

/// One command's quarantine directory, `<root>/<UTC time>-<pid>-<n>`,
/// created (private to the user) the first time something is moved into it.
#[derive(Debug)]
pub(crate) struct Quarantine {
    root: PathBuf,
    tree: Tree,
    dir: Option<(PathBuf, Dir)>,
}

impl Quarantine {
    pub(crate) fn new(root: &Path, workspace: &Path) -> Quarantine {
        Quarantine {
            root: root.to_path_buf(),
            tree: Tree::new(workspace),
            dir: None,
        }
    }

    /// Moves `path`, which is below the workspace, into the quarantine
    /// directory at the same path relative to the workspace, and returns
    /// where it went. Across filesystems it is copied and the original
    /// removed; if that fails, it is renamed in place
    /// (`<name>.harness-quarantine-<n>`) so git no longer uses it, and that
    /// path is returned. `NotFound` means nothing is there; any other error
    /// means it was not handled (a symlink on the way, say).
    pub(crate) fn take(&mut self, path: &Path) -> io::Result<PathBuf> {
        self.take_with(
            path,
            |from, name, to, to_name| from.rename_new(name, to, to_name),
            || {},
        )
    }

    /// [`take`](Self::take), with `rename` moving an entry into quarantine
    /// and `after_copy` run between a copy across filesystems and the
    /// removal of the original.
    fn take_with(
        &mut self,
        path: &Path,
        rename: impl Fn(&Dir, &OsStr, &Dir, &OsStr) -> io::Result<()>,
        after_copy: impl FnOnce(),
    ) -> io::Result<PathBuf> {
        let (parent, name) = self.tree.parent(path)?;
        parent.stat(&name)?;
        let Ok((dest_dir, dest_name, dest)) = self.destination(path) else {
            return rename_in_place(&parent, &name, path);
        };
        match rename(&parent, &name, &dest_dir, &dest_name) {
            Ok(()) => Ok(dest),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Err(err),
            Err(err) if err.kind() == io::ErrorKind::CrossesDevices => {
                let copy = Copy {
                    from: &parent,
                    name: &name,
                    to: &dest_dir,
                    to_name: &dest_name,
                };
                copy.move_across(&dest, path, after_copy)
            }
            Err(_) => rename_in_place(&parent, &name, path),
        }
    }

    /// A free name in the quarantine directory for `path`, and the
    /// directory to create it in, with its parents created.
    fn destination(&mut self, path: &Path) -> io::Result<(Dir, OsString, PathBuf)> {
        let rel = path
            .strip_prefix(self.tree.root())
            .map_err(|_| io::Error::other("outside the workspace"))?;
        let name = rel
            .file_name()
            .ok_or_else(|| io::Error::other("nothing to quarantine"))?;
        let (mut dest, mut dir) = self.command_dir()?;
        for component in rel.parent().into_iter().flat_map(Path::iter) {
            match dir.mkdir(component, 0o700) {
                Ok(()) => dir.open_dir(component)?.set_mode(0o700)?,
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
                Err(err) => return Err(err),
            }
            dir = dir.open_dir(component)?;
            dest.push(component);
        }
        let mut free = name.to_os_string();
        for n in 1..10_000 {
            match dir.stat(&free) {
                Err(err) if err.kind() == io::ErrorKind::NotFound => {
                    return Ok((dir, free.clone(), dest.join(free)));
                }
                Err(err) => return Err(err),
                Ok(_) => {
                    free = name.to_os_string();
                    free.push(format!(".{n}"));
                }
            }
        }
        Err(io::Error::other("no free name in the quarantine"))
    }

    /// This command's directory, created the first time.
    fn command_dir(&mut self) -> io::Result<(PathBuf, Dir)> {
        if let Some((path, dir)) = &self.dir {
            return Ok((path.clone(), dir.try_clone()?));
        }
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.root)?;
        let root = Dir::open(&self.root)?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        for _ in 0..100 {
            let name = format!(
                "{}-{}-{}",
                utc_stamp(now),
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            );
            let name = OsStr::new(&name);
            match root.mkdir(name, 0o700) {
                Ok(()) => {
                    let dir = root.open_dir(name)?;
                    dir.set_mode(0o700)?;
                    let path = self.root.join(name);
                    self.dir = Some((path.clone(), dir.try_clone()?));
                    return Ok((path, dir));
                }
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
                Err(err) => return Err(err),
            }
        }
        Err(io::Error::other(
            "no free name for the quarantine directory",
        ))
    }
}

/// An entry to move across filesystems: `name` in `from`, to `to_name` in
/// `to`.
struct Copy<'a> {
    from: &'a Dir,
    name: &'a OsStr,
    to: &'a Dir,
    to_name: &'a OsStr,
}

impl Copy<'_> {
    /// Copies the entry to `dest` in quarantine, runs `after_copy`, then
    /// removes what was copied from the workspace. `path` is where the entry
    /// is, for a rename in place.
    fn move_across(
        &self,
        dest: &Path,
        path: &Path,
        after_copy: impl FnOnce(),
    ) -> io::Result<PathBuf> {
        let mut budget = Budget {
            entries: MAX_COPY_ENTRIES,
            bytes: MAX_COPY_BYTES,
        };
        let copied = match copy(self.from, self.name, self.to, self.to_name, &mut budget, 0) {
            Ok(copied) => copied,
            Err(_) => {
                // The partial copy is harness's own.
                let _ = remove_copy(self.to, self.to_name, 0);
                return rename_in_place(self.from, self.name, path);
            }
        };
        after_copy();
        let Err(err) = remove_copied(self.from, self.name, &copied) else {
            return Ok(dest.to_path_buf());
        };
        let left = match rename_in_place(self.from, self.name, path) {
            Ok(renamed) => format!("what is left of it was renamed to {}", renamed.display()),
            Err(why) => format!("what is left of it is still there ({why})"),
        };
        Err(io::Error::other(format!(
            "copied it to {}, but it changed while it was being moved ({err}), so {left}",
            dest.display()
        )))
    }
}

/// What a copy across filesystems may still spend.
struct Budget {
    entries: usize,
    bytes: u64,
}

/// What was copied: the entry as it was when read, and for a directory,
/// each entry in it.
struct Copied {
    stat: Stat,
    children: Vec<(OsString, Copied)>,
}

fn changed() -> io::Error {
    io::Error::other("it changed while it was being copied")
}

/// Copies `name` in `from` to `to_name` in `to`, never following a symlink:
/// directories, regular files (with their permissions) and symlinks.
/// Anything else, or more than the budget, is an error.
fn copy(
    from: &Dir,
    name: &OsStr,
    to: &Dir,
    to_name: &OsStr,
    budget: &mut Budget,
    depth: usize,
) -> io::Result<Copied> {
    if depth > MAX_COPY_DEPTH || budget.entries == 0 {
        return Err(io::Error::other("too large to copy"));
    }
    budget.entries -= 1;
    let stat = from.stat(name)?;
    match stat.kind {
        Kind::Symlink => {
            let target = from.read_link(name)?;
            to.symlink(&target, to_name)?;
            if !from.stat(name)?.unchanged(&stat) {
                return Err(changed());
            }
            Ok(Copied {
                stat,
                children: Vec::new(),
            })
        }
        Kind::File => {
            let (file, opened) = from.open_regular(name)?;
            if !opened.same_entry(&stat) {
                return Err(changed());
            }
            let mut out = to.create_file(to_name, 0o600)?;
            let written = io::copy(&mut (&file).take(budget.bytes + 1), &mut out)?;
            if written > budget.bytes {
                return Err(io::Error::other("too large to copy"));
            }
            budget.bytes -= written;
            out.set_permissions(PermissionsExt::from_mode(opened.mode & 0o777))?;
            let read = stat_file(&file)?;
            if !read.unchanged(&opened) {
                return Err(changed());
            }
            Ok(Copied {
                stat: read,
                children: Vec::new(),
            })
        }
        Kind::Dir => {
            let dir = from.open_dir(name)?;
            let opened = dir.stat_self()?;
            if !opened.same_entry(&stat) {
                return Err(changed());
            }
            to.mkdir(to_name, 0o700)?;
            let into = to.open_dir(to_name)?;
            let mut names = dir.entries()?;
            names.sort();
            let mut children = Vec::with_capacity(names.len());
            for child in names {
                let copied = copy(&dir, &child, &into, &child, budget, depth + 1)?;
                children.push((child, copied));
            }
            into.set_mode((opened.mode & 0o777) | 0o700)?;
            Ok(Copied {
                stat: opened,
                children,
            })
        }
        Kind::Other => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "not a regular file, directory or symlink",
        )),
    }
}

/// Removes what [`copy`] copied from `name` in `parent`, through the
/// descriptors it opens on the way, and only while each entry is still the
/// one copied. The first one that is not stops it.
fn remove_copied(parent: &Dir, name: &OsStr, copied: &Copied) -> io::Result<()> {
    let now = parent.stat(name)?;
    if copied.stat.kind != Kind::Dir {
        if !now.unchanged(&copied.stat) {
            return Err(io::Error::other(format!(
                "{} is not what was copied",
                name.display()
            )));
        }
        return parent.remove(name, false);
    }
    let dir = parent.open_dir(name)?;
    if !now.same_entry(&copied.stat) || !dir.stat_self()?.same_entry(&copied.stat) {
        return Err(io::Error::other(format!(
            "{} is not the directory that was copied",
            name.display()
        )));
    }
    for (child, copied) in &copied.children {
        remove_copied(&dir, child, copied)?;
    }
    // Fails when something new is in it.
    parent.remove(name, true)
}

/// Removes a partial copy harness made in quarantine.
fn remove_copy(dir: &Dir, name: &OsStr, depth: usize) -> io::Result<()> {
    if dir.stat(name)?.kind != Kind::Dir {
        return dir.remove(name, false);
    }
    if depth > MAX_COPY_DEPTH {
        return Err(io::Error::other("too deep"));
    }
    let inner = dir.open_dir(name)?;
    for child in inner.entries()? {
        remove_copy(&inner, &child, depth + 1)?;
    }
    dir.remove(name, true)
}

/// Renames `name` in `parent`, at `path`, to `<name>.harness-quarantine-<n>`
/// next to it, never replacing an entry.
fn rename_in_place(parent: &Dir, name: &OsStr, path: &Path) -> io::Result<PathBuf> {
    for n in 0..100 {
        let mut new_name = name.to_os_string();
        new_name.push(format!(".harness-quarantine-{n}"));
        match parent.rename_new(name, parent, &new_name) {
            Ok(()) => return Ok(path.with_file_name(new_name)),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(err),
        }
    }
    Err(io::Error::other("no free name to rename it to"))
}

/// `secs` since the Unix epoch as a UTC time such as `20260927T143012Z`.
pub(crate) fn utc_stamp(secs: u64) -> String {
    // Howard Hinnant's days-to-civil algorithm.
    let z = (secs / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let rest = secs % 86_400;
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        rest / 3_600,
        rest % 3_600 / 60,
        rest % 60
    )
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;

    fn dirs() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let ws = base.join("ws");
        std::fs::create_dir(&ws).unwrap();
        (dir, ws, base.join("quarantine"))
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    fn exists(path: &Path) -> bool {
        std::fs::symlink_metadata(path).is_ok()
    }

    /// A rename that fails as it does across filesystems.
    fn across(_: &Dir, _: &OsStr, _: &Dir, _: &OsStr) -> io::Result<()> {
        Err(io::Error::from(io::ErrorKind::CrossesDevices))
    }

    /// `run()` on a thread that must finish within 10 seconds.
    fn bounded<T: Send + 'static>(run: impl FnOnce() -> T + Send + 'static) -> T {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(run());
        });
        rx.recv_timeout(Duration::from_secs(10)).expect("blocked")
    }

    #[test]
    fn stamps_are_utc_calendar_times() {
        assert_eq!(utc_stamp(0), "19700101T000000Z");
        assert_eq!(utc_stamp(1_709_164_800), "20240229T000000Z");
        assert_eq!(utc_stamp(1_709_251_199), "20240229T235959Z");
    }

    #[test]
    fn a_moved_entry_keeps_its_path_below_a_private_directory() {
        let (_d, ws, root) = dirs();
        std::fs::create_dir_all(ws.join(".git/hooks")).unwrap();
        std::fs::write(ws.join(".git/hooks/pre-commit"), "echo hi\n").unwrap();
        let mut q = Quarantine::new(&root, &ws);
        let dest = q.take(&ws.join(".git/hooks/pre-commit")).unwrap();
        assert!(dest.ends_with(".git/hooks/pre-commit"), "{dest:?}");
        assert_eq!(read(&dest), "echo hi\n");
        assert!(!ws.join(".git/hooks/pre-commit").exists());
        let command_dir = std::fs::read_dir(&root)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert!(dest.starts_with(&command_dir));
        for dir in [&command_dir, &command_dir.join(".git/hooks")] {
            let mode = std::fs::metadata(dir).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "{dir:?}");
        }
    }

    #[test]
    fn the_same_path_twice_gets_a_numbered_name() {
        let (_d, ws, root) = dirs();
        let mut q = Quarantine::new(&root, &ws);
        std::fs::write(ws.join("HEAD"), "one").unwrap();
        let first = q.take(&ws.join("HEAD")).unwrap();
        std::fs::write(ws.join("HEAD"), "two").unwrap();
        let second = q.take(&ws.join("HEAD")).unwrap();
        assert_eq!(second, first.with_file_name("HEAD.1"));
        assert_eq!(read(&first), "one");
        assert_eq!(read(&second), "two");
    }

    #[test]
    fn a_missing_entry_is_not_found() {
        let (_d, ws, root) = dirs();
        let err = Quarantine::new(&root, &ws)
            .take(&ws.join("HEAD"))
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(!root.exists(), "nothing to quarantine creates no directory");
    }

    #[test]
    fn across_filesystems_a_tree_is_copied_then_removed() {
        let (_d, ws, root) = dirs();
        std::fs::create_dir_all(ws.join("sub/.git/hooks")).unwrap();
        std::fs::write(ws.join("sub/.git/hooks/x"), "hook").unwrap();
        std::fs::set_permissions(
            ws.join("sub/.git/hooks/x"),
            PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        symlink("hooks/x", ws.join("sub/.git/link")).unwrap();
        let mut q = Quarantine::new(&root, &ws);
        let dest = q.take_with(&ws.join("sub/.git"), across, || {}).unwrap();
        assert!(dest.starts_with(&root));
        assert_eq!(read(&dest.join("hooks/x")), "hook");
        let mode = std::fs::metadata(dest.join("hooks/x"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
        assert_eq!(
            std::fs::read_link(dest.join("link")).unwrap(),
            PathBuf::from("hooks/x")
        );
        assert!(!exists(&ws.join("sub/.git")));
        assert!(ws.join("sub").is_dir());
    }

    #[test]
    fn what_cannot_be_copied_is_renamed_in_place_without_replacing_anything() {
        let (_d, ws, root) = dirs();
        std::fs::create_dir_all(ws.join(".git")).unwrap();
        let status = std::process::Command::new("/usr/bin/mkfifo")
            .arg(ws.join(".git/commondir"))
            .status()
            .unwrap();
        assert!(status.success());
        std::fs::write(ws.join(".git/commondir.harness-quarantine-0"), "taken").unwrap();
        let (moved, ws) = bounded(move || {
            let mut q = Quarantine::new(&root, &ws);
            (q.take_with(&ws.join(".git/commondir"), across, || {}), ws)
        });
        let dest = moved.unwrap();
        assert_eq!(dest, ws.join(".git/commondir.harness-quarantine-1"));
        assert!(exists(&dest));
        assert!(!exists(&ws.join(".git/commondir")));
        assert_eq!(
            read(&ws.join(".git/commondir.harness-quarantine-0")),
            "taken"
        );
    }

    #[test]
    fn a_symlink_swapped_into_the_path_makes_the_move_fail_safely() {
        let (_d, ws, root) = dirs();
        let outside = ws.parent().unwrap().join("outside");
        std::fs::create_dir_all(outside.join("sub/.git/hooks")).unwrap();
        std::fs::write(outside.join("sub/.git/hooks/x"), "outside").unwrap();
        std::fs::create_dir_all(ws.join("sub/.git/hooks")).unwrap();
        std::fs::write(ws.join("sub/.git/hooks/x"), "planted").unwrap();
        // The guard decided to move `sub/.git`; then the command swaps `sub`
        // for a symlink to a directory outside the workspace.
        std::fs::rename(ws.join("sub"), ws.join("sub-real")).unwrap();
        symlink(outside.join("sub"), ws.join("sub")).unwrap();
        let mut q = Quarantine::new(&root, &ws);
        let err = q.take(&ws.join("sub/.git")).unwrap_err();
        assert_ne!(err.kind(), io::ErrorKind::NotFound, "{err}");
        assert_eq!(read(&outside.join("sub/.git/hooks/x")), "outside");
        assert_eq!(read(&ws.join("sub-real/.git/hooks/x")), "planted");
        assert!(!root.exists() || std::fs::read_dir(&root).unwrap().next().is_none());
        // The same across filesystems, where the move copies and removes.
        let err = q
            .take_with(&ws.join("sub/.git"), across, || {})
            .unwrap_err();
        assert_ne!(err.kind(), io::ErrorKind::NotFound, "{err}");
        assert_eq!(read(&outside.join("sub/.git/hooks/x")), "outside");
    }

    #[test]
    fn across_filesystems_nothing_that_was_not_copied_is_removed() {
        let (_d, ws, root) = dirs();
        let outside = ws.parent().unwrap().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("x"), "outside").unwrap();
        let git = ws.join("sub/.git");
        std::fs::create_dir_all(git.join("hooks")).unwrap();
        std::fs::create_dir_all(git.join("objects")).unwrap();
        std::fs::write(git.join("hooks/x"), "hook").unwrap();
        std::fs::write(git.join("config"), "copied").unwrap();
        std::fs::write(git.join("objects/o"), "object").unwrap();
        let mut q = Quarantine::new(&root, &ws);
        let changing = git.clone();
        let elsewhere = outside.clone();
        let err = q
            .take_with(&git, across, move || {
                // A process the command left running, between the copy and
                // the removal.
                let git = changing;
                std::fs::write(git.join("new"), "not copied").unwrap();
                std::fs::remove_file(git.join("config")).unwrap();
                std::fs::write(git.join("config"), "replaced").unwrap();
                std::fs::write(git.join("objects/o"), "rewritten").unwrap();
                std::fs::rename(git.join("hooks"), git.join("hooks-moved")).unwrap();
                symlink(elsewhere, git.join("hooks")).unwrap();
            })
            .unwrap_err();
        assert_ne!(err.kind(), io::ErrorKind::NotFound, "{err}");
        // What was left was renamed in place, so git no longer uses it.
        assert!(!exists(&git));
        let left = ws.join("sub/.git.harness-quarantine-0");
        assert_eq!(read(&left.join("new")), "not copied");
        assert_eq!(read(&left.join("config")), "replaced");
        assert_eq!(read(&left.join("objects/o")), "rewritten");
        assert_eq!(read(&left.join("hooks-moved/x")), "hook");
        assert_eq!(read(&outside.join("x")), "outside");
        // The copy holds what was there before.
        let copy = std::fs::read_dir(&root)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
            .join("sub/.git");
        assert_eq!(read(&copy.join("config")), "copied");
        assert_eq!(read(&copy.join("objects/o")), "object");
        assert_eq!(read(&copy.join("hooks/x")), "hook");
        assert!(
            err.to_string().contains(&copy.display().to_string()),
            "{err}"
        );
    }
}
