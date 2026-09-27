//! What protected git metadata looked like before a command, so that changes
//! to it can be found and undone afterwards.
//!
//! Everything is read and written through [`nofollow`](super::nofollow): an
//! entry behind a directory that was swapped for a symlink counts as
//! missing, and is never read or restored through it. One behind a directory
//! harness may not read is unreachable: it may be there all the same.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::nofollow::{Dir, Kind, Stat, Tree, absent, same_birth, stat_file};

/// Larger files are compared by size, times and inode, and are not saved.
const MAX_FILE_BYTES: u64 = 1 << 20;
/// Once this many bytes are saved, later files are only compared.
const MAX_TOTAL_BYTES: u64 = 16 << 20;
/// Entries deeper than this below a protected entry are not recorded.
const MAX_DEPTH: usize = 16;
/// At most this many entries are recorded.
const MAX_ENTRIES: usize = 20_000;

/// One recorded entry.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Node {
    Dir {
        mode: u32,
        /// Whether every entry in it was recorded too: only then is an entry
        /// that was not new.
        whole: bool,
    },
    File {
        mode: u32,
        content: Content,
    },
    Symlink {
        target: PathBuf,
    },
    /// A FIFO, socket or device node: compared by inode (and birth time),
    /// never opened.
    Other {
        dev: u64,
        ino: u64,
        birth: Option<(i64, i64)>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Content {
    Saved(Vec<u8>),
    /// Too large to save: compared by size, times and inode.
    Unsaved(Stamp),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Stamp {
    len: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
    ino: u64,
    birth: Option<(i64, i64)>,
}

impl Stamp {
    fn of(stat: &Stat) -> Stamp {
        Stamp {
            len: stat.size,
            mtime: stat.mtime,
            ctime: stat.ctime,
            ino: stat.ino,
            birth: stat.birth,
        }
    }

    /// Whether `stat` is of the file stamped, not written since.
    fn matches(&self, stat: &Stat) -> bool {
        self.len == stat.size
            && self.mtime == stat.mtime
            && self.ctime == stat.ctime
            && self.ino == stat.ino
            && same_birth(self.birth, stat.birth)
    }
}

/// How an entry differs from its recorded state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Difference {
    /// Something else is there now.
    Changed,
    /// Still a directory, with other permissions.
    Permissions,
    /// Nothing is there now, or nothing reachable without a symlink.
    Missing,
    /// It cannot be looked at (a directory above it cannot be read, say), so
    /// nothing below it is looked at either.
    Unreachable,
    /// New in a recorded directory.
    Added,
}

/// What a [`Snapshot::walk`] does after it has handed an entry on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Next {
    Go,
    /// Look at the entry again: it was unreachable, and may not be now.
    Again,
}

/// The recorded entries, parents before children.
#[derive(Debug, Default)]
pub(crate) struct Snapshot {
    nodes: BTreeMap<PathBuf, Node>,
    saved_bytes: u64,
}

impl Snapshot {
    /// Records `roots` (below `tree`) and everything below them, without
    /// following symlinks. With `everything`, every entry is recorded and
    /// regular files' bytes are saved (up to the size limits). Without it,
    /// only what a read-only mount cannot protect is: symlinks, which cannot
    /// be mounted over, and regular files with more than one hard link,
    /// which can be written through another name.
    ///
    /// What `leave_out` picks is not recorded, and so counts as new in a
    /// directory recorded whole.
    pub(crate) fn take(
        tree: &Tree,
        roots: &[PathBuf],
        everything: bool,
        leave_out: impl Fn(&Path) -> bool,
    ) -> Snapshot {
        let mut snapshot = Snapshot::default();
        for root in roots {
            if let Ok((parent, name)) = tree.parent(root) {
                snapshot.record(&parent, &name, root, 0, everything, &leave_out);
            }
        }
        snapshot
    }

