//! Finds every gitdir in a workspace.
//!
//! The workspace is hostile: a sandboxed command can write any of it, and
//! the Linux guard runs [`discover`] in the harness process around every
//! command. So the walks never follow a symlink and open nothing but regular
//! files (through [`read_regular`]). Their cost is bounded: a budget of
//! [`MAX_ENTRIES`] directory entries and `readlink` lookups, and a deadline
//! of [`MAX_TIME`]. The clock is looked at every [`CLOCK_EVERY`] steps, for
//! each directory matched against the rules, and before each symlink
//! resolution, so a walk can run past the deadline by the work between two
//! looks: [`CLOCK_EVERY`] directory entries or ignore files read (each
//! bounded by [`MAX_IGNORE_BYTES`] and the pattern caps), one directory
//! matched (bounded by [`MAX_WILD_COST`]), or one resolution of at most
//! [`MAX_LOOKUPS`] lookups. A last look when the walk ends reports any
//! overrun as incomplete. What the ignore matchers hold in memory is bounded
//! by [`MAX_PATTERNS_TOTAL`] and [`MAX_WILD_COST`].
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

use super::linked::{Allowance, common_dir, gitdir_at, linked_gitdirs_at, noting, within};
use super::read::{missing, read_regular};

/// Directory entries one walk reads before it stops. Each gitdir
/// [`discover`] follows counts as one too.
const MAX_ENTRIES: usize = 200_000;

/// How long one walk may take before it stops.
const MAX_TIME: Duration = Duration::from_secs(5);

/// Steps (an entry read, an ignore file read, a gitdir followed) between two
/// looks at the clock. The clock is also looked at for each directory
/// matched against the rules, before each symlink resolution, and when a
/// walk ends.
const CLOCK_EVERY: u32 = 64;

/// Entries one symlink resolution (a `.git`, a gitfile, a `commondir`, and
/// the chains they lead through) may look at, each a `readlink` charged to
/// the budget: git's own limit of 40 symlinks lets one run to tens of
/// thousands. Past it, the resolution gives up and the walk is incomplete.
const MAX_LOOKUPS: usize = 4096;

/// The longest path a symlink resolution may reach: `PATH_MAX` on Linux.
const MAX_RESOLVED_PATH: usize = 4096;

/// Bytes of paths one [`discover`] records in [`GitIndex::links`]. Past it,
/// no more are recorded and the index is incomplete.
const MAX_LINKS_BYTES: usize = 16 << 20;

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
/// reads: a bound on the bytes read and parsed. (What the matchers built
/// from them cost is bounded by the pattern caps below.) None of a file past
/// it applies.
const MAX_IGNORE_TOTAL: u64 = 4 << 20;

/// Patterns one ignore file may hold. None of a larger one applies. A plain
/// pattern costs its matcher about 1 KB (measured: 10,000 names, 12 MB).
const MAX_PATTERNS: usize = 10_000;

/// Patterns all the ignore files one reading of the rules uses may hold
/// together: about 35 MB of matchers.
const MAX_PATTERNS_TOTAL: usize = 30_000;

