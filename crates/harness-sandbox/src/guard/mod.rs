//! The git-metadata guard: finds and undoes changes to protected git metadata
//! around each sandboxed command, for the Linux sandbox. Platform-neutral, so
//! it is tested on every host.
//!
//! Mounts cannot cover a name that does not exist yet, and in the Linux basic
//! tier there are no mounts at all. So around each command the guard:
//!
//! - before it, moves to quarantine protected names that appeared in a known
//!   gitdir, or at the top of the workspace, since the previous command
//!   ended (a process that command left running may have planted them), and
//!   in the basic tier, when such processes were running, restores the
//!   protected files they changed ([`GuardSession::set_survivor_probe`]);
//! - indexes the workspace ([`discover`], with the ignore rules read once per
//!   session: [`GuardSession::prime`]) and records which protected names
//!   exist, and in the basic tier saves the protected files;
//! - while it runs (a watcher calls [`WatchHandle::check`]) and after it
//!   ends, moves to quarantine every new protected name, new gitdir and
//!   replaced `.git`, and restores changed protected files;
//! - after it ends, also walks the workspace for new `.git` entries.
//!
//! Nothing is ever deleted: it is moved to `<quarantine root>/<time>-<pid>-<n>/`
//! at its path relative to the workspace. Nothing in the workspace is reached
//! through a symlink either ([`nofollow`]): a command, or a process it left
//! running, can swap a directory for one at any moment.

mod nofollow;
mod quarantine;
mod report;
mod snapshot;

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use harness_core::tool::GuardReport;

use crate::gitmeta::{
    GITDIR_PROTECTED, GitIndex, IgnoreRules, WORKSPACE_PROTECTED, discover, nested_gitdirs,
    read_ignore_rules,
};
use nofollow::{Kind, Tree, absent};
use quarantine::Quarantine;
use report::{Finding, Findings, Outcome, What, push};
use snapshot::{Difference, Snapshot};

/// Says whether processes that sandboxed commands started are still running
/// after those commands ended. The Linux sandbox sets one; the default says
/// there are none.
pub type SurvivorProbe = Arc<dyn Fn() -> bool + Send + Sync>;

/// Guards every command of one session, in one or more workspaces. Remembers
/// each workspace's ignore rules, and where things stood when its previous
/// command's guard finished.
pub struct GuardSession {
    quarantine_root: PathBuf,
    probe: Mutex<SurvivorProbe>,
    workspaces: Mutex<BTreeMap<PathBuf, Workspace>>,
}

impl fmt::Debug for GuardSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GuardSession")
            .field("quarantine_root", &self.quarantine_root)
            .field("workspaces", &self.workspaces)
            .finish_non_exhaustive()
    }
}

/// What a session keeps for one workspace.
#[derive(Debug, Default)]
struct Workspace {
    /// Read once, for every scan of the workspace in the session.
    rules: Option<IgnoreRules>,
    /// Whether the session has said that a scan was incomplete.
    reported_incomplete: bool,
    /// Counts the commands begun, so that a [`WatchHandle`] for the time
    /// between two commands does nothing once the next one begins.
    generation: u64,
    /// Where things stood when the last command's guard finished, until the
    /// next one begins.
    kept: Option<Kept>,
}

impl GuardSession {
    /// A session whose quarantined entries go below `quarantine_root`.
    pub fn new(quarantine_root: &Path) -> Arc<GuardSession> {
        let default: SurvivorProbe = Arc::new(|| false);
        Arc::new(GuardSession {
            quarantine_root: canonical_as_far_as_it_exists(quarantine_root),
            probe: Mutex::new(default),
            workspaces: Mutex::default(),
        })
    }

    /// Reads the ignore rules of the canonical `workspace` for the rest of
    /// the session, if they have not been read yet. Call it when the session
    /// starts, before any command (or tool) can have written a `.gitignore`:
    /// a rule a command writes could otherwise hide the repository it makes.
    /// [`begin`](Self::begin) reads them itself if nothing did before.
    pub fn prime(&self, workspace: &Path) {
        self.rules(workspace);
    }

