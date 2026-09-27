//! Finds every gitdir in a workspace.
//!
//! The workspace is hostile: a sandboxed command can write any of it, and
//! the Linux guard runs [`discover`] in the harness process around every
//! command. So the walks never follow a symlink, open nothing but regular
//! files (through [`read_regular`]), and stop after [`MAX_ENTRIES`]
//! directory entries or [`MAX_TIME`].
//!
//! The ignore rules are read once, by [`read_ignore_rules`] when a session
//! starts, and [`discover`] reads no ignore file: a command could otherwise
//! write a rule that hides the repository it makes. They are read by the
//! walk here, not the `ignore` crate's walker, which opens ignore files,
//! gitfiles and `commondir` files with a plain `File::open`: that blocks on
//! a FIFO and reads `/dev/zero` forever.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fmt;
use std::fs::FileType;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};

use super::linked::{common_dir, gitdir_at, linked_gitdirs_at, noting, within};
use super::read::{missing, read_regular};

/// Directory entries one walk reads before it stops. Each gitdir
/// [`discover`] follows counts as one too.
const MAX_ENTRIES: usize = 200_000;

/// How long one walk may take before it stops.
const MAX_TIME: Duration = Duration::from_secs(5);

/// Steps (an entry read, a directory matched against the rules, an ignore
/// file read) between two looks at the clock.
const CLOCK_EVERY: u32 = 64;

/// How many directories below `modules/` are searched for submodule gitdirs
/// (a submodule's name can contain `/`).
const MAX_MODULE_DEPTH: usize = 64;

/// The directories of a gitdir that hold git's data and never a gitdir, so
/// they are not searched for submodules: `objects` alone can hold 256.
const DATA_DIRS: [&str; 5] = ["objects", "refs", "logs", "lfs", "info"];

/// The most of one `.gitignore` or `info/exclude` that is read. None of a
/// larger one applies.
const MAX_IGNORE_BYTES: u64 = 1 << 20;

/// The most of all ignore files together that one reading of the rules
/// reads: a bound on the memory and time its matchers take. None of a file
/// past it applies.
const MAX_IGNORE_TOTAL: u64 = 4 << 20;

/// How many nested `.gitignore` files apply to one directory: each is
/// matched against every directory below it. Below a deeper one, none do.
const MAX_NESTED_IGNORES: usize = 32;

/// Where git keeps metadata in one workspace.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitIndex {
    /// Every `.git` entry (directory, gitfile or symlink) in the workspace
    /// outside the directories the [`IgnoreRules`] ignore, the top-level one
    /// included.
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
    /// the whole workspace: a directory could not be read; a gitfile or
    /// `commondir` file is there but could not be read; a `modules/` tree
    /// went deeper than 64 directories; the walk stopped after 200,000
    /// directory entries or 5 seconds; or the [`IgnoreRules`] it was given
    /// are incomplete.
    pub incomplete: bool,
}

/// The ignore rules of a workspace, read by [`read_ignore_rules`] when a
/// session starts, for every [`discover`] in it: each directory's
/// `.gitignore` and each repository's `info/exclude`, as they were then.
///
/// The default holds no rules: [`discover`] then walks every directory.
#[derive(Clone, Default)]
pub struct IgnoreRules(Arc<Recorded>);

#[derive(Default)]
struct Recorded {
    /// The `.gitignore` of each directory that held one: `None` when it could
    /// not be used, and no rules apply below it.
    gitignores: BTreeMap<PathBuf, Option<Arc<Gitignore>>>,
    /// The `info/exclude` of each repository, by the directory that holds its
    /// `.git`: its patterns are relative to that directory.
    excludes: BTreeMap<PathBuf, Arc<Gitignore>>,
    /// Every directory that held a `.git`: the rules above stop there.
    repositories: BTreeSet<PathBuf>,
    incomplete: bool,
}

impl IgnoreRules {
    /// Whether an ignore file could not be used: it was not a regular file,
    /// could not be read, was over 1 MiB, over 4 MiB with the others read, or
    /// the 33rd nested `.gitignore`, or held a negation the matcher cannot
    /// take as git would. Also when a directory, gitfile or `commondir` file
    /// could not be read, or the reading stopped after 200,000 directory
    /// entries or 5 seconds, so ignore files may have gone unread. Every
    /// [`GitIndex`] built with these rules is then incomplete: its walk sees
    /// more than git would, never less (below a `.gitignore` that could not
    /// be used, no rules apply).
    pub fn incomplete(&self) -> bool {
        self.0.incomplete
    }
}