    /// Records `name` in `parent`, at `path`. Whether it was recorded (or
    /// left out on purpose).
    fn record(
        &mut self,
        parent: &Dir,
        name: &OsStr,
        path: &Path,
        depth: usize,
        everything: bool,
        leave_out: &dyn Fn(&Path) -> bool,
    ) -> bool {
        if leave_out(path) {
            return true;
        }
        if depth > MAX_DEPTH || self.nodes.len() >= MAX_ENTRIES {
            return false;
        }
        let Ok(stat) = parent.stat(name) else {
            return false;
        };
        let node = match stat.kind {
            Kind::File if everything || stat.nlink > 1 => {
                let content = match self.save(parent, name, &stat) {
                    Some(bytes) => Content::Saved(bytes),
                    None => Content::Unsaved(Stamp::of(&stat)),
                };
                Node::File {
                    mode: stat.mode,
                    content,
                }
            }
            Kind::Dir => {
                return self.record_dir(parent, name, path, &stat, depth, everything, leave_out);
            }
            Kind::Symlink => match parent.read_link(name) {
                Ok(target) => Node::Symlink { target },
                Err(_) => return false,
            },
            Kind::Other if everything => Node::Other {
                dev: stat.dev,
                ino: stat.ino,
                birth: stat.birth,
            },
            Kind::File | Kind::Other => return false,
        };
        self.nodes.insert(path.to_path_buf(), node);
        true
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the walk's state, passed down as it goes"
    )]
    fn record_dir(
        &mut self,
        parent: &Dir,
        name: &OsStr,
        path: &Path,
        stat: &Stat,
        depth: usize,
        everything: bool,
        leave_out: &dyn Fn(&Path) -> bool,
    ) -> bool {
        if everything {
            let node = Node::Dir {
                mode: stat.mode,
                whole: false,
            };
            self.nodes.insert(path.to_path_buf(), node);
        }
        let listed = parent.open_dir(name).and_then(|dir| {
            if dir.stat_self()?.same_entry(stat) {
                Ok((dir.entries()?, dir))
            } else {
                Err(io::Error::other("replaced while it was read"))
            }
        });
        let whole = match listed {
            Ok((mut names, dir)) => {
                names.sort();
                let mut whole = true;
                for child in names {
                    let child_path = path.join(&child);
                    whole &=
                        self.record(&dir, &child, &child_path, depth + 1, everything, leave_out);
                }
                whole
            }
            Err(_) => false,
        };
        if let Some(Node::Dir {
            whole: recorded, ..
        }) = self.nodes.get_mut(path)
        {
            *recorded = whole;
        }
        everything
    }

    /// The bytes of the regular file `name` in `parent`, when they fit.
    fn save(&mut self, parent: &Dir, name: &OsStr, stat: &Stat) -> Option<Vec<u8>> {
        if stat.size > MAX_FILE_BYTES || self.saved_bytes + stat.size > MAX_TOTAL_BYTES {
            return None;
        }
        let bytes = read_regular(parent, name, stat, MAX_FILE_BYTES)?;
        let len = bytes.len() as u64;
        if self.saved_bytes + len > MAX_TOTAL_BYTES {
            return None;
        }
        self.saved_bytes += len;
        Some(bytes)
    }

    /// Every recorded directory: where a watcher should look for changes.
    pub(crate) fn dirs(&self) -> impl Iterator<Item = &Path> {
        self.nodes
            .iter()
            .filter(|(_, node)| matches!(node, Node::Dir { .. }))
            .map(|(path, _)| path.as_path())
    }

    /// Whether `path` is inside a recorded directory.
    pub(crate) fn covers(&self, path: &Path) -> bool {
        path.ancestors()
            .skip(1)
            .any(|dir| matches!(self.nodes.get(dir), Some(Node::Dir { .. })))
    }

    /// Takes `from`'s record of `path` and everything below it in place of
    /// this one's: an earlier version, kept while it is still to be put
    /// back.
    pub(crate) fn adopt(&mut self, from: &Snapshot, path: &Path) {
        let below = |(recorded, _): &(&PathBuf, &Node)| recorded.starts_with(path);
        let mine: Vec<PathBuf> = self
            .nodes
            .range(path.to_path_buf()..)
            .take_while(below)
            .map(|(recorded, _)| recorded.clone())
            .collect();
        for recorded in mine {
            self.nodes.remove(&recorded);
        }
        for (recorded, node) in from.nodes.range(path.to_path_buf()..).take_while(below) {
            self.nodes.insert(recorded.clone(), node.clone());
        }
    }

    /// How the entry at `path` differs from its record: `None` when it is
    /// not recorded, `Some(None)` when it is as recorded.
    pub(crate) fn state_of(&self, tree: &Tree, path: &Path) -> Option<Option<Difference>> {
        self.nodes
            .get(path)
            .map(|node| difference(tree, path, node))
    }

    /// Whether `path` is a regular file whose bytes were saved.
    pub(crate) fn saved(&self, path: &Path) -> bool {
        matches!(
            self.nodes.get(path),
            Some(Node::File {
                content: Content::Saved(_),
                ..
            })
        )
    }

    /// Calls `act` for each difference from the recorded state, parents
    /// before children, except below the paths `skip` picks. Each entry is
    /// looked at when its turn comes, after `act` has undone what came
    /// before it: a directory whose permissions `act` put back is looked
    /// into.
    ///
    /// For an entry it cannot reach, `act` may make it reachable (give a
    /// directory above it its permissions back) and answer [`Next::Again`]:
    /// the entry is then looked at once more.
    pub(crate) fn walk(
        &self,
        tree: &Tree,
        skip: impl Fn(&Path) -> bool,
        mut act: impl FnMut(&Path, Difference) -> Next,
    ) {
        let mut unreachable: Vec<&Path> = Vec::new();
        for (path, node) in &self.nodes {
            if skip(path) || unreachable.iter().any(|above| path.starts_with(above)) {
                continue;
            }
            let mut found = difference(tree, path, node);
            if let Some(first) = found
                && act(path, first) == Next::Again
            {
                found = difference(tree, path, node);
                if let Some(second) = found {
                    act(path, second);
                }
            }
            if found == Some(Difference::Unreachable) {
                unreachable.push(path);
                continue;
            }
            if let Node::Dir { whole: true, .. } = node {
                for added in self.added(tree, path) {
                    if !skip(&added) {
                        act(&added, Difference::Added);
                    }
                }
            }
        }
    }

    /// What differs from the recorded state now, parents before children.
    #[cfg(test)]
    pub(crate) fn differences(&self, tree: &Tree) -> Vec<(PathBuf, Difference)> {
        let mut found = Vec::new();
        self.walk(
            tree,
            |_| false,
            |path, difference| {
                found.push((path.to_path_buf(), difference));
                Next::Go
            },
        );
        found
    }

    /// The entries in the recorded directory `dir` that were not recorded.
    fn added(&self, tree: &Tree, dir: &Path) -> Vec<PathBuf> {
        let Ok(names) = tree.dir(dir).and_then(|dir| dir.entries()) else {
            return Vec::new();
        };
        let mut added: Vec<PathBuf> = names
            .into_iter()
            .map(|name| dir.join(name))
            .filter(|path| !self.nodes.contains_key(path))
            .collect();
        added.sort();
        added
    }

    /// Puts the recorded entry back at `path`, where nothing is now (or, for
    /// a directory, a directory with the wrong permissions is). It never
    /// replaces an entry: whatever is there is left, and it fails.
    pub(crate) fn restore(&self, tree: &Tree, path: &Path) -> io::Result<()> {
        let Some(node) = self.nodes.get(path) else {
            return Err(io::Error::from(io::ErrorKind::NotFound));
        };
        let (parent, name) = tree.parent(path)?;
        match node {
            Node::Dir { mode, .. } => {
                match parent.stat(&name) {
                    Ok(stat) if stat.kind == Kind::Dir => {}
                    Ok(_) => return Err(io::Error::from(io::ErrorKind::AlreadyExists)),
                    Err(err) if err.kind() == io::ErrorKind::NotFound => {
                        parent.mkdir(&name, 0o700)?;
                    }
                    Err(err) => return Err(err),
                }
                parent.chmod(&name, *mode)
            }
            Node::File {
                mode,
                content: Content::Saved(bytes),
            } => replace(&parent, &name, bytes, *mode),
            Node::Symlink { target } => parent.symlink(target, &name),
            Node::File {
                content: Content::Unsaved(_),
                ..
            } => Err(io::Error::other(
                "it was too large to save before the command, so it cannot be restored",
            )),
            Node::Other { .. } => Err(io::Error::other(
                "it was not a regular file, directory or symlink, so it cannot be restored",
            )),
        }
    }
}