    /// The ignore rules the session uses for `workspace`: read the first
    /// time they are asked for (by this, [`prime`](Self::prime) or
    /// [`begin`](Self::begin)), then the same ones every time. The quarantine
    /// directory is never walked.
    pub fn rules(&self, workspace: &Path) -> IgnoreRules {
        if let Some(rules) = lock(&self.workspaces)
            .get(workspace)
            .and_then(|entry| entry.rules.clone())
        {
            return rules;
        }
        let read = read_ignore_rules(workspace, Some(&self.quarantine_root));
        lock(&self.workspaces)
            .entry(workspace.to_path_buf())
            .or_default()
            .rules
            .get_or_insert(read)
            .clone()
    }

    /// Sets what says whether processes that sandboxed commands started are
    /// still running. When it says so as a command's guard finishes, or at
    /// any check after that, the basic tier restores the protected files
    /// those processes change before the next command begins (and whenever
    /// [`between_commands`](Self::between_commands)' handle checks).
    pub fn set_survivor_probe(&self, probe: SurvivorProbe) {
        *lock(&self.probe) = probe;
    }

    fn survivors(&self) -> bool {
        let probe = Arc::clone(&lock(&self.probe));
        probe()
    }

    /// Starts guarding one command in the canonical `workspace`: see the
    /// module docs. `placeholders` runs after the workspace is indexed and
    /// before the existing protected names are recorded (the Linux full tier
    /// creates empty `hooks/` directories there). With `save_all`, every
    /// protected file is saved so it can be restored (the Linux basic tier);
    /// without it, only protected symlinks and files with more than one hard
    /// link are.
    pub fn begin(
        self: &Arc<Self>,
        workspace: &Path,
        save_all: bool,
        placeholders: impl FnOnce(&GitIndex),
    ) -> GitGuard {
        let rules = self.rules(workspace);
        let tree = Tree::new(workspace);
        let mut quarantine = Quarantine::new(&self.quarantine_root, workspace);
        let mut findings = Findings::default();
        if let Some(mut kept) = self.take_kept(workspace) {
            let survivors = self.survivors();
            kept.check(&tree, &mut quarantine, survivors);
            findings.before = kept.found;
        }

        let index = discover(workspace, Some(&self.quarantine_root), &rules);
        placeholders(&index);
        let candidates = candidates(workspace, &index.gitdirs);
        let existing: BTreeSet<PathBuf> = candidates
            .iter()
            .filter(|path| may_exist(&tree, path))
            .cloned()
            .collect();
        let identities = index
            .dot_gits
            .iter()
            .chain(&index.gitdirs)
            .chain(&index.links)
            .map(|path| (path.clone(), Tracked::new(Identity::of(&tree, path))))
            .collect();
        let top_existed = exists(&tree, &workspace.join(".git"));
        // A gitfile names the gitdir git uses, so it is saved like the
        // protected files.
        let roots: Vec<PathBuf> = existing
            .iter()
            .cloned()
            .chain(gitfiles(&tree, &index.dot_gits))
            .collect();
        let snapshot = Snapshot::take(&tree, &roots, save_all);
        findings.incomplete = index.incomplete && self.first_report_of_incomplete(workspace);
        GitGuard {
            session: Arc::clone(self),
            state: Arc::new(Mutex::new(State {
                workspace: workspace.to_path_buf(),
                tree,
                save_all,
                index,
                candidates,
                existing,
                identities,
                detached: BTreeSet::new(),
                top_existed,
                snapshot,
                quarantine,
                findings,
                finished: false,
            })),
        }
    }