/// What the wildcard patterns (see [`is_wild`]) of all the ignore files one
/// reading uses may cost, a file's cost being how many it holds times their
/// bytes. The matcher tests them together in one regex set, and searching
/// it takes memory that grows with that product: measured at about 100
/// bytes per unit at worst (3,000 patterns like `*a1*b*c*/`, 37 KB, took
/// 5 GB; at this cap, the worst shapes found took 49 to 84 MB). A large
/// ordinary file costs a fraction of it: GitHub's Node, Python,
/// VisualStudio, Java, Go, macOS and JetBrains templates together, 112
/// wildcard patterns in 1,714 bytes, cost 192,000.
const MAX_WILD_COST: usize = 1_000_000;

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
    /// names: each symlink, directory and gitfile. At most 16 MiB of paths.
    pub links: BTreeSet<PathBuf>,
    /// Whether the walk may have missed something, so the sets above are not
    /// the whole workspace: a directory could not be read; a gitfile or
    /// `commondir` file is there but could not be read; a symlink resolution
    /// gave up (past 4,096 lookups or a 4,096-byte path); a `modules/` tree
    /// went deeper than 64 directories; the walk ran out of its budget of
    /// 200,000 directory entries and lookups; it ran past 5 seconds (it
    /// stops at the next look at the clock, so it can overrun by one step's
    /// work); `links` reached 16 MiB of paths, and no more were recorded; or
    /// the [`IgnoreRules`] it was given are incomplete.
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
    /// could not be read, was over 1 MiB or over 4 MiB with the others read,
    /// held over 10,000 patterns or over 30,000 with the others, held
    /// wildcard patterns whose matcher would be costly, was the 33rd nested
    /// `.gitignore`, or held a negation the matcher cannot take as git
    /// would. Also when a directory, gitfile or `commondir` file could not be
    /// read, a symlink resolution gave up, or the reading ran out of its
    /// budget or past its deadline, so ignore files may have gone unread.
    /// Every
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
    walk.finish();
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
        if !walk.take() || !walk.in_time() {
            break;
        }
        let mut visited = Vec::new();
        let mut allowance = walk.allowance();
        let common = common_dir(&gitdir, &mut visited, &mut allowance);
        walk.charge(&allowance);
        let common = noting(common, &mut walk.incomplete);
        // As for a gitfile: the entries on the way, but not the gitdir it
        // starts from or the directories above that.
        walk.record_links(
            &mut index.links,
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
    walk.finish();
    index.incomplete = walk.incomplete || rules.incomplete();
    index
}

/// What one walk may spend.
#[derive(Debug, Clone)]
pub(crate) struct Budget {
    /// Directory entries read and `readlink` lookups made.
    pub(crate) entries: usize,
    /// What says the walk's time is up.
    pub(crate) clock: Clock,
    /// Bytes of paths the index's `links` may hold.
    pub(crate) links_bytes: usize,
}

impl Budget {
    pub(crate) const DEFAULT: Budget = Budget {
        entries: MAX_ENTRIES,
        clock: Clock::Wall(MAX_TIME),
        links_bytes: MAX_LINKS_BYTES,
    };
}

/// What says a walk's time is up.
#[derive(Debug, Clone)]
pub(crate) enum Clock {
    /// The wall clock: this long after the walk starts.
    Wall(Duration),
    /// For tests: a clock that runs with the walk's work, the budget it has
    /// spent, so that its time is up once `deadline` of it is spent. It
    /// counts in `meter` how often it is looked at, and what was spent by
    /// the last look.
    #[cfg(test)]
    Work { deadline: usize, meter: Arc<Meter> },
}

/// What a [`Clock::Work`] saw.
#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct Meter {
    pub(crate) looks: std::sync::atomic::AtomicUsize,
    pub(crate) spent: std::sync::atomic::AtomicUsize,
}

/// The gitdirs of `gitdir`'s linked worktrees (every directory in
/// `worktrees/`) and submodules (every directory below `modules/` that holds
/// a `HEAD`, down to 64 levels, and not in the [`DATA_DIRS`] of another).
/// Symlinks are not followed.
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
            // Matching a directory against the rules can take milliseconds
            // (see `MAX_WILD_COST`): the clock is looked at for each.
            if !walk.tick() || !walk.in_time() {
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
        } else if walk.in_time() {
            let mut allowance = walk.allowance();
            let linked = linked_gitdirs_at(dir, self.workspace, &mut allowance);
            walk.charge(&allowance);
            walk.incomplete |= linked.unreadable;
            self.index.gitdirs.extend(linked.gitdirs);
            walk.record_links(&mut self.index.links, linked.entries);
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
    /// The budget's entries, for [`Clock::Work`].
    #[cfg(test)]
    entries: usize,
    entries_left: usize,
    clock: Clock,
    /// For [`Clock::Wall`]: `None` when its time is too long to add to now.
    deadline: Option<Instant>,
    steps: u32,
    exhausted: bool,
    ignore_bytes_left: u64,
    links_bytes_left: usize,
    patterns_left: usize,
    wild_cost_left: usize,
    incomplete: bool,
}

