//! Finds every gitdir in a workspace.
//!
//! The workspace is hostile: a sandboxed command can write any of it, and
//! the Linux guard runs [`discover`] in the harness process around every
//! command. So the walk never follows a symlink, opens nothing but regular
//! files (through [`read_regular`]), and stops after [`MAX_ENTRIES`]
//! directory entries. It reads the ignore rules itself: the `ignore` crate's
//! walker opens ignore files, gitfiles and `commondir` files with a plain
//! `File::open`, which blocks on a FIFO and reads `/dev/zero` forever.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs::FileType;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};

use super::linked::{common_dir, gitdir_at, linked_gitdirs_at, within};
use super::read::{missing, read_regular};

/// Directory entries one [`discover`] reads before it stops. Each gitdir it
/// follows counts as one too.
const MAX_ENTRIES: usize = 200_000;

/// How many directories below `modules/` are searched for submodule gitdirs
/// (a submodule's name can contain `/`).
const MAX_MODULE_DEPTH: usize = 64;

/// The directories of a gitdir that hold git's data and never a gitdir, so
/// they are not searched for submodules: `objects` alone can hold 256.
const DATA_DIRS: [&str; 5] = ["objects", "refs", "logs", "lfs", "info"];

/// The most of one `.gitignore` or `info/exclude` that is read. None of a
/// larger one applies.
const MAX_IGNORE_BYTES: u64 = 1 << 20;

/// The most of all ignore files together that one [`discover`] reads: a
/// bound on the memory and time its matchers take. None of a file past it
/// applies.
const MAX_IGNORE_TOTAL: u64 = 4 << 20;

/// How many nested `.gitignore` files apply to one directory: each is
/// matched against every directory below it. Below a deeper one, none do.
const MAX_NESTED_IGNORES: usize = 32;

/// Where git keeps metadata in one workspace.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitIndex {
    /// Every `.git` entry (directory, gitfile or symlink) in the workspace
    /// outside git-ignored directories, the top-level one included.
    pub dot_gits: BTreeSet<PathBuf>,
    /// Every gitdir inside the workspace: each `.git` directory; the gitdir a
    /// gitfile or symlinked `.git` leads to and the one its `commondir` names;
    /// and in each, the gitdirs of linked worktrees (`worktrees/*`) and
    /// submodules (`modules/**`). A parent sorts before its children.
    pub gitdirs: BTreeSet<PathBuf>,
    /// Entries inside the workspace on the way from a gitfile or symlinked
    /// `.git` to its gitdirs, and from a gitdir to the one its `commondir`
    /// names: each symlink, directory and gitfile.
    pub links: BTreeSet<PathBuf>,
    /// Whether the walk may have missed something, so the sets above are not
    /// the whole workspace: a directory could not be read, a `modules/` tree
    /// went deeper than 64 directories, or the walk stopped after 200,000
    /// directory entries. Also set when an ignore file could not be used (not
    /// a regular file, unreadable, over 1 MiB, over 4 MiB with the others
    /// read, more than 32 nested `.gitignore` files, or a negation the
    /// matcher cannot take as git would); the walk then sees more than git
    /// would, never less: below such a `.gitignore` no rules apply.
    pub incomplete: bool,
}

/// Indexes the canonical `workspace` (as [`Path::canonicalize`] returns it:
/// every path in the index is spelled below it; debug builds assert this).
/// Directories git ignores are not walked, nor is `skip` (harness's
/// quarantine directory, should it be inside the workspace), and symlinks
/// are not followed.
///
/// Ignore rules come from inside the workspace only, and apply where git
/// applies them: in a repository, from the directory that holds its `.git`
/// down. They are each directory's `.gitignore` and the repository's
/// `info/exclude` (when its gitdir is in the workspace), never a
/// `.gitignore` above the workspace or the user's global excludes. See
/// [`GitIndex::incomplete`] for the ignore files that are not used.
pub fn discover(workspace: &Path, skip: Option<&Path>) -> GitIndex {
    discover_with_budget(workspace, skip, MAX_ENTRIES)
}