    /// A handle for a file watcher to run the checks between the command
    /// that last finished in `workspace` and the next one, while processes
    /// it left running may still change things: `None` unless the survivor
    /// probe said so when that command's guard finished (or at a check
    /// since). What the checks find is reported with the next command. The
    /// handle does nothing once the next command begins.
    pub fn between_commands(self: &Arc<Self>, workspace: &Path) -> Option<WatchHandle> {
        let workspaces = lock(&self.workspaces);
        let entry = workspaces.get(workspace)?;
        entry.kept.as_ref()?.survivors.then(|| {
            WatchHandle(Watched::Between {
                session: Arc::clone(self),
                workspace: workspace.to_path_buf(),
                generation: entry.generation,
            })
        })
    }

    /// Takes where things stood after the previous command, as the next
    /// one begins.
    fn take_kept(&self, workspace: &Path) -> Option<Kept> {
        let mut workspaces = lock(&self.workspaces);
        let entry = workspaces.entry(workspace.to_path_buf()).or_default();
        entry.generation += 1;
        entry.kept.take()
    }

    fn keep(&self, workspace: &Path, kept: Kept) {
        lock(&self.workspaces)
            .entry(workspace.to_path_buf())
            .or_default()
            .kept = Some(kept);
    }

    /// Whether this is the first time the session reports an incomplete scan
    /// of `workspace`.
    fn first_report_of_incomplete(&self, workspace: &Path) -> bool {
        let mut workspaces = lock(&self.workspaces);
        let entry = workspaces.entry(workspace.to_path_buf()).or_default();
        !std::mem::replace(&mut entry.reported_incomplete, true)
    }

    /// The checks between commands, unless a command has begun since the
    /// handle was made.
    fn check_between(&self, workspace: &Path, generation: u64) {
        let survivors = self.survivors();
        let mut workspaces = lock(&self.workspaces);
        let Some(entry) = workspaces.get_mut(workspace) else {
            return;
        };
        if entry.generation != generation {
            return;
        }
        let Some(kept) = entry.kept.as_mut() else {
            return;
        };
        let mut quarantine = kept
            .quarantine
            .take()
            .unwrap_or_else(|| Quarantine::new(&self.quarantine_root, workspace));
        kept.check(&Tree::new(workspace), &mut quarantine, survivors);
        kept.quarantine = Some(quarantine);
    }

    /// `f` on what was kept for `workspace`, unless a command has begun
    /// since the handle of `generation` was made.
    fn with_kept<T>(
        &self,
        workspace: &Path,
        generation: u64,
        f: impl FnOnce(&Kept) -> T,
    ) -> Option<T> {
        let workspaces = lock(&self.workspaces);
        let entry = workspaces.get(workspace)?;
        (entry.generation == generation)
            .then_some(entry.kept.as_ref())
            .flatten()
            .map(f)
    }
}

/// Guards one command. Give [`watch_handle`](Self::watch_handle) to a file
/// watcher, then call [`finish`](Self::finish) once the command has ended.
#[derive(Debug)]
pub struct GitGuard {
    session: Arc<GuardSession>,
    state: Arc<Mutex<State>>,
}

impl GitGuard {
    /// The workspace's git metadata as it was when the command started.
    pub fn index(&self) -> GitIndex {
        lock(&self.state).index.clone()
    }

    /// Lets a file watcher run the checks while the command runs. It does
    /// nothing once the guard has finished.
    pub fn watch_handle(&self) -> WatchHandle {
        WatchHandle(Watched::Command(Arc::clone(&self.state)))
    }

