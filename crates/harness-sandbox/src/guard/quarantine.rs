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
//! The quarantine directory is trusted: outside the workspace, private to
//! the user, and canonical when harness started. It is still reached without
//! following a symlink, so one swapped in since is not used.
//!
//! A repository in quarantine must not be one git would use, so every `.git`
//! is stored as `dot-git`: in the path an entry is stored at, and below a
//! directory moved there.

#[cfg(test)]
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::io::{self, Read};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use super::nofollow::{Dir, Kind, Stat, Tree, create_dirs, stat_file};

/// A copy across filesystems stops (and the entry is renamed in place
/// instead) after this many entries,
const MAX_COPY_ENTRIES: usize = 100_000;
/// this many bytes,
const MAX_COPY_BYTES: u64 = 256 << 20;
/// or this many directories deep.
const MAX_COPY_DEPTH: usize = 32;

/// How many directories in the quarantine are kept open.
const MAX_MADE: usize = 32;

/// One command's quarantine directory, `<root>/<UTC time>-<pid>-<n>`,
/// created (private to the user) the first time something is moved into it.
#[derive(Debug)]
pub(crate) struct Quarantine {
    root: PathBuf,
    tree: Tree,
    dir: Option<(PathBuf, Dir)>,
    /// For each path an entry was stored at, the suffix to try next, so a
    /// name taken again and again is not searched for a free one.
    next: HashMap<PathBuf, u64>,
    /// Directories made in this command's directory, by their path in it.
    made: HashMap<PathBuf, Dir>,
    /// The workspace directory the last entry was taken from, until
    /// [`forget_sources`](Self::forget_sources): entries taken one after the
    /// other from one directory are reached without walking to it each time.
    source: Option<(PathBuf, Dir)>,
    /// Names tried in the quarantine.
    #[cfg(test)]
    probes: usize,
    /// Paths whose moves fail, for tests.
    #[cfg(test)]
    pub(crate) stuck: BTreeSet<PathBuf>,
}

impl Quarantine {
    pub(crate) fn new(root: &Path, workspace: &Path) -> Quarantine {
        Quarantine {
            root: root.to_path_buf(),
            tree: Tree::new(workspace),
            dir: None,
            next: HashMap::new(),
            made: HashMap::new(),
            source: None,
            #[cfg(test)]
            probes: 0,
            #[cfg(test)]
            stuck: BTreeSet::new(),
        }
    }

    /// Forgets the workspace directory it last took an entry from: a check
    /// begins, and walks to each directory afresh.
    pub(crate) fn forget_sources(&mut self) {
        self.source = None;
    }

    /// Closes every directory it holds open: nothing more will be moved.
    pub(crate) fn close(&mut self) {
        self.source = None;
        self.made.clear();
        self.dir = None;
    }

    /// How many names were tried in the quarantine.
    #[cfg(test)]
    pub(crate) fn probes(&self) -> usize {
        self.probes
    }

    /// Moves `path`, which is below the workspace, into the quarantine
    /// directory at the same path relative to the workspace, and returns
    /// where it went. Across filesystems it is copied and the original
    /// removed; if that fails, it is renamed in place
    /// (`<name>.harness-quarantine-<n>`) so git no longer uses it, and that
    /// path is returned. `NotFound` means nothing is there; any other error
    /// means it was not handled (a symlink on the way, say).
    pub(crate) fn take_stored(&mut self, path: &Path) -> io::Result<Stored> {
        self.take_with(
            path,
            |from, name, to, to_name| from.rename_new(name, to, to_name),
            || {},
        )
    }

    /// Where [`take_stored`](Self::take_stored) moved `path`.
    #[cfg(test)]
    pub(crate) fn take(&mut self, path: &Path) -> io::Result<PathBuf> {
        self.take_stored(path).map(|stored| stored.path)
    }