impl fmt::Debug for IgnoreRules {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let with = |usable: bool| {
            self.0
                .gitignores
                .iter()
                .filter(move |(_, rules)| rules.is_some() == usable)
                .map(|(dir, _)| dir)
                .collect::<Vec<_>>()
        };
        f.debug_struct("IgnoreRules")
            .field("gitignores", &with(true))
            .field("unusable", &with(false))
            .field("excludes", &self.0.excludes.keys().collect::<Vec<_>>())
            .field("repositories", &self.0.repositories)
            .field("incomplete", &self.0.incomplete)
            .finish()
    }
}

/// Reads the ignore rules of the canonical `workspace` (see [`discover`]),
/// once, when a session starts: rules read after a sandboxed command ran
/// could be ones it wrote.
///
/// They come from inside the workspace only: each directory's `.gitignore`,
/// from the workspace down (whether or not it holds a `.git`), and the
/// `info/exclude` of each repository whose gitdir is in the workspace;
/// never a `.gitignore` above the workspace or the user's global excludes.
/// They apply as git applies them: in each repository from the directory
/// that holds its `.git` down. Directories they ignore are not walked, nor
/// is `skip`, and symlinks are not followed.
pub fn read_ignore_rules(workspace: &Path, skip: Option<&Path>) -> IgnoreRules {
    read_ignore_rules_with_budget(workspace, skip, Budget::DEFAULT)
}

/// [`read_ignore_rules`] within `budget`.
pub(crate) fn read_ignore_rules_with_budget(
    workspace: &Path,
    skip: Option<&Path>,
    budget: Budget,
) -> IgnoreRules {
    debug_assert!(
        is_canonical(workspace),
        "read_ignore_rules needs a canonical workspace, not {workspace:?}"
    );
    let mut walk = Walk::new(budget);
    let mut reading = Reading {
        workspace,
        recorded: Recorded::default(),
    };
    walk_workspace(&mut walk, workspace, skip, &mut reading);
    let mut recorded = reading.recorded;
    recorded.incomplete = walk.incomplete;
    IgnoreRules(Arc::new(recorded))
}

/// Indexes the canonical `workspace` (as [`Path::canonicalize`] returns it:
/// every path in the index is spelled below it; debug builds assert this).
/// `skip` (harness's quarantine directory, should it be inside the
/// workspace) is not walked, and symlinks are not followed.
///
/// No ignore file is read: a directory is skipped only when `rules` ignore
/// it. The `.gitignore` recorded for each directory above it, from the top
/// of its repository (or of the workspace) down, applies as git applies it,
/// then the repository's recorded `info/exclude`. A directory with none
/// recorded (one made since, say) has no rules of its own, whatever it
/// holds now. Rules restart at each directory that holds a `.git` now or
/// held one when they were read, so a change since can only make the walk
/// see more; but a repository made since inside a directory the rules
/// ignore stays hidden.
pub fn discover(workspace: &Path, skip: Option<&Path>, rules: &IgnoreRules) -> GitIndex {
    discover_with_budget(workspace, skip, rules, Budget::DEFAULT)
}

/// [`discover`] within `budget`.
pub(crate) fn discover_with_budget(
    workspace: &Path,
    skip: Option<&Path>,
    rules: &IgnoreRules,
    budget: Budget,
) -> GitIndex {
    debug_assert!(
        is_canonical(workspace),
        "discover needs a canonical workspace, not {workspace:?}"
    );
    let mut walk = Walk::new(budget);
    let mut indexing = Indexing {
        workspace,
        rules: &rules.0,
        index: GitIndex::default(),
    };
    walk_workspace(&mut walk, workspace, skip, &mut indexing);
    let mut index = indexing.index;

    // The gitdirs found lead to more: the one each `commondir` names, and
    // those of linked worktrees and submodules.
    let mut pending: Vec<PathBuf> = index.gitdirs.iter().cloned().collect();
    while let Some(gitdir) = pending.pop() {
        if !walk.take() {
            break;
        }
        let mut visited = Vec::new();
        let common = noting(common_dir(&gitdir, &mut visited), &mut walk.incomplete);
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
    index.incomplete = walk.incomplete || rules.incomplete();
    index
}

/// What one walk may spend.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Budget {
    /// Directory entries read.
    pub(crate) entries: usize,
    /// Time since the walk started.
    pub(crate) time: Duration,
}

impl Budget {
    pub(crate) const DEFAULT: Budget = Budget {
        entries: MAX_ENTRIES,
        time: MAX_TIME,
    };
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
    Walk::new(Budget::DEFAULT).nested_gitdirs(gitdir)
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

/// What a walk of the workspace does in each directory it reaches, and where
/// its ignore rules come from: the files ([`Reading`]) or what an earlier
/// reading recorded ([`Indexing`]).
trait Visit {
    /// `dir` holds a `.git` of the type `kind`.
    fn dot_git(&mut self, walk: &mut Walk, dir: &Path, kind: FileType);