    /// Runs every check once more, walks the workspace for new `.git`
    /// entries, keeps where things stand for the checks before the next
    /// command, and says what was done, if anything.
    pub fn finish(self) -> Option<GuardReport> {
        let mut state = lock(&self.state);
        state.check();
        let rules = self.session.rules(&state.workspace);
        let now = discover(
            &state.workspace,
            Some(&self.session.quarantine_root),
            &rules,
        );
        let new: Vec<PathBuf> = now
            .dot_gits
            .difference(&state.index.dot_gits)
            .cloned()
            .collect();
        if state.index.incomplete {
            // Both scans may have missed different parts: what the second
            // found may have been there all along.
            state.findings.unchecked = new;
        } else {
            // What the first scan did not find is new, whether or not the
            // second one was complete.
            for dot_git in new {
                state.take(&dot_git, What::Repository);
            }
            state.findings.uncheckable = now.incomplete;
        }
        state.finished = true;
        let kept = state.keep(self.session.survivors());
        self.session.keep(&state.workspace, kept);
        state.findings.report(&state.workspace)
    }
}

/// Runs the guard's checks for a file watcher: while a command runs
/// ([`GitGuard::watch_handle`]), or between two commands while processes the
/// first left running may still change things
/// ([`GuardSession::between_commands`]).
#[derive(Debug, Clone)]
pub struct WatchHandle(Watched);

#[derive(Debug, Clone)]
enum Watched {
    Command(Arc<Mutex<State>>),
    Between {
        session: Arc<GuardSession>,
        workspace: PathBuf,
        generation: u64,
    },
}

impl WatchHandle {
    /// The directories to watch: the workspace, every gitdir and the
    /// `worktrees` and `modules` directories in it, and every saved directory
    /// inside a protected entry. None once the handle does nothing.
    pub fn dirs(&self) -> Vec<PathBuf> {
        match &self.0 {
            Watched::Command(state) => {
                let state = lock(state);
                if state.finished {
                    return Vec::new();
                }
                watched_dirs(
                    &state.workspace,
                    &state.index.gitdirs,
                    Some(&state.snapshot),
                )
            }
            Watched::Between {
                session,
                workspace,
                generation,
            } => session
                .with_kept(workspace, *generation, |kept| {
                    watched_dirs(workspace, &kept.gitdirs, kept.snapshot.as_ref())
                })
                .unwrap_or_default(),
        }
    }

    /// Whether a change to `name` in the watched directory `dir`, or to `dir`
    /// itself when `name` is `None`, can concern protected metadata.
    pub fn relevant(&self, dir: &Path, name: Option<&OsStr>) -> bool {
        let Some(name) = name else {
            return true;
        };
        match &self.0 {
            Watched::Command(state) => {
                let state = lock(state);
                relevant(
                    &state.workspace,
                    &state.index.gitdirs,
                    Some(&state.snapshot),
                    dir,
                    name,
                )
            }
            Watched::Between {
                session,
                workspace,
                generation,
            } => session
                .with_kept(workspace, *generation, |kept| {
                    relevant(workspace, &kept.gitdirs, kept.snapshot.as_ref(), dir, name)
                })
                .unwrap_or(false),
        }
    }

    /// Runs the checks that need no walk of the workspace, undoing what they
    /// find.
    pub fn check(&self) {
        match &self.0 {
            Watched::Command(state) => lock(state).check(),
            Watched::Between {
                session,
                workspace,
                generation,
            } => session.check_between(workspace, *generation),
        }
    }
}

/// One command's guard.
#[derive(Debug)]
struct State {
    workspace: PathBuf,
    tree: Tree,
    save_all: bool,
    index: GitIndex,
    /// Every path a protected name could appear at: the protected names in
    /// each gitdir, and `.harness` and `HEAD` at the top of the workspace.
    candidates: BTreeSet<PathBuf>,
    /// The candidates that existed when the command started.
    existing: BTreeSet<PathBuf>,
    /// What each `.git` entry, gitdir and link was when the command started,
    /// and is expected to be now.
    identities: BTreeMap<PathBuf, Tracked>,
    /// `.git` entries, gitdirs and links that are no longer the ones indexed:
    /// what the snapshot recorded below them is not compared.
    detached: BTreeSet<PathBuf>,
    /// Whether the workspace had a `.git` when the command started.
    top_existed: bool,
    snapshot: Snapshot,
    quarantine: Quarantine,
    findings: Findings,
    /// The guard has finished: its checks do nothing.
    finished: bool,
}