    /// [`take`](Self::take), with `rename` moving an entry into quarantine
    /// and `after_copy` run between a copy across filesystems and the
    /// removal of the original.
    fn take_with(
        &mut self,
        path: &Path,
        rename: impl Fn(&Dir, &OsStr, &Dir, &OsStr) -> io::Result<()>,
        after_copy: impl FnOnce(),
    ) -> io::Result<Stored> {
        #[cfg(test)]
        if self.stuck.contains(path) {
            return Err(io::Error::other("stuck, for a test"));
        }
        let (parent, name, reused) = self.parent(path)?;
        let (parent, name, found) = match parent.stat(&name) {
            Ok(found) => (parent, name, found),
            // The directory it was taken from last may have been replaced
            // since: walk to it afresh.
            Err(err) if reused && err.kind() == io::ErrorKind::NotFound => {
                self.source = None;
                let (parent, name, _) = self.parent(path)?;
                let found = parent.stat(&name)?;
                (parent, name, found)
            }
            Err(err) => return Err(err),
        };
        let Ok((dest_dir, dest_name, dest)) = self.destination(path) else {
            return in_place(&parent, &name, path);
        };
        match rename(&parent, &name, &dest_dir, &dest_name) {
            Ok(()) => Ok(Stored {
                path: dest,
                live: (found.kind == Kind::Dir)
                    .then(|| neutralize(&dest_dir, &dest_name).err())
                    .flatten(),
            }),
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
            Err(_) => in_place(&parent, &name, path),
        }
    }

    /// The workspace directory `path` is in, its name there, and whether
    /// the directory is the one the last entry was taken from.
    fn parent(&mut self, path: &Path) -> io::Result<(Dir, OsString, bool)> {
        let above = path.parent().unwrap_or(path);
        if let Some((dir_path, dir)) = &self.source
            && dir_path == above
            && let Some(name) = path.file_name()
        {
            return Ok((dir.try_clone()?, name.to_os_string(), true));
        }
        let (dir, name) = self.tree.parent(path)?;
        self.source = Some((above.to_path_buf(), dir.try_clone()?));
        Ok((dir, name, false))
    }

    /// A free name in the quarantine directory for `path`, and the
    /// directory to create it in, with its parents created: at `path`
    /// relative to the workspace, with every `.git` stored as `dot-git`, and
    /// a numbered name (`HEAD.1`) when that is taken.
    fn destination(&mut self, path: &Path) -> io::Result<(Dir, OsString, PathBuf)> {
        let rel = path
            .strip_prefix(self.tree.root())
            .map_err(|_| io::Error::other("outside the workspace"))?;
        let name = stored(
            rel.file_name()
                .ok_or_else(|| io::Error::other("nothing to quarantine"))?,
        );
        let within: PathBuf = rel
            .parent()
            .into_iter()
            .flat_map(Path::iter)
            .map(stored)
            .collect();
        let (dest, dir) = self.made_dir(&within)?;
        let key = dest.join(name);
        let first = self.next.get(&key).copied().unwrap_or(0);
        for n in first..first + 10_000 {
            let mut free = name.to_os_string();
            if n > 0 {
                free.push(format!(".{n}"));
            }
            #[cfg(test)]
            {
                self.probes += 1;
            }
            match dir.stat(&free) {
                Err(err) if err.kind() == io::ErrorKind::NotFound => {
                    self.next.insert(key, n + 1);
                    return Ok((dir, free.clone(), dest.join(free)));
                }
                Err(err) => return Err(err),
                Ok(_) => {}
            }
        }
        Err(io::Error::other("no free name in the quarantine"))
    }

    /// The directory `within` this command's directory, made with its
    /// parents the first time, and its path.
    fn made_dir(&mut self, within: &Path) -> io::Result<(PathBuf, Dir)> {
        let (mut dest, mut dir) = self.command_dir()?;
        if let Some(made) = self.made.get(within) {
            return Ok((dest.join(within), made.try_clone()?));
        }
        for component in within {
            match dir.mkdir(component, 0o700) {
                Ok(()) => dir.open_dir(component)?.set_mode(0o700)?,
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
                Err(err) => return Err(err),
            }
            dir = dir.open_dir(component)?;
            dest.push(component);
        }
        if self.made.len() >= MAX_MADE {
            self.made.clear();
        }
        self.made.insert(within.to_path_buf(), dir.try_clone()?);
        Ok((dest, dir))
    }