    /// Whether the rules above stop at `dir` though it holds no `.git` now.
    fn was_repository(&self, dir: &Path) -> bool;

    /// The `info/exclude` of the repository whose `.git` is in `dir`.
    fn exclude(&mut self, walk: &mut Walk, dir: &Path) -> Option<Arc<Gitignore>>;

    /// The `.gitignore` of `dir`: `listed` when `dir` holds one now, `full`
    /// when [`MAX_NESTED_IGNORES`] apply to `dir` already.
    fn gitignore(&mut self, walk: &mut Walk, dir: &Path, listed: bool, full: bool) -> IgnoreFile;
}

/// Walks the workspace from the top, applying the ignore rules as it goes
/// down.
fn walk_workspace(walk: &mut Walk, workspace: &Path, skip: Option<&Path>, visit: &mut impl Visit) {
    // Rules start at the workspace, whether or not it holds a `.git`, and
    // never come from above it. `None`: below a `.gitignore` that could not
    // be used, where none apply.
    let mut pending: Vec<(PathBuf, Option<Rules>)> =
        vec![(workspace.to_path_buf(), Some(Rules::start(None)))];
    while let Some((dir, mut rules)) = pending.pop() {
        let Some(entries) = walk.list(&dir) else {
            continue;
        };
        let dot_git = entries
            .iter()
            .find(|entry| entry.name == ".git")
            .map(|entry| entry.kind);
        if let Some(kind) = dot_git {
            visit.dot_git(walk, &dir, kind);
        }
        if dot_git.is_some() || visit.was_repository(&dir) {
            // A repository of its own: git reads it with its own rules, and
            // those of the directories above stop here. Without its
            // `info/exclude`, which only ever ignores, the walk sees more.
            rules = Some(Rules::start(visit.exclude(walk, &dir)));
        }
        if let Some(current) = &mut rules {
            let listed = entries.iter().any(|entry| entry.name == ".gitignore");
            let full = current.nested == MAX_NESTED_IGNORES;
            match visit.gitignore(walk, &dir, listed, full) {
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
            if !walk.tick() {
                break;
            }
            let path = dir.join(&entry.name);
            if skip == Some(path.as_path()) || rules.as_ref().is_some_and(|r| r.ignores(&path)) {
                continue;
            }
            pending.push((path, rules.clone()));
        }
    }
}

/// Reads the ignore files as the walk reaches them, and records them.
struct Reading<'a> {
    workspace: &'a Path,
    recorded: Recorded,
}

impl Visit for Reading<'_> {
    fn dot_git(&mut self, _walk: &mut Walk, dir: &Path, _kind: FileType) {
        self.recorded.repositories.insert(dir.to_path_buf());
    }

    fn was_repository(&self, _dir: &Path) -> bool {
        false
    }

    fn exclude(&mut self, walk: &mut Walk, dir: &Path) -> Option<Arc<Gitignore>> {
        let exclude = walk.exclude(dir, self.workspace).found()?;
        self.recorded
            .excludes
            .insert(dir.to_path_buf(), Arc::clone(&exclude));
        Some(exclude)
    }

    fn gitignore(&mut self, walk: &mut Walk, dir: &Path, listed: bool, full: bool) -> IgnoreFile {
        if !listed {
            return IgnoreFile::Missing;
        }
        let file = if full {
            walk.unusable()
        } else {
            walk.ignore_file(dir, &dir.join(".gitignore"))
        };
        match &file {
            IgnoreFile::Missing => {}
            IgnoreFile::Found(gitignore) => {
                let gitignore = Some(Arc::clone(gitignore));
                self.recorded
                    .gitignores
                    .insert(dir.to_path_buf(), gitignore);
            }
            IgnoreFile::Unusable => {
                self.recorded.gitignores.insert(dir.to_path_buf(), None);
            }
        }
        file
    }
}

/// Builds the index, with the rules an earlier [`Reading`] recorded.
struct Indexing<'a> {
    workspace: &'a Path,
    rules: &'a Recorded,
    index: GitIndex,
}