impl State {
    fn check(&mut self) {
        if self.finished {
            return;
        }
        self.check_identities();
        let new: Vec<PathBuf> = self
            .candidates
            .difference(&self.existing)
            .filter(|path| exists(&self.tree, path))
            .cloned()
            .collect();
        for path in new {
            self.take(&path, What::New);
        }
        let gitdirs: Vec<PathBuf> = self
            .index
            .gitdirs
            .iter()
            .filter(|gitdir| !below_any(&self.detached, gitdir))
            .cloned()
            .collect();
        for gitdir in gitdirs {
            for found in nested_gitdirs(&gitdir) {
                if !self.index.gitdirs.contains(&found) {
                    self.take(&found, What::Gitdir);
                }
            }
        }
        let top = self.workspace.join(".git");
        if !self.top_existed && exists(&self.tree, &top) {
            self.take(&top, What::Repository);
        }
        let State {
            tree,
            snapshot,
            quarantine,
            detached,
            findings,
            ..
        } = self;
        undo(
            snapshot,
            tree,
            quarantine,
            |path| below_any(detached, path),
            &mut findings.after,
        );
    }

    /// Quarantines whatever replaced a `.git` entry, gitdir or link, and
    /// puts a symlink or gitfile back. Notes the ones that are gone.
    fn check_identities(&mut self) {
        let paths: Vec<PathBuf> = self.identities.keys().cloned().collect();
        for path in paths {
            let now = Identity::of(&self.tree, &path);
            let Some(tracked) = self.identities.get(&path) else {
                continue;
            };
            if now == tracked.expected {
                continue;
            }
            let original = tracked.original.clone();
            if now == Identity::Missing {
                self.gone(&path);
                continue;
            }
            if now == Identity::Unreachable {
                let outcome = unreachable(&self.tree, &path);
                self.after(&path, What::Unreachable, outcome);
                self.detached.insert(path.clone());
                self.expect(&path, now);
                continue;
            }
            let moved = match self.quarantine.take(&path) {
                Ok(moved) => moved,
                Err(err) if nothing_there(&err) => {
                    self.gone(&path);
                    continue;
                }
                Err(err) => {
                    let outcome = Outcome::Failed(format!("could not move it: {err}"));
                    self.after(&path, What::Replaced, outcome);
                    self.detached.insert(path.clone());
                    self.expect(&path, now);
                    continue;
                }
            };
            let restored = match &original {
                Identity::Symlink { target } => Some(
                    self.tree
                        .parent(&path)
                        .and_then(|(dir, name)| dir.symlink(target, &name)),
                ),
                Identity::Inode { dir: false, .. } if self.snapshot.saved(&path) => {
                    Some(self.snapshot.restore(&self.tree, &path))
                }
                _ => None,
            };
            let outcome = match restored {
                None => Outcome::Moved(moved),
                Some(Ok(())) => Outcome::Restored(Some(moved)),
                Some(Err(err)) => Outcome::Failed(format!(
                    "moved to {}, but could not put the earlier version back: {err}",
                    moved.display()
                )),
            };
            if matches!(outcome, Outcome::Restored(_)) {
                self.detached.remove(&path);
            } else {
                self.detached.insert(path.clone());
            }
            self.expect(&path, Identity::of(&self.tree, &path));
            self.after(&path, What::Replaced, outcome);
        }
    }

    /// `path`, a `.git` entry, gitdir or link, is no longer where it was.
    fn gone(&mut self, path: &Path) {
        self.findings.gone.insert(path.to_path_buf());
        self.detached.insert(path.to_path_buf());
        self.expect(path, Identity::Missing);
    }

    fn expect(&mut self, path: &Path, identity: Identity) {
        if let Some(tracked) = self.identities.get_mut(path) {
            tracked.expected = identity;
        }
    }