    /// This command's directory, created the first time.
    fn command_dir(&mut self) -> io::Result<(PathBuf, Dir)> {
        if let Some((path, dir)) = &self.dir {
            return Ok((path.clone(), dir.try_clone()?));
        }
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let root = create_dirs(&self.root, 0o700)?;
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

/// Where an entry was moved, and, when git could still take something in it
/// for a repository, why.
#[derive(Debug)]
pub(crate) struct Stored {
    pub(crate) path: PathBuf,
    pub(crate) live: Option<String>,
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
    ) -> io::Result<Stored> {
        let mut budget = Budget {
            entries: MAX_COPY_ENTRIES,
            bytes: MAX_COPY_BYTES,
        };
        let copied = match copy(self.from, self.name, self.to, self.to_name, &mut budget, 0) {
            Ok(copied) => copied,
            Err(_) => {
                // The partial copy is harness's own.
                let _ = remove_copy(self.to, self.to_name, 0);
                return in_place(self.from, self.name, path);
            }
        };
        after_copy();
        // The copy stored every `.git` and `HEAD` as it copied.
        let Err(err) = remove_copied(self.from, self.name, &copied) else {
            return Ok(Stored {
                path: dest.to_path_buf(),
                live: None,
            });
        };
        let left = match in_place(self.from, self.name, path) {
            Ok(renamed) => format!(
                "what is left of it was renamed to {}",
                renamed.path.display()
            ),
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
                let copied = copy(&dir, &child, &into, stored(&child), budget, depth + 1)?;
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

/// How `name` is stored in quarantine: `.git` as `dot-git`.
fn stored(name: &OsStr) -> &OsStr {
    if name == ".git" {
        OsStr::new("dot-git")
    } else if name == "HEAD" {
        OsStr::new(STORED_HEAD)
    } else {
        name
    }
}

/// How a `HEAD` is stored: git takes a directory for a repository only
/// with a valid `HEAD` in it.
const STORED_HEAD: &str = "HEAD.quarantined";

/// Renames every `.git` below the directory `name` in `dir` to `dot-git`,
/// and every `HEAD` to `HEAD.quarantined`, so that git takes nothing in it
/// for a repository. Stops after [`MAX_COPY_ENTRIES`] entries or
/// [`MAX_COPY_DEPTH`] levels. Why it could not make sure, if it could not.
fn neutralize(dir: &Dir, name: &OsStr) -> Result<(), String> {
    let inner = dir
        .open_dir(name)
        .map_err(|err| format!("harness could not look inside it ({err})"))?;
    let mut left = MAX_COPY_ENTRIES;
    let mut failure = None;
    neutralize_below(&inner, 0, &mut left, &mut failure);
    failure.map_or(Ok(()), Err)
}

fn neutralize_below(dir: &Dir, depth: usize, left: &mut usize, failure: &mut Option<String>) {
    let too_large =
        "it is too deep or too large for harness to make sure no repository inside it still works";
    if depth > MAX_COPY_DEPTH {
        failure.get_or_insert_with(|| too_large.to_string());
        return;
    }
    let names = match dir.entries() {
        Ok(names) => names,
        Err(err) => {
            failure
                .get_or_insert_with(|| format!("harness could not look inside all of it ({err})"));
            return;
        }
    };
    for mut name in names {
        let Some(fewer) = left.checked_sub(1) else {
            failure.get_or_insert_with(|| too_large.to_string());
            return;
        };
        *left = fewer;
        let renamed_to = if name == ".git" {
            Some("dot-git")
        } else if name == "HEAD" {
            Some(STORED_HEAD)
        } else {
            None
        };
        if let Some(to) = renamed_to {
            match rename_free(dir, &name, to) {
                Ok(renamed) => name = renamed,
                Err(err) => {
                    failure.get_or_insert_with(|| {
                        format!(
                            "a {} inside it could not be renamed ({err}), so a repository inside it may still work",
                            name.display()
                        )
                    });
                    continue;
                }
            }
        }
        if dir.stat(&name).is_ok_and(|stat| stat.kind == Kind::Dir)
            && let Ok(inner) = dir.open_dir(&name)
        {
            neutralize_below(&inner, depth + 1, left, failure);
        }
    }
}

/// Renames `name` in `dir` to `to`, or `<to>.<n>` when that is taken.
fn rename_free(dir: &Dir, name: &OsStr, to: &str) -> io::Result<OsString> {
    for n in 0..100 {
        let mut free = OsString::from(to);
        if n > 0 {
            free.push(format!(".{n}"));
        }
        match dir.rename_new(name, dir, &free) {
            Ok(()) => return Ok(free),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(err),
        }
    }
    Err(io::Error::other("no free name for it"))
}

/// [`rename_in_place`], neutralized as a directory in quarantine is, since
/// git could be run in it all the same.
fn in_place(parent: &Dir, name: &OsStr, path: &Path) -> io::Result<Stored> {
    let renamed = rename_in_place(parent, name, path)?;
    let live = match renamed.file_name() {
        Some(new_name) if parent.stat(new_name).is_ok_and(|s| s.kind == Kind::Dir) => {
            neutralize(parent, new_name).err()
        }
        _ => None,
    };
    Ok(Stored {
        path: renamed,
        live,
    })
}

/// Whether `name` is one [`rename_in_place`] gives: it ends in
/// `.harness-quarantine-<n>`.
pub(crate) fn renamed_in_place(name: &OsStr) -> bool {
    use std::os::unix::ffi::OsStrExt;
    const SUFFIX: &[u8] = b".harness-quarantine-";
    let name = name.as_bytes();
    name.windows(SUFFIX.len())
        .rposition(|part| part == SUFFIX)
        .map(|at| &name[at + SUFFIX.len()..])
        .is_some_and(|n| !n.is_empty() && n.iter().all(u8::is_ascii_digit))
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
        assert!(dest.ends_with("dot-git/hooks/pre-commit"), "{dest:?}");
        assert_eq!(read(&dest), "echo hi\n");
        assert!(!ws.join(".git/hooks/pre-commit").exists());
        let command_dir = std::fs::read_dir(&root)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert!(dest.starts_with(&command_dir));
        for dir in [&command_dir, &command_dir.join("dot-git/hooks")] {
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
        assert!(first.ends_with("HEAD.quarantined"), "{first:?}");
        assert_eq!(second, first.with_file_name("HEAD.quarantined.1"));
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
        let dest = q
            .take_with(&ws.join("sub/.git"), across, || {})
            .unwrap()
            .path;
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
    fn only_names_renamed_in_place_end_in_the_suffix_and_a_number() {
        for name in [
            "post-checkout.harness-quarantine-0",
            ".git.harness-quarantine-12",
            "x.harness-quarantine-0.harness-quarantine-3",
        ] {
            assert!(renamed_in_place(OsStr::new(name)), "{name}");
        }
        for name in [
            "post-checkout",
            "post-checkout.harness-quarantine-",
            "post-checkout.harness-quarantine-1x",
            ".harness-quarantine-x",
            "harness-quarantine-0",
        ] {
            assert!(!renamed_in_place(OsStr::new(name)), "{name}");
        }
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
        let dest = moved.unwrap().path;
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
    fn a_repository_is_stored_as_dot_git_at_any_depth() {
        let (_d, ws, root) = dirs();
        std::fs::create_dir_all(ws.join("sub/.git/hooks")).unwrap();
        std::fs::create_dir_all(ws.join("sub/.git/inner/.git")).unwrap();
        std::fs::write(ws.join("sub/.git/inner/.git/config"), "inner").unwrap();
        std::fs::create_dir_all(ws.join(".git/hooks")).unwrap();
        std::fs::write(ws.join(".git/hooks/x"), "hook").unwrap();
        std::fs::create_dir_all(ws.join("two/.git/inner/.git")).unwrap();
        let mut q = Quarantine::new(&root, &ws);
        let dest = q.take(&ws.join("sub/.git")).unwrap();
        assert!(dest.ends_with("sub/dot-git"), "{dest:?}");
        assert_eq!(read(&dest.join("inner/dot-git/config")), "inner");
        assert!(!exists(&dest.join("inner/.git")));
        let hook = q.take(&ws.join(".git/hooks/x")).unwrap();
        assert!(hook.ends_with("dot-git/hooks/x"), "{hook:?}");
        // Across filesystems as well.
        let copied = q
            .take_with(&ws.join("two/.git"), across, || {})
            .unwrap()
            .path;
        assert!(copied.ends_with("two/dot-git"), "{copied:?}");
        assert!(copied.join("inner/dot-git").is_dir());
        assert!(!exists(&copied.join("inner/.git")));
    }

    #[test]
    fn a_stored_repository_has_no_head_at_any_depth() {
        let (_d, ws, root) = dirs();
        for dir in [
            "sub/.git",
            "sub/.git/inner/.git",
            "sub/.git/modules/m",
            "two/.git",
        ] {
            std::fs::create_dir_all(ws.join(dir).join("refs")).unwrap();
            std::fs::write(ws.join(dir).join("HEAD"), "ref: refs/heads/main\n").unwrap();
        }
        let mut q = Quarantine::new(&root, &ws);
        let stored = q.take_stored(&ws.join("sub/.git")).unwrap();
        assert_eq!(stored.live, None);
        for dir in ["", "inner/dot-git", "modules/m"] {
            let dir = stored.path.join(dir);
            assert!(!exists(&dir.join("HEAD")), "{dir:?}");
            assert_eq!(
                read(&dir.join("HEAD.quarantined")),
                "ref: refs/heads/main\n"
            );
        }
        // Across filesystems as well.
        let copied = q.take_with(&ws.join("two/.git"), across, || {}).unwrap();
        assert!(!exists(&copied.path.join("HEAD")));
        assert_eq!(
            read(&copied.path.join("HEAD.quarantined")),
            "ref: refs/heads/main\n"
        );
    }

    #[test]
    fn what_is_too_deep_to_neutralize_is_reported() {
        let (_d, ws, root) = dirs();
        let mut deep = ws.join("sub/.git");
        for _ in 0..40 {
            deep.push("d");
        }
        std::fs::create_dir_all(deep.join(".git")).unwrap();
        let stored = Quarantine::new(&root, &ws)
            .take_stored(&ws.join("sub/.git"))
            .unwrap();
        let live = stored.live.expect("not fully neutralized");
        assert!(live.contains("too deep or too large"), "{live}");
    }

    #[test]
    fn an_entry_behind_a_directory_replaced_since_the_last_move_is_still_found() {
        let (_d, ws, root) = dirs();
        let hooks = ws.join(".git/hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        std::fs::write(hooks.join("a"), "a").unwrap();
        let mut q = Quarantine::new(&root, &ws);
        q.take(&hooks.join("a")).unwrap();
        // The directory it was taken from is replaced by another.
        std::fs::rename(&hooks, ws.join(".git/hooks-old")).unwrap();
        std::fs::create_dir(&hooks).unwrap();
        std::fs::write(hooks.join("b"), "b").unwrap();
        let moved = q.take(&hooks.join("b")).unwrap();
        assert_eq!(read(&moved), "b");
        assert!(!exists(&hooks.join("b")));
    }

    #[test]
    fn a_free_name_is_found_without_searching() {
        let (_d, ws, root) = dirs();
        let mut q = Quarantine::new(&root, &ws);
        let mut last = PathBuf::new();
        for i in 0..200 {
            std::fs::write(ws.join("HEAD"), format!("{i}")).unwrap();
            last = q.take(&ws.join("HEAD")).unwrap();
        }
        assert_eq!(last.file_name().unwrap(), "HEAD.quarantined.199");
        assert_eq!(read(&last), "199");
        assert!(q.probes() <= 200, "{} names tried", q.probes());
    }

    #[test]
    fn a_quarantine_root_reached_through_a_symlink_is_not_used() {
        // The root was canonical when the session started; then something
        // swapped it for a symlink.
        let (_d, ws, root) = dirs();
        let outside = ws.parent().unwrap().join("outside");
        std::fs::create_dir(&outside).unwrap();
        symlink(&outside, &root).unwrap();
        std::fs::write(ws.join("HEAD"), "ref: x\n").unwrap();
        let moved = Quarantine::new(&root, &ws).take(&ws.join("HEAD")).unwrap();
        assert_eq!(moved, ws.join("HEAD.harness-quarantine-0"));
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
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
            .join("sub/dot-git");
        assert_eq!(read(&copy.join("config")), "copied");
        assert_eq!(read(&copy.join("objects/o")), "object");
        assert_eq!(read(&copy.join("hooks/x")), "hook");
        assert!(
            err.to_string().contains(&copy.display().to_string()),
            "{err}"
        );
    }
}