/// How the entry at `path` differs from `node`, if it does.
fn difference(tree: &Tree, path: &Path, node: &Node) -> Option<Difference> {
    let found = tree.parent(path).and_then(|(parent, name)| {
        let stat = parent.stat(&name)?;
        Ok((parent, name, stat))
    });
    let (parent, name, stat) = match found {
        Ok(found) => found,
        Err(err) if absent(&err) => return Some(Difference::Missing),
        Err(_) => return Some(Difference::Unreachable),
    };
    if unchanged(&parent, &name, &stat, node) {
        return None;
    }
    Some(match node {
        Node::Dir { .. } if stat.kind == Kind::Dir => Difference::Permissions,
        _ => Difference::Changed,
    })
}

/// Whether `name` in `parent` (with `stat`) still matches `node`.
fn unchanged(parent: &Dir, name: &OsStr, stat: &Stat, node: &Node) -> bool {
    match node {
        Node::Dir { mode, .. } => stat.kind == Kind::Dir && stat.mode == *mode,
        Node::File { mode, content } => {
            stat.kind == Kind::File
                && stat.mode == *mode
                && match content {
                    Content::Saved(bytes) => {
                        stat.size == bytes.len() as u64
                            && read_regular(parent, name, stat, stat.size).as_ref() == Some(bytes)
                    }
                    Content::Unsaved(stamp) => stamp.matches(stat),
                }
        }
        Node::Symlink { target } => {
            stat.kind == Kind::Symlink && parent.read_link(name).is_ok_and(|t| t == *target)
        }
        Node::Other { dev, ino, birth } => {
            stat.kind == Kind::Other
                && stat.dev == *dev
                && stat.ino == *ino
                && same_birth(stat.birth, *birth)
        }
    }
}