    fn after(&mut self, path: &Path, what: What, outcome: Outcome) {
        let finding = Finding {
            path: path.to_path_buf(),
            what,
            outcome,
        };
        push(&mut self.findings.after, Some(finding));
    }

    fn take(&mut self, path: &Path, what: What) {
        push(
            &mut self.findings.after,
            take(&mut self.quarantine, path, what),
        );
    }

    /// Where things stand now that the command has ended and the guard has
    /// undone what it could: the gitdirs still where they were, the
    /// protected names in them and at the top of the workspace, and in the
    /// basic tier the protected files and gitfiles as they are now.
    fn keep(&self, survivors: bool) -> Kept {
        let gitdirs: BTreeSet<PathBuf> = self
            .index
            .gitdirs
            .iter()
            .filter(|gitdir| !below_any(&self.detached, gitdir))
            .filter(|gitdir| self.tree.stat(gitdir).is_ok_and(|s| s.kind == Kind::Dir))
            .cloned()
            .collect();
        let candidates = candidates(&self.workspace, &gitdirs);
        let existing: BTreeSet<PathBuf> = candidates
            .iter()
            .filter(|path| may_exist(&self.tree, path))
            .cloned()
            .collect();
        let snapshot = self.save_all.then(|| {
            let dot_gits: BTreeSet<PathBuf> = self
                .index
                .dot_gits
                .iter()
                .filter(|dot_git| !below_any(&self.detached, dot_git))
                .cloned()
                .collect();
            let roots: Vec<PathBuf> = existing
                .iter()
                .cloned()
                .chain(gitfiles(&self.tree, &dot_gits))
                .collect();
            Snapshot::take(&self.tree, &roots, true)
        });
        Kept {
            gitdirs,
            candidates,
            existing,
            snapshot,
            survivors,
            found: Vec::new(),
            quarantine: None,
        }
    }
}

/// Where things stood when a command's guard finished, for the checks until
/// the next command begins.
#[derive(Debug)]
struct Kept {
    gitdirs: BTreeSet<PathBuf>,
    candidates: BTreeSet<PathBuf>,
    existing: BTreeSet<PathBuf>,
    /// The protected files and gitfiles, after the guard's own restores
    /// (the basic tier only).
    snapshot: Option<Snapshot>,
    /// Whether processes a sandboxed command started were running when the
    /// guard finished, or at any check since.
    survivors: bool,
    /// What the checks since found.
    found: Vec<Finding>,
    /// Where the checks between commands move things.
    quarantine: Option<Quarantine>,
}

impl Kept {
    /// Moves protected names planted since the command ended to quarantine,
    /// and if processes it left running may have changed things, restores
    /// the protected files and gitfiles.
    fn check(&mut self, tree: &Tree, quarantine: &mut Quarantine, survivors: bool) {
        self.survivors |= survivors;
        let new: Vec<PathBuf> = self
            .candidates
            .difference(&self.existing)
            .filter(|path| exists(tree, path))
            .cloned()
            .collect();
        for path in new {
            push(&mut self.found, take(quarantine, &path, What::New));
        }
        if self.survivors
            && let Some(snapshot) = &self.snapshot
        {
            undo(snapshot, tree, quarantine, |_| false, &mut self.found);
        }
    }
}