/// [`discover`], stopping after `budget` directory entries.
pub(crate) fn discover_with_budget(
    workspace: &Path,
    skip: Option<&Path>,
    budget: usize,
) -> GitIndex {
    debug_assert!(
        is_canonical(workspace),
        "discover needs a canonical workspace, not {workspace:?}"
    );
    let mut walk = Walk::new(budget);
    let mut index = GitIndex::default();
    walk_workspace(&mut walk, &mut index, workspace, skip);

    // The gitdirs found lead to more: the one each `commondir` names, and
    // those of linked worktrees and submodules.
    let mut pending: Vec<PathBuf> = index.gitdirs.iter().cloned().collect();
    while let Some(gitdir) = pending.pop() {
        if !walk.take() {
            break;
        }
        let mut visited = Vec::new();
        let common = common_dir(&gitdir, &mut visited);
        // As for a gitfile: the entries on the way, but not the gitdir it
        // starts from or the directories above that.
        index.links.extend(
            visited
                .into_iter()
                .filter(|entry| !gitdir.starts_with(entry) && within(entry, workspace)),
        );
        for found in common.into_iter().chain(walk.nested_gitdirs(&gitdir)) {
            if within(&found, workspace) && index.gitdirs.insert(found.clone()) {
                pending.push(found);
            }
        }
    }
    index.incomplete = walk.incomplete;
    index
}

/// The gitdirs of `gitdir`'s linked worktrees (every directory in
/// `worktrees/`) and submodules (every directory below `modules/` that holds
/// a `HEAD`, down to 64 levels, and not in the [`DATA_DIRS`] of another).
/// Symlinks are not followed.
#[expect(
    dead_code,
    reason = "for the Linux guard's re-check of the gitdirs it knows; drop this once it calls it"
)]
pub(crate) fn nested_gitdirs(gitdir: &Path) -> Vec<PathBuf> {
    Walk::new(MAX_ENTRIES).nested_gitdirs(gitdir)
}

/// Whether `path` is absolute with no symlink, `.` or `..` in it. A path
/// that does not exist cannot be checked, and passes.
fn is_canonical(path: &Path) -> bool {
    path.is_absolute()
        && path
            .canonicalize()
            .ok()
            .is_none_or(|canonical| canonical == path)
}

/// Walks the workspace for `.git` entries, applying the ignore rules as it
/// goes down.
fn walk_workspace(walk: &mut Walk, index: &mut GitIndex, workspace: &Path, skip: Option<&Path>) {
    // `None`: not in a repository, where git applies no ignore rules.
    let mut pending: Vec<(PathBuf, Option<Rules>)> = vec![(workspace.to_path_buf(), None)];
    while let Some((dir, mut rules)) = pending.pop() {
        let Some(entries) = walk.list(&dir) else {
            continue;
        };
        if let Some(dot_git) = entries.iter().find(|entry| entry.name == ".git") {
            let path = dir.join(".git");
            index.dot_gits.insert(path.clone());
            if dot_git.kind.is_dir() {
                index.gitdirs.insert(path);
            } else {
                let linked = linked_gitdirs_at(&dir, workspace);
                index.gitdirs.extend(linked.gitdirs);
                index.links.extend(linked.entries);
            }
            // A repository of its own: git reads it with its own rules, and
            // those of the directories above stop here. Without its
            // `info/exclude`, which only ever ignores, the walk sees more.
            rules = Some(Rules::repository(walk.exclude(&dir, workspace).found()));
        }
        if let Some(current) = &mut rules
            && entries.iter().any(|entry| entry.name == ".gitignore")
        {
            let file = if current.nested == MAX_NESTED_IGNORES {
                walk.unusable()
            } else {
                walk.ignore_file(&dir, &dir.join(".gitignore"))
            };
            match file {
                IgnoreFile::Missing => {}
                IgnoreFile::Found(gitignore) => current.push(gitignore),
                // What it says is unknown, and it could re-include what the
                // rules above ignore: none apply below.
                IgnoreFile::Unusable => rules = None,
            }
        }
        for entry in entries {
            if !entry.kind.is_dir() || entry.name == ".git" {
                continue;
            }
            let path = dir.join(&entry.name);
            if skip == Some(path.as_path()) || rules.as_ref().is_some_and(|r| r.ignores(&path)) {
                continue;
            }
            pending.push((path, rules.clone()));
        }
    }
}

/// One entry of a directory listing: its name, and its type as the listing
/// gives it (a symlink is a symlink).
struct Entry {
    name: OsString,
    kind: FileType,
}

/// An ignore file, as far as the walk can use it.
enum IgnoreFile {
    Missing,
    Found(Gitignore),
    /// There, but not usable as git would use it (see [`Walk::ignore_file`]).
    Unusable,
}

impl IgnoreFile {
    fn found(self) -> Option<Gitignore> {
        match self {
            IgnoreFile::Found(gitignore) => Some(gitignore),
            IgnoreFile::Missing | IgnoreFile::Unusable => None,
        }
    }
}

/// One [`discover`]'s reads: what is left of its budgets, and whether it has
/// missed anything.
struct Walk {
    entries_left: usize,
    exhausted: bool,
    ignore_bytes_left: u64,
    incomplete: bool,
}