/// The bytes of the regular file `name` in `parent`, which `stat` was taken
/// of, when it has at most `limit` of them. Never follows a symlink, and
/// never blocks on a FIFO swapped in.
fn read_regular(parent: &Dir, name: &OsStr, stat: &Stat, limit: u64) -> Option<Vec<u8>> {
    let (file, opened) = parent.open_regular(name).ok()?;
    if !opened.same_entry(stat) {
        return None;
    }
    let mut bytes = Vec::new();
    (&file).take(limit + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() as u64 <= limit).then_some(bytes)
}

/// Writes `bytes` with `mode` to a new file next to `name` in `parent`, then
/// renames it to `name`, where nothing may be.
fn replace(parent: &Dir, name: &OsStr, bytes: &[u8], mode: u32) -> io::Result<()> {
    let (temp, mut file) = create_temp(parent)?;
    let created = stat_file(&file)?;
    let renamed = file
        .write_all(bytes)
        .and_then(|()| file.set_permissions(PermissionsExt::from_mode(mode)))
        .and_then(|()| parent.rename_new(&temp, parent, name));
    if renamed.is_err() {
        // Only the file created here.
        if parent.stat(&temp).is_ok_and(|now| now.same_entry(&created)) {
            let _ = parent.remove(&temp, false);
        }
    }
    renamed
}