impl Walk {
    fn new(budget: Budget) -> Walk {
        let deadline = match budget.clock {
            Clock::Wall(time) => Instant::now().checked_add(time),
            #[cfg(test)]
            Clock::Work { .. } => None,
        };
        Walk {
            #[cfg(test)]
            entries: budget.entries,
            entries_left: budget.entries,
            clock: budget.clock,
            deadline,
            steps: 0,
            exhausted: false,
            ignore_bytes_left: MAX_IGNORE_TOTAL,
            links_bytes_left: budget.links_bytes,
            patterns_left: MAX_PATTERNS_TOTAL,
            wild_cost_left: MAX_WILD_COST,
            incomplete: false,
        }
    }

    /// Counts one step. `false`, and the walk is incomplete, once the budget
    /// is spent; the clock is looked at every [`CLOCK_EVERY`] steps.
    fn tick(&mut self) -> bool {
        if self.exhausted || (self.steps.is_multiple_of(CLOCK_EVERY) && !self.in_time()) {
            return false;
        }
        self.steps = self.steps.wrapping_add(1);
        true
    }

    /// Looks at the clock: `false`, and the walk stops, past the deadline.
    fn in_time(&mut self) -> bool {
        if self.past_deadline() {
            self.stop();
        }
        !self.exhausted
    }

    fn past_deadline(&self) -> bool {
        match &self.clock {
            Clock::Wall(_) => self.deadline.is_some_and(|d| Instant::now() >= d),
            #[cfg(test)]
            Clock::Work { deadline, meter } => {
                use std::sync::atomic::Ordering::Relaxed;
                let spent = self.entries - self.entries_left;
                meter.looks.fetch_add(1, Relaxed);
                meter.spent.store(spent, Relaxed);
                spent >= *deadline
            }
        }
    }

    /// Ends the walk with one more look at the clock, so that an overrun
    /// since the last is reported.
    fn finish(&mut self) {
        if self.past_deadline() {
            self.incomplete = true;
        }
    }

    /// What one symlink resolution may look at: [`MAX_LOOKUPS`], or what is
    /// left of the budget.
    fn allowance(&self) -> Allowance {
        Allowance::new(self.entries_left.min(MAX_LOOKUPS), MAX_RESOLVED_PATH)
    }