impl Walk {
    fn new(budget: usize) -> Walk {
        Walk {
            entries_left: budget,
            exhausted: false,
            ignore_bytes_left: MAX_IGNORE_TOTAL,
            incomplete: false,
        }
    }

    /// Takes one entry from the budget. `false`, and the index is incomplete,
    /// once it is spent.
    fn take(&mut self) -> bool {
        if self.entries_left == 0 {
            self.exhausted = true;
            self.incomplete = true;
        }
        if self.exhausted {
            return false;
        }
        self.entries_left -= 1;
        true
    }

    /// The entries of the directory `dir`. `None`, and the index is
    /// incomplete, when it cannot be read or the budget runs out; an entry
    /// that cannot be read is left out and makes it incomplete too.
    fn list(&mut self, dir: &Path) -> Option<Vec<Entry>> {
        if self.exhausted {
            return None;
        }
        let Ok(read) = std::fs::read_dir(dir) else {
            self.incomplete = true;
            return None;
        };
        let mut entries = Vec::new();
        for entry in read {
            if !self.take() {
                return None;
            }
            match entry.and_then(|entry| {
                Ok(Entry {
                    kind: entry.file_type()?,
                    name: entry.file_name(),
                })
            }) {
                Ok(entry) => entries.push(entry),
                Err(_) => self.incomplete = true,
            }
        }
        Some(entries)
    }

    /// [`Walk::list`] for a directory that need not be there: nothing when
    /// `dir` is missing or is not a directory (a symlink to one included).
    fn list_if_dir(&mut self, dir: &Path) -> Option<Vec<Entry>> {
        match std::fs::symlink_metadata(dir) {
            Ok(meta) if meta.is_dir() => self.list(dir),
            Ok(_) => None,
            Err(err) if missing(&err) => None,
            Err(_) => {
                self.incomplete = true;
                None
            }
        }
    }

    /// See [`nested_gitdirs`].
    fn nested_gitdirs(&mut self, gitdir: &Path) -> Vec<PathBuf> {
        let worktrees = gitdir.join("worktrees");
        let mut found: Vec<PathBuf> = self
            .list_if_dir(&worktrees)
            .unwrap_or_default()
            .into_iter()
            .filter(|entry| entry.kind.is_dir())
            .map(|entry| worktrees.join(entry.name))
            .collect();

        let modules = gitdir.join("modules");
        let mut pending: Vec<(PathBuf, usize)> = self
            .list_if_dir(&modules)
            .unwrap_or_default()
            .into_iter()
            .filter(|entry| entry.kind.is_dir())
            .map(|entry| (modules.join(entry.name), 1))
            .collect();
        while let Some((dir, depth)) = pending.pop() {
            let Some(entries) = self.list(&dir) else {
                continue;
            };
            // A junk `HEAD` does not hide what is below it: the walk goes on,
            // past the gitdir's own data.
            let is_gitdir = entries.iter().any(|entry| entry.name == "HEAD");
            for entry in entries {
                if !entry.kind.is_dir()
                    || (is_gitdir && DATA_DIRS.iter().any(|data| entry.name == *data))
                {
                    continue;
                }
                if depth == MAX_MODULE_DEPTH {
                    self.incomplete = true;
                    continue;
                }
                pending.push((dir.join(entry.name), depth + 1));
            }
            if is_gitdir {
                found.push(dir);
            }
        }
        found
    }

    /// The `info/exclude` of the repository whose `.git` is in `holder`: in
    /// its gitdir, or in the common one its `commondir` names. Missing unless
    /// that is in the workspace.
    fn exclude(&mut self, holder: &Path, workspace: &Path) -> IgnoreFile {
        let Some(gitdir) = gitdir_at(holder) else {
            return IgnoreFile::Missing;
        };
        let common = if std::fs::symlink_metadata(gitdir.join("commondir")).is_ok() {
            match common_dir(&gitdir, &mut Vec::new()) {
                Some(common) => common,
                None => return IgnoreFile::Missing,
            }
        } else {
            gitdir
        };
        if !within(&common, workspace) {
            return IgnoreFile::Missing;
        }
        self.ignore_file(holder, &common.join("info/exclude"))
    }

    /// The ignore file `file`, for the paths below `root`. Unusable, and the
    /// index is incomplete, when it is not a regular file, cannot be read, is
    /// larger than [`MAX_IGNORE_BYTES`] or than what is left of
    /// [`MAX_IGNORE_TOTAL`], or holds a negation git would match differently.
    fn ignore_file(&mut self, root: &Path, file: &Path) -> IgnoreFile {
        let limit = MAX_IGNORE_BYTES.min(self.ignore_bytes_left);
        let bytes = match read_regular(file, limit + 1) {
            Ok(bytes) => bytes,
            Err(err) if missing(&err) => return IgnoreFile::Missing,
            Err(_) => return self.unusable(),
        };
        let read = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        self.ignore_bytes_left = self.ignore_bytes_left.saturating_sub(read);
        if read > limit {
            return self.unusable();
        }
        match gitignore(root, &bytes) {
            Some(gitignore) => IgnoreFile::Found(gitignore),
            None => self.unusable(),
        }
    }