impl Visit for Indexing<'_> {
    fn dot_git(&mut self, walk: &mut Walk, dir: &Path, kind: FileType) {
        let path = dir.join(".git");
        self.index.dot_gits.insert(path.clone());
        if kind.is_dir() {
            self.index.gitdirs.insert(path);
        } else {
            let linked = linked_gitdirs_at(dir, self.workspace);
            walk.incomplete |= linked.unreadable;
            self.index.gitdirs.extend(linked.gitdirs);
            self.index.links.extend(linked.entries);
        }
    }

    fn was_repository(&self, dir: &Path) -> bool {
        self.rules.repositories.contains(dir)
    }

    fn exclude(&mut self, _walk: &mut Walk, dir: &Path) -> Option<Arc<Gitignore>> {
        self.rules.excludes.get(dir).cloned()
    }

    fn gitignore(&mut self, walk: &mut Walk, dir: &Path, _listed: bool, full: bool) -> IgnoreFile {
        match self.rules.gitignores.get(dir) {
            None => IgnoreFile::Missing,
            // The rules are incomplete already.
            Some(None) => IgnoreFile::Unusable,
            // Past the cap, as when the rules were read. (Rules restart
            // wherever they did then, so this does not happen with rules read
            // from this workspace.)
            Some(Some(_)) if full => walk.unusable(),
            Some(Some(gitignore)) => IgnoreFile::Found(Arc::clone(gitignore)),
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
    Found(Arc<Gitignore>),
    /// There, but not usable as git would use it (see [`Walk::ignore_file`]).
    Unusable,
}

impl IgnoreFile {
    fn found(self) -> Option<Arc<Gitignore>> {
        match self {
            IgnoreFile::Found(gitignore) => Some(gitignore),
            IgnoreFile::Missing | IgnoreFile::Unusable => None,
        }
    }
}

/// One walk's reads: what is left of its budget, and whether it has missed
/// anything.
struct Walk {
    entries_left: usize,
    /// `None` when the budget's time is too long to add to now.
    deadline: Option<Instant>,
    steps: u32,
    exhausted: bool,
    ignore_bytes_left: u64,
    incomplete: bool,
}

impl Walk {
    fn new(budget: Budget) -> Walk {
        Walk {
            entries_left: budget.entries,
            deadline: Instant::now().checked_add(budget.time),
            steps: 0,
            exhausted: false,
            ignore_bytes_left: MAX_IGNORE_TOTAL,
            incomplete: false,
        }
    }

    /// Counts one step. `false`, and the walk is incomplete, once the budget
    /// is spent; the clock is looked at every [`CLOCK_EVERY`] steps.
    fn tick(&mut self) -> bool {
        if self.exhausted {
            return false;
        }
        if self.steps.is_multiple_of(CLOCK_EVERY)
            && self.deadline.is_some_and(|d| Instant::now() >= d)
        {
            self.stop();
            return false;
        }
        self.steps = self.steps.wrapping_add(1);
        true
    }

    /// Takes one entry from the budget: [`Walk::tick`], and one entry fewer.
    fn take(&mut self) -> bool {
        if !self.tick() {
            return false;
        }
        if self.entries_left == 0 {
            self.stop();
            return false;
        }
        self.entries_left -= 1;
        true
    }

    fn stop(&mut self) {
        self.exhausted = true;
        self.incomplete = true;
    }

    /// The entries of the directory `dir`. `None`, and the walk is
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
        let Some(gitdir) = noting(gitdir_at(holder), &mut self.incomplete) else {
            return IgnoreFile::Missing;
        };
        let common = if std::fs::symlink_metadata(gitdir.join("commondir")).is_ok() {
            match noting(common_dir(&gitdir, &mut Vec::new()), &mut self.incomplete) {
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
    /// walk is incomplete, when it is not a regular file, cannot be read, is
    /// larger than [`MAX_IGNORE_BYTES`] or than what is left of
    /// [`MAX_IGNORE_TOTAL`], or holds a negation git would match differently.
    fn ignore_file(&mut self, root: &Path, file: &Path) -> IgnoreFile {
        if !self.tick() {
            return IgnoreFile::Unusable;
        }
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
            Some(gitignore) => IgnoreFile::Found(Arc::new(gitignore)),
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

/// The ignore rules for what is directly in one directory: the
/// repository's `info/exclude`, and the `.gitignore` of each directory from
/// the repository's top (or the workspace's) down to this one.
#[derive(Clone)]
struct Rules {
    /// The deepest `.gitignore` first.
    gitignores: Option<Rc<Chain>>,
    /// How many there are.
    nested: usize,
    exclude: Option<Arc<Gitignore>>,
}

struct Chain {
    gitignore: Arc<Gitignore>,
    parent: Option<Rc<Chain>>,
}

impl Rules {
    /// The rules at the top of a repository or of the workspace.
    fn start(exclude: Option<Arc<Gitignore>>) -> Rules {
        Rules {
            gitignores: None,
            nested: 0,
            exclude,
        }
    }

    fn push(&mut self, gitignore: Arc<Gitignore>) {
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

    fn entries(entries: usize) -> Budget {
        Budget {
            entries,
            ..Budget::DEFAULT
        }
    }

    #[test]
    fn the_walk_stops_when_its_budget_runs_out() {
        let (_d, ws) = workspace();
        for i in 0..20 {
            std::fs::create_dir_all(ws.join(format!("r{i}/.git"))).unwrap();
        }
        let none = IgnoreRules::default();
        // 20 entries in the workspace, one (`.git`) in each repository, and
        // each of the 20 gitdirs followed.
        let enough = discover_with_budget(&ws, None, &none, entries(60));
        assert!(!enough.incomplete, "{enough:?}");
        assert_eq!(enough.dot_gits.len(), 20);
        let short = discover_with_budget(&ws, None, &none, entries(59));
        assert!(short.incomplete, "{short:?}");
        // Spent during the walk, it stops the walk there.
        let shorter = discover_with_budget(&ws, None, &none, entries(30));
        assert!(shorter.incomplete, "{shorter:?}");
        assert_eq!(shorter.dot_gits.len(), 10, "{shorter:?}");

        assert!(!read_ignore_rules_with_budget(&ws, None, entries(40)).incomplete());
        assert!(read_ignore_rules_with_budget(&ws, None, entries(39)).incomplete());
    }

    #[test]
    fn the_modules_walk_counts_against_the_budget() {
        let (_d, ws) = workspace();
        for i in 0..30 {
            let module = ws.join(format!(".git/modules/m{i}"));
            std::fs::create_dir_all(&module).unwrap();
            std::fs::write(module.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        }
        let none = IgnoreRules::default();
        let enough = discover_with_budget(&ws, None, &none, entries(1000));
        assert!(!enough.incomplete, "{enough:?}");
        assert_eq!(enough.gitdirs.len(), 31);
        let short = discover_with_budget(&ws, None, &none, entries(20));
        assert!(short.incomplete, "{short:?}");
        assert!(short.gitdirs.len() < 31, "{short:?}");
    }

    #[test]
    fn the_walks_stop_at_their_deadline() {
        let (_d, ws) = workspace();
        std::fs::create_dir_all(ws.join(".git")).unwrap();
        std::fs::create_dir_all(ws.join("r/.git")).unwrap();
        let now = Budget {
            time: Duration::ZERO,
            ..Budget::DEFAULT
        };
        let rules = read_ignore_rules_with_budget(&ws, None, now);
        assert!(rules.incomplete(), "{rules:?}");
        let index = discover_with_budget(&ws, None, &IgnoreRules::default(), now);
        assert!(index.incomplete, "{index:?}");
        assert!(index.dot_gits.is_empty(), "{index:?}");
    }

    #[test]
    fn the_clock_is_looked_at_while_directories_are_matched() {
        // 3,000 wildcard patterns, and 3,000 directories that each match some
        // of them: seconds of matching, with no directory read in between.
        let (_d, ws) = workspace();
        std::fs::create_dir(ws.join(".git")).unwrap();
        let patterns: String = (0..3000).map(|i| format!("*x{i}*y*/\n")).collect();
        std::fs::write(ws.join(".gitignore"), patterns).unwrap();
        for j in 0..3000 {
            std::fs::create_dir(ws.join(format!("d{j}x{j}qy"))).unwrap();
        }
        let started = Instant::now();
        let soon = Budget {
            time: Duration::from_millis(200),
            ..Budget::DEFAULT
        };
        let rules = read_ignore_rules_with_budget(&ws, None, soon);
        let took = started.elapsed();
        assert!(rules.incomplete(), "{rules:?}");
        assert!(took < Duration::from_secs(3), "{took:?}");
    }

    #[test]
    fn the_rules_can_be_kept_for_a_session_on_any_thread() {
        fn send_and_sync<T: Send + Sync>() {}
        send_and_sync::<IgnoreRules>();
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "canonical")]
    fn a_workspace_that_is_not_canonical_is_a_bug() {
        let (_d, ws) = workspace();
        std::fs::create_dir_all(ws.join("sub")).unwrap();
        read_ignore_rules(&ws.join("sub/.."), None);
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "canonical")]
    fn a_workspace_that_is_not_canonical_is_a_bug_for_discover_too() {
        let (_d, ws) = workspace();
        std::fs::create_dir_all(ws.join("sub")).unwrap();
        discover(&ws.join("sub/.."), None, &IgnoreRules::default());
    }
}