    /// Charges what a resolution looked at to the budget. One that gave up
    /// leaves the walk incomplete.
    fn charge(&mut self, allowance: &Allowance) {
        self.entries_left = self.entries_left.saturating_sub(allowance.spent);
        if allowance.ran_out {
            self.incomplete = true;
            if self.entries_left == 0 {
                self.stop();
            }
        }
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

    /// Records `found` in `links` while the bytes of the paths recorded stay
    /// within the budget's `links_bytes`. Past it, none more are, and the
    /// walk is incomplete.
    fn record_links(
        &mut self,
        links: &mut BTreeSet<PathBuf>,
        found: impl IntoIterator<Item = PathBuf>,
    ) {
        for link in found {
            if links.contains(&link) {
                continue;
            }
            let bytes = link.as_os_str().len();
            if bytes > self.links_bytes_left {
                self.links_bytes_left = 0;
                self.incomplete = true;
                return;
            }
            self.links_bytes_left -= bytes;
            links.insert(link);
        }
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
        if !self.in_time() {
            return IgnoreFile::Missing;
        }
        let mut allowance = self.allowance();
        let common = self.resolve_common(holder, &mut allowance);
        self.charge(&allowance);
        let Some(common) = common else {
            return IgnoreFile::Missing;
        };
        if !within(&common, workspace) {
            return IgnoreFile::Missing;
        }
        self.ignore_file(holder, &common.join("info/exclude"))
    }

    /// The gitdir the `.git` in `holder` leads to, or the common one its
    /// `commondir` names.
    fn resolve_common(&mut self, holder: &Path, allowance: &mut Allowance) -> Option<PathBuf> {
        let gitdir = noting(gitdir_at(holder, allowance), &mut self.incomplete)?;
        if std::fs::symlink_metadata(gitdir.join("commondir")).is_err() {
            return Some(gitdir);
        }
        noting(
            common_dir(&gitdir, &mut Vec::new(), allowance),
            &mut self.incomplete,
        )
    }

    /// The ignore file `file`, for the paths below `root`. Unusable, and the
    /// walk is incomplete, when it is not a regular file, cannot be read, is
    /// larger than [`MAX_IGNORE_BYTES`] or than what is left of
    /// [`MAX_IGNORE_TOTAL`], holds more patterns than [`MAX_PATTERNS`] or
    /// what is left of [`MAX_PATTERNS_TOTAL`], or wildcard patterns that cost
    /// more than what is left of [`MAX_WILD_COST`] (checked before its
    /// matcher is built), or holds a negation git would match differently.
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
        let cost = Cost::of(&bytes);
        if cost.patterns > MAX_PATTERNS.min(self.patterns_left) || cost.wild() > self.wild_cost_left
        {
            return self.unusable();
        }
        self.patterns_left -= cost.patterns;
        self.wild_cost_left -= cost.wild();
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
    let mut builder = GitignoreBuilder::new(root);
    for line in lines(bytes) {
        let negation = line.starts_with(b"!");
        let added =
            std::str::from_utf8(line).is_ok_and(|line| builder.add_line(None, line).is_ok());
        if !added && negation {
            return None;
        }
    }
    builder.build().ok()
}

/// The lines of an ignore file as git splits them: after a UTF-8 BOM, on
/// `\n`, without the `\r` before it.
fn lines(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    bytes
        .split(|&b| b == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
}

/// What the matcher built from an ignore file would cost.
#[derive(Debug, Default)]
struct Cost {
    patterns: usize,
    wild: usize,
    wild_bytes: usize,
}

impl Cost {
    /// Counted before the matcher is built: its patterns (every line that is
    /// not blank or a comment), and those of them that are wild.
    fn of(bytes: &[u8]) -> Cost {
        let mut cost = Cost::default();
        for line in lines(bytes) {
            if line.starts_with(b"#") || line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            cost.patterns += 1;
            if is_wild(line) {
                cost.wild += 1;
                cost.wild_bytes += line.len();
            }
        }
        cost
    }

    /// How many wild patterns times their bytes: what searching their regex
    /// set takes memory in proportion to.
    fn wild(&self) -> usize {
        self.wild.saturating_mul(self.wild_bytes)
    }
}

/// Whether the matcher tests `pattern` in its regex set (or with a regex of
/// its own), rather than looking it up. globset (`MatchStrategy::new`, for
/// gitignore globs, whose `*` does not match `/`) looks up only literals
/// (after a leading `!`, a leading or trailing `/`, or a leading `**/`), and
/// `*.ext` at any depth: with a leading `**/`, or with no `/` at all, to
/// which git's matcher adds one. Anything else with a wildcard (`*`, `?`,
/// `[`, `{`, or an escape) is wild: `/*.ext` and `dir/*.ext` too. (Measured:
/// `*suffix` and `prefix*` patterns cost about 8 KB each to build.)
fn is_wild(pattern: &[u8]) -> bool {
    let is_meta = |b: &u8| matches!(b, b'*' | b'?' | b'[' | b'{' | b'\\');
    let mut pattern = pattern.trim_ascii_end();
    pattern = pattern.strip_prefix(b"!").unwrap_or(pattern);
    let anchored = pattern.starts_with(b"/");
    pattern = pattern.strip_prefix(b"/").unwrap_or(pattern);
    pattern = pattern.strip_suffix(b"/").unwrap_or(pattern);
    let (any_depth, rest) = match pattern.strip_prefix(b"**/") {
        Some(rest) => (true, rest),
        None => (!anchored && !pattern.contains(&b'/'), pattern),
    };
    if !rest.iter().any(is_meta) {
        return false;
    }
    let ext = rest.strip_prefix(b"*.").filter(|_| any_depth);
    !ext.is_some_and(|ext| {
        !ext.is_empty() && !ext.iter().any(|b| is_meta(b) || matches!(b, b'/' | b'.'))
    })
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
    use std::sync::atomic::Ordering;

    use super::super::linked::long_chain;
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
        // A lookup for each component of an absolute path resolved.
        let lookups = |rel: &str| ws.join(rel).components().count() - 1;
        // 20 entries in the workspace and one (`.git`) in each repository;
        // then each of the 20 gitdirs followed, and a lookup of its
        // `commondir`.
        let spent = 20 + 20 + 20 * (1 + lookups("r0/.git/commondir"));
        let enough = discover_with_budget(&ws, None, &none, entries(spent));
        assert!(!enough.incomplete, "{enough:?}");
        assert_eq!(enough.dot_gits.len(), 20);
        let short = discover_with_budget(&ws, None, &none, entries(spent - 1));
        assert!(short.incomplete, "{short:?}");
        // Spent during the walk, it stops the walk there.
        let shorter = discover_with_budget(&ws, None, &none, entries(30));
        assert!(shorter.incomplete, "{shorter:?}");
        assert_eq!(shorter.dot_gits.len(), 10, "{shorter:?}");

        // The entries, and a lookup of each `.git` for its `info/exclude`.
        let spent = 20 + 20 + 20 * lookups("r0/.git");
        assert!(!read_ignore_rules_with_budget(&ws, None, entries(spent)).incomplete());
        assert!(read_ignore_rules_with_budget(&ws, None, entries(spent - 1)).incomplete());
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
            clock: Clock::Wall(Duration::ZERO),
            ..Budget::DEFAULT
        };
        let rules = read_ignore_rules_with_budget(&ws, None, now.clone());
        assert!(rules.incomplete(), "{rules:?}");
        let index = discover_with_budget(&ws, None, &IgnoreRules::default(), now);
        assert!(index.incomplete, "{index:?}");
        assert!(index.dot_gits.is_empty(), "{index:?}");
    }