/// Undoes each difference from `snapshot`, except below the paths `skip`
/// picks: moves what is new or changed to quarantine, and restores what was
/// there.
fn undo(
    snapshot: &Snapshot,
    tree: &Tree,
    quarantine: &mut Quarantine,
    skip: impl Fn(&Path) -> bool,
    found: &mut Vec<Finding>,
) {
    snapshot.walk(tree, skip, |path, difference| {
        let finding = match difference {
            Difference::Added => take(quarantine, path, What::Added),
            Difference::Changed => match quarantine.take(path) {
                Ok(moved) => Some(restore(snapshot, tree, path, What::Changed, Some(moved))),
                Err(err) if nothing_there(&err) => {
                    Some(restore(snapshot, tree, path, What::Changed, None))
                }
                Err(err) => Some(Finding {
                    path: path.to_path_buf(),
                    what: What::Changed,
                    outcome: Outcome::Failed(format!("could not move it: {err}")),
                }),
            },
            // With the directory it was in gone as well, there is nowhere to
            // restore it to; what happened to the directory is reported.
            Difference::Missing if tree.parent(path).is_err_and(|err| absent(&err)) => None,
            Difference::Missing => Some(restore(snapshot, tree, path, What::Deleted, None)),
            Difference::Permissions => Some(restore(snapshot, tree, path, What::Changed, None)),
            Difference::Unreachable => Some(Finding {
                path: path.to_path_buf(),
                what: What::Unreachable,
                outcome: unreachable(tree, path),
            }),
        };
        push(found, finding);
    });
}

/// Restores `path` from `snapshot`; what was there was `moved` to
/// quarantine, if anything.
fn restore(
    snapshot: &Snapshot,
    tree: &Tree,
    path: &Path,
    what: What,
    moved: Option<PathBuf>,
) -> Finding {
    let outcome = match snapshot.restore(tree, path) {
        Ok(()) => Outcome::Restored(moved),
        Err(err) => match moved {
            Some(to) => Outcome::Failed(format!(
                "moved to {}, but could not restore the earlier version: {err}",
                to.display()
            )),
            None => Outcome::Failed(format!("could not restore the earlier version: {err}")),
        },
    };
    Finding {
        path: path.to_path_buf(),
        what,
        outcome,
    }
}

/// Moves `path` to quarantine, if something is there.
fn take(quarantine: &mut Quarantine, path: &Path, what: What) -> Option<Finding> {
    let outcome = match quarantine.take(path) {
        Ok(to) => Outcome::Moved(to),
        Err(err) if nothing_there(&err) => return None,
        Err(err) => Outcome::Failed(format!("could not move it: {err}")),
    };
    Some(Finding {
        path: path.to_path_buf(),
        what,
        outcome,
    })
}

/// Whether `err` means nothing is at the path. A symlink on the way is not
/// nothing: something is there, and it cannot be reached safely.
fn nothing_there(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    )
}

/// Every path a protected name could appear at, given `gitdirs`.
fn candidates(workspace: &Path, gitdirs: &BTreeSet<PathBuf>) -> BTreeSet<PathBuf> {
    let mut candidates: BTreeSet<PathBuf> = gitdirs
        .iter()
        .flat_map(|gitdir| GITDIR_PROTECTED.iter().map(move |name| gitdir.join(name)))
        .collect();
    candidates.extend(WORKSPACE_PROTECTED.iter().map(|name| workspace.join(name)));
    candidates
}

/// The `.git` entries in `dot_gits` that are gitfiles.
fn gitfiles<'a>(
    tree: &'a Tree,
    dot_gits: &'a BTreeSet<PathBuf>,
) -> impl Iterator<Item = PathBuf> + 'a {
    dot_gits
        .iter()
        .filter(|path| tree.stat(path).is_ok_and(|stat| stat.kind == Kind::File))
        .cloned()
}

/// Whether anything is at `path`, reached without following a symlink.
fn exists(tree: &Tree, path: &Path) -> bool {
    tree.stat(path).is_ok()
}

/// Whether something may be at `path`: it is there, or harness cannot look
/// (a directory above it cannot be read). What harness could not see is not
/// taken for new once it can.
fn may_exist(tree: &Tree, path: &Path) -> bool {
    tree.stat(path).map_or_else(|err| !absent(&err), |_| true)
}