    fn unusable(&mut self) -> IgnoreFile {
        self.incomplete = true;
        IgnoreFile::Unusable
    }
}

/// `bytes`, an ignore file for the paths below `root`, read as git reads it:
/// line by line after a UTF-8 BOM, without the `\r` before a `\n`. `None`
/// when a negation cannot be matched as git would match it (its line is not
/// UTF-8, or not a pattern the matcher takes): without it the walk could skip
/// a directory git reads. An ignore pattern like that is left out, which
/// only makes the walk see more.
fn gitignore(root: &Path, bytes: &[u8]) -> Option<Gitignore> {
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    let mut builder = GitignoreBuilder::new(root);
    for line in bytes.split(|&b| b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let negation = line.starts_with(b"!");
        let added =
            std::str::from_utf8(line).is_ok_and(|line| builder.add_line(None, line).is_ok());
        if !added && negation {
            return None;
        }
    }
    builder.build().ok()
}

/// The ignore rules for what is directly in one directory of a repository:
/// the repository's `info/exclude`, and the `.gitignore` of each directory
/// from the repository's top down to this one.
#[derive(Clone)]
struct Rules {
    /// The deepest `.gitignore` first.
    gitignores: Option<Rc<Chain>>,
    /// How many there are.
    nested: usize,
    exclude: Option<Rc<Gitignore>>,
}

struct Chain {
    gitignore: Gitignore,
    parent: Option<Rc<Chain>>,
}

impl Rules {
    fn repository(exclude: Option<Gitignore>) -> Rules {
        Rules {
            gitignores: None,
            nested: 0,
            exclude: exclude.map(Rc::new),
        }
    }

    fn push(&mut self, gitignore: Gitignore) {
        let parent = self.gitignores.take();
        self.gitignores = Some(Rc::new(Chain { gitignore, parent }));
        self.nested += 1;
    }

    /// Whether git ignores the directory `dir`. The deepest `.gitignore` with
    /// a pattern that matches it decides (its last such pattern, which may
    /// be a negation), and `info/exclude` only when none does.
    fn ignores(&self, dir: &Path) -> bool {
        let mut chain = self.gitignores.as_deref();
        while let Some(link) = chain {
            match link.gitignore.matched(dir, true) {
                Match::None => chain = link.parent.as_deref(),
                decided => return decided.is_ignore(),
            }
        }
        self.exclude
            .as_ref()
            .is_some_and(|exclude| exclude.matched(dir, true).is_ignore())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let canon = dir.path().canonicalize().unwrap();
        (dir, canon)
    }

    #[test]
    fn the_walk_stops_when_its_budget_runs_out() {
        let (_d, ws) = workspace();
        for i in 0..20 {
            std::fs::create_dir_all(ws.join(format!("r{i}/.git"))).unwrap();
        }
        // 20 entries in the workspace, one (`.git`) in each repository, and
        // each of the 20 gitdirs followed.
        let enough = discover_with_budget(&ws, None, 60);
        assert!(!enough.incomplete, "{enough:?}");
        assert_eq!(enough.dot_gits.len(), 20);
        let short = discover_with_budget(&ws, None, 59);
        assert!(short.incomplete, "{short:?}");
        // Spent during the walk, it stops the walk there.
        let shorter = discover_with_budget(&ws, None, 30);
        assert!(shorter.incomplete, "{shorter:?}");
        assert_eq!(shorter.dot_gits.len(), 10, "{shorter:?}");
    }

    #[test]
    fn the_modules_walk_counts_against_the_budget() {
        let (_d, ws) = workspace();
        for i in 0..30 {
            let module = ws.join(format!(".git/modules/m{i}"));
            std::fs::create_dir_all(&module).unwrap();
            std::fs::write(module.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        }
        let enough = discover_with_budget(&ws, None, 1000);
        assert!(!enough.incomplete, "{enough:?}");
        assert_eq!(enough.gitdirs.len(), 31);
        let short = discover_with_budget(&ws, None, 20);
        assert!(short.incomplete, "{short:?}");
        assert!(short.gitdirs.len() < 31, "{short:?}");
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "canonical")]
    fn a_workspace_that_is_not_canonical_is_a_bug() {
        let (_d, ws) = workspace();
        std::fs::create_dir_all(ws.join("sub")).unwrap();
        discover(&ws.join("sub/.."), None);
    }
}