    /// Wildcard patterns of the kind that made matchers take gigabytes (at
    /// 3,000 of them, matched against names like `a1a8a15...bbc`).
    fn adversarial_patterns(count: usize) -> String {
        (0..count).map(|i| format!("*a{i}*b*c*/\n")).collect()
    }

    /// Writes `text` to `dir/name` and has `walk` read it as an ignore file.
    fn read_as_ignore_file(walk: &mut Walk, dir: &Path, name: &str, text: &str) -> IgnoreFile {
        std::fs::write(dir.join(name), text).unwrap();
        walk.ignore_file(dir, &dir.join(name))
    }

    #[test]
    fn an_ignore_file_whose_matcher_would_be_costly_is_not_used() {
        let (_d, dir) = workspace();
        // Refused before a matcher is built, and nothing is matched here.
        let mut walk = Walk::new(Budget::DEFAULT);
        let file = read_as_ignore_file(&mut walk, &dir, "wild", &adversarial_patterns(3000));
        assert!(matches!(file, IgnoreFile::Unusable));
        assert!(walk.incomplete);
        // More patterns than one file may hold, even plain ones.
        let mut walk = Walk::new(Budget::DEFAULT);
        let plain: String = (0..=MAX_PATTERNS).map(|i| format!("name{i}\n")).collect();
        let file = read_as_ignore_file(&mut walk, &dir, "plain", &plain);
        assert!(matches!(file, IgnoreFile::Unusable));
        assert!(walk.incomplete);
    }

    #[test]
    fn what_the_matchers_cost_in_all_is_bounded() {
        let (_d, dir) = workspace();
        // About 230,000 of `MAX_WILD_COST` each: four fit, a fifth does not.
        let mut walk = Walk::new(Budget::DEFAULT);
        let wild = adversarial_patterns(150);
        for i in 0..4 {
            let file = read_as_ignore_file(&mut walk, &dir, &format!("w{i}"), &wild);
            assert!(matches!(file, IgnoreFile::Found(_)), "file {i}");
        }
        let file = read_as_ignore_file(&mut walk, &dir, "w4", &wild);
        assert!(matches!(file, IgnoreFile::Unusable));
        // Patterns in all.
        let mut walk = Walk::new(Budget::DEFAULT);
        let plain: String = (0..MAX_PATTERNS).map(|i| format!("name{i}\n")).collect();
        for i in 0..MAX_PATTERNS_TOTAL / MAX_PATTERNS {
            let file = read_as_ignore_file(&mut walk, &dir, &format!("p{i}"), &plain);
            assert!(matches!(file, IgnoreFile::Found(_)), "file {i}");
        }
        let file = read_as_ignore_file(&mut walk, &dir, "one", "one\n");
        assert!(matches!(file, IgnoreFile::Unusable));
    }