/// Why harness cannot look at `path`.
fn unreachable(tree: &Tree, path: &Path) -> Outcome {
    let why = match tree.stat(path) {
        Err(err) => err.to_string(),
        Ok(_) => "it changed while harness looked at it".into(),
    };
    Outcome::Failed(why)
}

/// Whether `path` is one of `paths` or below one.
fn below_any(paths: &BTreeSet<PathBuf>, path: &Path) -> bool {
    paths.iter().any(|above| path.starts_with(above))
}

/// See [`WatchHandle::dirs`].
fn watched_dirs(
    workspace: &Path,
    gitdirs: &BTreeSet<PathBuf>,
    snapshot: Option<&Snapshot>,
) -> Vec<PathBuf> {
    let mut dirs = vec![workspace.to_path_buf()];
    for gitdir in gitdirs {
        dirs.push(gitdir.clone());
        dirs.push(gitdir.join("worktrees"));
        dirs.push(gitdir.join("modules"));
    }
    if let Some(snapshot) = snapshot {
        dirs.extend(snapshot.dirs().map(Path::to_path_buf));
    }
    dirs.retain(|dir| std::fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir()));
    dirs.sort();
    dirs.dedup();
    dirs
}

/// See [`WatchHandle::relevant`].
fn relevant(
    workspace: &Path,
    gitdirs: &BTreeSet<PathBuf>,
    snapshot: Option<&Snapshot>,
    dir: &Path,
    name: &OsStr,
) -> bool {
    let named = |names: &[&str]| names.iter().any(|n| OsStr::new(n) == name);
    if dir == workspace {
        return named(&[".git"]) || named(&WORKSPACE_PROTECTED);
    }
    if gitdirs.contains(dir) {
        return named(&GITDIR_PROTECTED) || named(&["worktrees", "modules"]);
    }
    let in_gitdir = |sub: &str| {
        dir.file_name() == Some(OsStr::new(sub))
            && dir.parent().is_some_and(|p| gitdirs.contains(p))
    };
    in_gitdir("worktrees")
        || in_gitdir("modules")
        || snapshot.is_some_and(|snapshot| snapshot.covers(&dir.join(name)))
}

/// `path` with the part that exists spelled canonically, as the walks spell
/// the paths they meet.
fn canonical_as_far_as_it_exists(path: &Path) -> PathBuf {
    let mut rest = Vec::new();
    let mut existing = path;
    loop {
        if let Ok(canonical) = existing.canonicalize() {
            return rest
                .iter()
                .rev()
                .fold(canonical, |path, name| path.join(name));
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name);
                existing = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What a `.git` entry, gitdir or link was at the start of the command, and
/// what the guard expects it to be now.
#[derive(Debug)]
struct Tracked {
    original: Identity,
    expected: Identity,
}

impl Tracked {
    fn new(identity: Identity) -> Tracked {
        Tracked {
            original: identity.clone(),
            expected: identity,
        }
    }
}

/// What an entry is, for noticing that it was replaced.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Identity {
    /// Nothing is there, or nothing reachable without a symlink.
    Missing,
    /// It cannot be looked at: a directory above it cannot be read, say.
    Unreachable,
    Symlink {
        target: PathBuf,
    },
    Inode {
        dir: bool,
        dev: u64,
        ino: u64,
    },
}

impl Identity {
    fn of(tree: &Tree, path: &Path) -> Identity {
        let found = tree.parent(path).and_then(|(parent, name)| {
            let stat = parent.stat(&name)?;
            Ok((parent, name, stat))
        });
        match found {
            Err(err) if absent(&err) => Identity::Missing,
            Err(_) => Identity::Unreachable,
            Ok((parent, name, stat)) if stat.kind == Kind::Symlink => parent
                .read_link(&name)
                .map_or(Identity::Unreachable, |target| Identity::Symlink { target }),
            Ok((_, _, stat)) => Identity::Inode {
                dir: stat.kind == Kind::Dir,
                dev: stat.dev,
                ino: stat.ino,
            },
        }
    }
}