/// A new file in `parent`, under a name nothing else uses.
fn create_temp(parent: &Dir) -> io::Result<(OsString, File)> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    for _ in 0..100 {
        let temp = OsString::from(format!(
            ".harness-restore-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        match parent.create_file(&temp, 0o600) {
            Ok(file) => return Ok((temp, file)),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(err),
        }
    }
    Err(io::Error::other("no free name for a temporary file"))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;

    fn gitdir() -> (tempfile::TempDir, Tree, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let git = base.join(".git");
        std::fs::create_dir_all(git.join("hooks")).unwrap();
        std::fs::write(git.join("config"), "[core]\n").unwrap();
        std::fs::write(git.join("hooks/pre-commit"), "exit 0\n").unwrap();
        (dir, Tree::new(&base), git)
    }

    fn roots(git: &Path) -> Vec<PathBuf> {
        vec![git.join("config"), git.join("hooks")]
    }

    fn mkfifo(path: &Path) {
        let status = std::process::Command::new("/usr/bin/mkfifo")
            .arg(path)
            .status()
            .unwrap();
        assert!(status.success());
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
    fn nothing_changed_means_no_differences() {
        let (_d, tree, git) = gitdir();
        let snapshot = Snapshot::take(&tree, &roots(&git), true, |_| false);
        assert!(snapshot.differences(&tree).is_empty());
    }

    #[test]
    fn changes_deletions_and_additions_are_found_and_undone() {
        let (_d, tree, git) = gitdir();
        let snapshot = Snapshot::take(&tree, &roots(&git), true, |_| false);
        std::fs::write(git.join("config"), "[core]\n\thooksPath = /tmp\n").unwrap();
        std::fs::remove_file(git.join("hooks/pre-commit")).unwrap();
        std::fs::write(git.join("hooks/post-checkout"), "evil\n").unwrap();
        assert_eq!(
            snapshot.differences(&tree),
            vec![
                (git.join("config"), Difference::Changed),
                (git.join("hooks/post-checkout"), Difference::Added),
                (git.join("hooks/pre-commit"), Difference::Missing),
            ]
        );
        std::fs::remove_file(git.join("config")).unwrap();
        snapshot.restore(&tree, &git.join("config")).unwrap();
        snapshot
            .restore(&tree, &git.join("hooks/pre-commit"))
            .unwrap();
        std::fs::remove_file(git.join("hooks/post-checkout")).unwrap();
        assert_eq!(
            std::fs::read_to_string(git.join("config")).unwrap(),
            "[core]\n"
        );
        assert!(snapshot.differences(&tree).is_empty());
        let mut names: Vec<_> = std::fs::read_dir(&git)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        names.sort();
        assert_eq!(names, ["config", "hooks"], "no temporary file is left");
    }

    #[test]
    fn a_directory_replaced_by_a_file_is_changed() {
        let (_d, tree, git) = gitdir();
        let snapshot = Snapshot::take(&tree, &roots(&git), true, |_| false);
        std::fs::remove_dir_all(git.join("hooks")).unwrap();
        std::fs::write(git.join("hooks"), "not a directory").unwrap();
        let found = snapshot.differences(&tree);
        assert!(
            found.contains(&(git.join("hooks"), Difference::Changed)),
            "{found:?}"
        );
        std::fs::remove_file(git.join("hooks")).unwrap();
        snapshot.restore(&tree, &git.join("hooks")).unwrap();
        snapshot
            .restore(&tree, &git.join("hooks/pre-commit"))
            .unwrap();
        assert!(snapshot.differences(&tree).is_empty());
    }

    #[test]
    fn a_permission_change_is_a_change() {
        let (_d, tree, git) = gitdir();
        let hook = git.join("hooks/pre-commit");
        std::fs::set_permissions(&hook, PermissionsExt::from_mode(0o644)).unwrap();
        let snapshot = Snapshot::take(&tree, &roots(&git), true, |_| false);
        std::fs::set_permissions(&hook, PermissionsExt::from_mode(0o755)).unwrap();
        assert_eq!(
            snapshot.differences(&tree),
            vec![(hook.clone(), Difference::Changed)]
        );
        std::fs::set_permissions(&hook, PermissionsExt::from_mode(0o644)).unwrap();
        let hooks = git.join("hooks");
        std::fs::set_permissions(&hooks, PermissionsExt::from_mode(0o777)).unwrap();
        assert_eq!(
            snapshot.differences(&tree),
            vec![(hooks.clone(), Difference::Permissions)]
        );
        snapshot.restore(&tree, &hooks).unwrap();
        assert!(snapshot.differences(&tree).is_empty());
    }

    #[test]
    fn without_everything_only_hard_linked_files_and_symlinks_are_recorded() {
        let (_d, tree, git) = gitdir();
        std::fs::hard_link(git.join("config"), git.parent().unwrap().join("alias")).unwrap();
        symlink("../shared-hooks", git.join("hooks/link")).unwrap();
        let snapshot = Snapshot::take(&tree, &roots(&git), false, |_| false);
        std::fs::write(git.join("hooks/pre-commit"), "changed\n").unwrap();
        std::fs::write(git.join("hooks/new"), "added\n").unwrap();
        assert!(
            snapshot.differences(&tree).is_empty(),
            "other hooks are not recorded"
        );
        std::fs::remove_file(git.join("hooks/link")).unwrap();
        symlink("/tmp/evil", git.join("hooks/link")).unwrap();
        assert_eq!(
            snapshot.differences(&tree),
            vec![(git.join("hooks/link"), Difference::Changed)]
        );
        std::fs::remove_file(git.join("hooks/link")).unwrap();
        snapshot.restore(&tree, &git.join("hooks/link")).unwrap();
        std::fs::write(
            git.parent().unwrap().join("alias"),
            "[core]\n\tfsmonitor = x\n",
        )
        .unwrap();
        assert_eq!(
            snapshot.differences(&tree),
            vec![(git.join("config"), Difference::Changed)]
        );
        std::fs::remove_file(git.join("config")).unwrap();
        snapshot.restore(&tree, &git.join("config")).unwrap();
        assert_eq!(
            std::fs::read_to_string(git.join("config")).unwrap(),
            "[core]\n"
        );
    }

    #[test]
    fn a_large_file_is_compared_but_cannot_be_restored() {
        let (_d, tree, git) = gitdir();
        let big = vec![b'x'; (MAX_FILE_BYTES + 1) as usize];
        std::fs::write(git.join("hooks/big"), &big).unwrap();
        let snapshot = Snapshot::take(&tree, &roots(&git), true, |_| false);
        std::fs::write(git.join("hooks/big"), b"small").unwrap();
        assert_eq!(
            snapshot.differences(&tree),
            vec![(git.join("hooks/big"), Difference::Changed)]
        );
        std::fs::remove_file(git.join("hooks/big")).unwrap();
        let err = snapshot.restore(&tree, &git.join("hooks/big")).unwrap_err();
        assert!(err.to_string().contains("too large"), "{err}");
    }

    #[test]
    fn a_fifo_swapped_in_is_changed_and_never_blocks() {
        let (_d, tree, git) = gitdir();
        let snapshot = Snapshot::take(&tree, &roots(&git), true, |_| false);
        std::fs::remove_file(git.join("config")).unwrap();
        mkfifo(&git.join("config"));
        let found = bounded(move || snapshot.differences(&tree));
        assert_eq!(found, vec![(git.join("config"), Difference::Changed)]);
    }

    #[test]
    fn a_symlink_is_restored_with_its_target() {
        let (_d, tree, git) = gitdir();
        symlink("../shared-hooks", git.join("hooks/link")).unwrap();
        let snapshot = Snapshot::take(&tree, &roots(&git), true, |_| false);
        std::fs::remove_file(git.join("hooks/link")).unwrap();
        symlink("/tmp/evil", git.join("hooks/link")).unwrap();
        assert_eq!(
            snapshot.differences(&tree),
            vec![(git.join("hooks/link"), Difference::Changed)]
        );
        std::fs::remove_file(git.join("hooks/link")).unwrap();
        snapshot.restore(&tree, &git.join("hooks/link")).unwrap();
        assert_eq!(
            std::fs::read_link(git.join("hooks/link")).unwrap(),
            PathBuf::from("../shared-hooks")
        );
    }

    #[test]
    fn a_directory_swapped_for_a_symlink_is_not_read_or_written_through() {
        let (_d, tree, git) = gitdir();
        let outside = git.parent().unwrap().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("pre-commit"), "outside\n").unwrap();
        std::fs::write(outside.join("post-checkout"), "outside hook\n").unwrap();
        let snapshot = Snapshot::take(&tree, &roots(&git), true, |_| false);
        std::fs::rename(git.join("hooks"), git.join("hooks-old")).unwrap();
        symlink(&outside, git.join("hooks")).unwrap();
        assert_eq!(
            snapshot.differences(&tree),
            vec![
                (git.join("hooks"), Difference::Changed),
                (git.join("hooks/pre-commit"), Difference::Missing),
            ]
        );
        assert!(
            snapshot
                .restore(&tree, &git.join("hooks/pre-commit"))
                .is_err()
        );
        assert!(snapshot.restore(&tree, &git.join("hooks")).is_err());
        assert_eq!(
            std::fs::read_to_string(outside.join("pre-commit")).unwrap(),
            "outside\n"
        );
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 2);
    }

    #[test]
    fn a_restore_never_replaces_what_is_there() {
        let (_d, tree, git) = gitdir();
        let snapshot = Snapshot::take(&tree, &roots(&git), true, |_| false);
        std::fs::write(git.join("config"), "[core]\n\tfsmonitor = x\n").unwrap();
        let err = snapshot.restore(&tree, &git.join("config")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists, "{err}");
        assert_eq!(
            std::fs::read_to_string(git.join("config")).unwrap(),
            "[core]\n\tfsmonitor = x\n"
        );
        assert_eq!(
            std::fs::read_dir(&git).unwrap().count(),
            2,
            "no temporary file is left"
        );
    }

    #[test]
    fn the_walk_acts_as_it_goes() {
        // A directory whose permissions are put back can be looked into
        // again, in the same walk.
        let (_d, tree, git) = gitdir();
        let snapshot = Snapshot::take(&tree, &roots(&git), true, |_| false);
        let hooks = git.join("hooks");
        std::fs::write(hooks.join("post-checkout"), "evil\n").unwrap();
        std::fs::set_permissions(&hooks, PermissionsExt::from_mode(0o000)).unwrap();
        let mut found = Vec::new();
        snapshot.walk(
            &tree,
            |_| false,
            |path, difference| {
                found.push((path.to_path_buf(), difference));
                if difference == Difference::Permissions {
                    snapshot.restore(&tree, path).unwrap();
                }
                Next::Go
            },
        );
        assert_eq!(
            found,
            vec![
                (hooks.clone(), Difference::Permissions),
                (hooks.join("post-checkout"), Difference::Added),
            ]
        );
        let skipped: Vec<_> = {
            let mut found = Vec::new();
            snapshot.walk(
                &tree,
                |path| path.starts_with(&hooks),
                |path, d| {
                    found.push((path.to_path_buf(), d));
                    Next::Go
                },
            );
            found
        };
        assert!(skipped.is_empty(), "{skipped:?}");
    }

    #[test]
    fn an_entry_made_reachable_again_is_looked_at_again() {
        let (_d, tree, git) = gitdir();
        let snapshot = Snapshot::take(&tree, &roots(&git), true, |_| false);
        std::fs::write(git.join("hooks/post-checkout"), "evil\n").unwrap();
        std::fs::set_permissions(&git, PermissionsExt::from_mode(0o000)).unwrap();
        let mut found = Vec::new();
        snapshot.walk(
            &tree,
            |_| false,
            |path, difference| {
                found.push((path.to_path_buf(), difference));
                if difference == Difference::Unreachable {
                    std::fs::set_permissions(&git, PermissionsExt::from_mode(0o755)).unwrap();
                    return Next::Again;
                }
                Next::Go
            },
        );
        std::fs::set_permissions(&git, PermissionsExt::from_mode(0o755)).unwrap();
        // SAFETY: no preconditions.
        let root = unsafe { libc::geteuid() } == 0;
        let mut expected = vec![(git.join("hooks/post-checkout"), Difference::Added)];
        if !root {
            expected.insert(0, (git.join("config"), Difference::Unreachable));
        }
        assert_eq!(found, expected);
    }

    #[test]
    fn what_was_not_recorded_whole_is_not_taken_for_new() {
        let (_d, tree, git) = gitdir();
        let mut deep = git.join("hooks");
        for _ in 0..20 {
            deep.push("d");
        }
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("file"), "deep\n").unwrap();
        let snapshot = Snapshot::take(&tree, &roots(&git), true, |_| false);
        assert!(snapshot.differences(&tree).is_empty());
    }
}