    #[test]
    fn a_large_ordinary_ignore_file_is_used() {
        let (_d, dir) = workspace();
        let templates = include_str!("../../tests/fixtures/templates.gitignore");
        let mut walk = Walk::new(Budget::DEFAULT);
        let file = read_as_ignore_file(&mut walk, &dir, "templates", templates);
        assert!(matches!(file, IgnoreFile::Found(_)));
        // Twice as large, too.
        let mut walk = Walk::new(Budget::DEFAULT);
        let file = read_as_ignore_file(&mut walk, &dir, "twice", &templates.repeat(2));
        assert!(matches!(file, IgnoreFile::Found(_)));
        assert!(!walk.incomplete);
    }

    #[test]
    fn the_patterns_the_matcher_tests_in_its_regex_set_are_wild() {
        // As globset (glob.rs, `MatchStrategy::new`) sorts gitignore globs,
        // which have `literal_separator` on: only literals, and `*.ext` at
        // any depth (`**/*.ext`, which git's matcher makes of a pattern with
        // no `/`), stay out of it.
        for wild in [
            "{a,b}",
            "x{1,2}y/",
            "/*.log",
            "!/*.log",
            "dir/*.log",
            "**/dir/*.log",
            "*a*",
            "a?b",
            "[Dd]ebug/",
            "*suffix",
            "prefix*",
            "*.tar.gz",
            "*.",
            "foo/**",
            "a/**/b",
            "**",
            "*",
            "\\*.log",
        ] {
            assert!(is_wild(wild.as_bytes()), "{wild} is wild");
        }
        for plain in [
            "*.log",
            "!*.log",
            "*.log  ",
            "*.d/",
            "**/*.log",
            "/**/*.log",
            "name",
            "name/",
            "!name",
            "/name",
            "dir/sub",
            "**/name",
            "**/foo/bar",
        ] {
            assert!(!is_wild(plain.as_bytes()), "{plain} is looked up");
        }
    }

    /// A clock that runs with the walk's work, its time up once `deadline`
    /// of the budget is spent, and what it saw.
    fn work_clock(deadline: usize) -> (Clock, Arc<Meter>) {
        let meter = Arc::new(Meter::default());
        let clock = Clock::Work {
            deadline,
            meter: Arc::clone(&meter),
        };
        (clock, meter)
    }

    #[test]
    fn the_clock_is_looked_at_while_directories_are_matched() {
        // Matching a directory against rules at the wildcard cap takes
        // milliseconds: with 2,000 in one directory, and no entry read
        // between them, the clock must be looked at for each.
        let (_d, ws) = workspace();
        for j in 0..2000 {
            std::fs::create_dir(ws.join(format!("d{j}"))).unwrap();
        }
        let (clock, meter) = work_clock(usize::MAX);
        let budget = Budget {
            clock,
            ..Budget::DEFAULT
        };
        discover_with_budget(&ws, None, &IgnoreRules::default(), budget);
        let looks = meter.looks.load(Ordering::Relaxed);
        assert!(looks >= 2000, "{looks} looks");
    }

    /// `holders` directories whose `.git` leads through three long symlink
    /// chains (see `linked::long_chain`), shared by all of them.
    fn chained_holders(ws: &Path, holders: usize) {
        std::fs::create_dir_all(ws.join("gd")).unwrap();
        std::fs::create_dir_all(ws.join("common")).unwrap();
        long_chain(ws, "a", 38, "gitfile");
        let gitfile = format!("gitdir: {}\n", ws.join("b0").display());
        std::fs::write(ws.join("gitfile"), gitfile).unwrap();
        long_chain(ws, "b", 39, "gd");
        std::fs::write(ws.join("gd/commondir"), "../c0\n").unwrap();
        long_chain(ws, "c", 39, "common");
        for h in 0..holders {
            std::fs::create_dir(ws.join(format!("h{h}"))).unwrap();
            std::os::unix::fs::symlink("../a0", ws.join(format!("h{h}/.git"))).unwrap();
        }
    }

    #[test]
    fn many_holders_sharing_long_chains_cost_little() {
        let (_d, ws) = workspace();
        chained_holders(&ws, 20);
        let (clock, meter) = work_clock(usize::MAX);
        let budget = Budget {
            clock,
            ..Budget::DEFAULT
        };
        let index = discover_with_budget(&ws, None, &IgnoreRules::default(), budget);
        assert_eq!(index.dot_gits.len(), 20, "{:?}", index.dot_gits);
        // Each resolution would look at over 10,000 entries, and gives up
        // at `MAX_LOOKUPS`.
        assert!(index.incomplete, "{index:?}");
        let spent = meter.spent.load(Ordering::Relaxed);
        assert!(spent <= 20 * MAX_LOOKUPS + 1000, "{spent} spent");
    }

    #[test]
    fn the_clock_is_looked_at_before_each_resolution() {
        // 60 holders whose `.git` goes through long symlink chains. The time
        // is up during the first resolution (it takes `MAX_LOOKUPS`, far more
        // than the few hundred entries listed before it).
        let (_d, ws) = workspace();
        chained_holders(&ws, 60);
        let (clock, _) = work_clock(1000);
        let budget = Budget {
            clock,
            ..Budget::DEFAULT
        };
        let index = discover_with_budget(&ws, None, &IgnoreRules::default(), budget);
        assert!(index.incomplete, "{index:?}");
        // Noticed before the next resolution: at most the holder listed
        // then is indexed too. (Each holder is one step, so without that
        // look the walk would go on to the next every-64-steps look.)
        assert!(index.dot_gits.len() <= 2, "{:?}", index.dot_gits);
    }

    #[test]
    fn a_walk_that_ends_past_its_deadline_says_so() {
        let (_d, ws) = workspace();
        // One resolution of about 2,700 entries, and nothing after it but
        // the end of the walk (the gitdir is outside, so nothing follows).
        long_chain(&ws, "a", 30, "/nonexistent-harness-gitdir");
        std::fs::create_dir(ws.join("h")).unwrap();
        std::os::unix::fs::symlink("../a0", ws.join("h/.git")).unwrap();
        // The time is up during that resolution.
        let (clock, meter) = work_clock(100);
        let budget = Budget {
            clock,
            ..Budget::DEFAULT
        };
        let index = discover_with_budget(&ws, None, &IgnoreRules::default(), budget);
        assert!(meter.spent.load(Ordering::Relaxed) > 2000);
        assert!(index.incomplete, "{index:?}");
    }

    #[test]
    fn links_stop_being_recorded_past_their_cap() {
        let (_d, ws) = workspace();
        std::fs::create_dir(ws.join("gd")).unwrap();
        long_chain(&ws, "a", 3, "gd");
        std::os::unix::fs::symlink("a0", ws.join(".git")).unwrap();
        let none = IgnoreRules::default();
        let bytes = |links: &BTreeSet<PathBuf>| -> usize {
            links.iter().map(|link| link.as_os_str().len()).sum()
        };
        let full = discover_with_budget(&ws, None, &none, Budget::DEFAULT);
        assert!(!full.incomplete, "{full:?}");
        let all = bytes(&full.links);
        assert!(full.links.len() > 200, "{}", full.links.len());
        let capped = Budget {
            links_bytes: all / 2,
            ..Budget::DEFAULT
        };
        let index = discover_with_budget(&ws, None, &none, capped);
        assert!(index.incomplete, "{index:?}");
        let kept = bytes(&index.links);
        assert!(kept > 0 && kept <= all / 2, "{kept} of {all}");
        assert_eq!(index.gitdirs, full.gitdirs);
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
