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
//! - indexes the workspace ([`discover`](crate::gitmeta::discover), with the
//!   ignore rules read once per session: [`GuardSession::prime`]) and records
//!   which protected names exist, and in the basic tier saves the protected
//!   files;
//! - while it runs (a watcher calls [`WatchHandle::check`]) and after it
//!   ends, moves to quarantine every new protected name, new gitdir and
//!   replaced `.git`, and restores changed protected files;
//! - after it ends, also walks the workspace for new `.git` entries.
//!
//! What the next command's checks compare against is what the guard restores
//! to, the state before the command, never what is on disk after it: a
//! process the command left running may write while the guard finishes.
//!
//! Nothing is ever deleted: it is moved to `<quarantine root>/<time>-<pid>-<n>/`
//! at its path relative to the workspace, with every `.git` stored as
//! `dot-git`. Nothing in the workspace is reached through a symlink either
//! ([`nofollow`]): a command, or a process it left running, can swap a
//! directory for one at any moment. When a directory's owner lost the
//! permissions the guard needs, the guard gives them back and says so.

mod nofollow;
mod quarantine;
mod report;
mod snapshot;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fmt;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use harness_core::tool::GuardReport;

use crate::gitmeta::{
    Budget, GITDIR_PROTECTED, GitIndex, IgnoreRules, WORKSPACE_PROTECTED, discover_with_budget,
    nested_gitdirs, read_ignore_rules,
};
use nofollow::{Dir, Kind, Stat, Tree, absent};
use quarantine::Quarantine;
use report::{Finding, Findings, List, Outcome, What};
use snapshot::{Difference, Next, Snapshot};

/// How many changes one check undoes (moves, restores). It stops there, and
/// reports how many more there were: the check holds the guard's lock.
const MAX_CHANGES: usize = 10_000;

/// How many linked-worktree and submodule gitdirs the guard records beyond
/// the ones the scan found.
const MAX_NESTED: usize = 10_000;

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
    #[cfg(test)]
    hooks: Mutex<TestHooks>,
}

impl fmt::Debug for GuardSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GuardSession")
            .field("quarantine_root", &self.quarantine_root)
            .field("workspaces", &self.workspaces)
            .finish_non_exhaustive()
    }
}

/// What tests change about a session.
#[cfg(test)]
#[derive(Default)]
struct TestHooks {
    /// The budget of every scan, in place of the default one.
    budget: Option<Budget>,
    /// Runs once, right after the next scan of a workspace.
    after_scan: Option<Box<dyn FnOnce() + Send>>,
    /// Paths whose moves to quarantine fail.
    stuck: BTreeSet<PathBuf>,
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
    /// A session whose quarantined entries go below `quarantine_root`, which
    /// is spelled canonically here, while harness starts: afterwards it is
    /// reached without following a symlink.
    pub fn new(quarantine_root: &Path) -> Arc<GuardSession> {
        let default: SurvivorProbe = Arc::new(|| false);
        Arc::new(GuardSession {
            quarantine_root: canonical_as_far_as_it_exists(quarantine_root),
            probe: Mutex::new(default),
            workspaces: Mutex::default(),
            #[cfg(test)]
            hooks: Mutex::default(),
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

    /// Scans `workspace` with the session's rules.
    fn scan(&self, workspace: &Path, rules: &IgnoreRules) -> GitIndex {
        #[cfg(test)]
        let budget = lock(&self.hooks).budget.clone().unwrap_or(Budget::DEFAULT);
        #[cfg(not(test))]
        let budget = Budget::DEFAULT;
        let index = discover_with_budget(workspace, Some(&self.quarantine_root), rules, budget);
        #[cfg(test)]
        {
            let after_scan = lock(&self.hooks).after_scan.take();
            if let Some(after_scan) = after_scan {
                after_scan();
            }
        }
        index
    }

    fn quarantine(&self, workspace: &Path) -> Quarantine {
        let quarantine = Quarantine::new(&self.quarantine_root, workspace);
        #[cfg(test)]
        let quarantine = {
            let mut quarantine = quarantine;
            quarantine.stuck = lock(&self.hooks).stuck.clone();
            quarantine
        };
        quarantine
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
        let mut quarantine = self.quarantine(workspace);
        let mut findings = Findings::default();
        if let Some(mut kept) = self.take_kept(workspace) {
            let survivors = self.survivors();
            kept.check(&tree, &mut quarantine, survivors);
            findings.before = kept.found;
            findings.left_before = kept.left_over;
        }

        let index = self.scan(workspace, &rules);
        placeholders(&index);
        // What an incomplete scan did not reach is still known: the
        // linked-worktree and submodule gitdirs in each gitdir it found.
        let nested = nested_in(&index.gitdirs);
        let gitdirs: BTreeSet<PathBuf> = index.gitdirs.union(&nested).cloned().collect();
        let candidates = candidates(workspace, &gitdirs);
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
        let top_existed = may_exist(&tree, &workspace.join(".git"));
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
                gitdirs,
                nested,
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
        let mut quarantine = self.quarantine(workspace);
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
        if let Some(used) = kept.quarantine.take() {
            quarantine = used;
        }
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
    /// entries, keeps what the checks before the next command compare
    /// against, and says what was done, if anything.
    pub fn finish(self) -> Option<GuardReport> {
        let mut state = lock(&self.state);
        state.check();
        let rules = self.session.rules(&state.workspace);
        let now = self.session.scan(&state.workspace, &rules);
        let new: Vec<PathBuf> = now
            .dot_gits
            .difference(&state.index.dot_gits)
            .cloned()
            .collect();
        if state.index.incomplete {
            // Both scans may have missed different parts: what the second
            // found may have been there all along.
            state.findings.unchecked.extend(new);
        } else {
            // What the first scan did not find is new, whether or not the
            // second one was complete.
            let State {
                tree,
                quarantine,
                findings,
                ..
            } = &mut *state;
            let mut pass = Pass::new(tree, quarantine);
            for dot_git in new {
                let taken = pass.take(&dot_git, What::Repository);
                findings.after.push(taken);
            }
            findings.left_after += pass.refused;
            pass.report_unlocked(&mut findings.after);
            findings.uncheckable = now.incomplete;
        }
        state.finished = true;
        state.quarantine.close();
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
                watched_dirs(&state.tree, &state.gitdirs, Some(&state.snapshot))
            }
            Watched::Between {
                session,
                workspace,
                generation,
            } => session
                .with_kept(workspace, *generation, |kept| {
                    watched_dirs(&Tree::new(workspace), &kept.gitdirs, kept.snapshot.as_ref())
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
                    &state.gitdirs,
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
    /// The gitdirs checked: the ones indexed, and [`nested`](Self::nested).
    gitdirs: BTreeSet<PathBuf>,
    /// The linked-worktree and submodule gitdirs in the indexed gitdirs when
    /// the command started, that the index lacks: an incomplete scan may not
    /// have reached them.
    nested: BTreeSet<PathBuf>,
    /// Every path a protected name could appear at: the protected names in
    /// each gitdir, and `.harness` and `HEAD` at the top of the workspace.
    candidates: BTreeSet<PathBuf>,
    /// The candidates that existed (or could not be looked at) when the
    /// command started.
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
        let State {
            workspace,
            tree,
            index,
            gitdirs,
            nested,
            candidates,
            existing,
            identities,
            detached,
            top_existed,
            snapshot,
            quarantine,
            findings,
            ..
        } = self;
        let mut pass = Pass::new(tree, quarantine);
        check_identities(&mut pass, identities, snapshot, detached, findings);
        let new: Vec<PathBuf> = candidates
            .difference(existing)
            .filter(|path| pass.exists(path))
            .cloned()
            .collect();
        for path in new {
            let taken = pass.take(&path, What::New);
            findings.after.push(taken);
        }
        for gitdir in gitdirs.iter().filter(|gitdir| !below_any(detached, gitdir)) {
            for found in nested_gitdirs(gitdir) {
                if index.gitdirs.contains(&found) || nested.contains(&found) {
                    continue;
                }
                if index.incomplete {
                    // The scan may not have reached it before the command
                    // either.
                    findings.unchecked.insert(found);
                } else {
                    let taken = pass.take(&found, What::Gitdir);
                    findings.after.push(taken);
                }
            }
        }
        let top = workspace.join(".git");
        if !*top_existed && pass.exists(&top) {
            let taken = pass.take(&top, What::Repository);
            findings.after.push(taken);
        }
        pass.undo(
            snapshot,
            |path| below_any(detached, path),
            &mut findings.after,
        );
        findings.left_after = pass.refused;
        pass.report_unlocked(&mut findings.after);
    }

    /// What the checks until the next command compare against: the state
    /// before this command, which the guard restored, never what is on disk
    /// now. The names that existed before the command and still may; the
    /// gitdirs that were directories then and are still the ones indexed;
    /// and in the basic tier the snapshot taken before the command.
    fn keep(&mut self, survivors: bool) -> Kept {
        let gitdirs: BTreeSet<PathBuf> = self
            .gitdirs
            .iter()
            .filter(|gitdir| !below_any(&self.detached, gitdir))
            .filter(|gitdir| {
                self.identities.get(*gitdir).is_none_or(|tracked| {
                    matches!(tracked.original, Identity::Inode { dir: true, .. })
                })
            })
            .cloned()
            .collect();
        let candidates = candidates(&self.workspace, &gitdirs);
        // An intersection can only shrink: nothing planted or left
        // unmoved since the command started gets in.
        let existing: BTreeSet<PathBuf> = self
            .existing
            .intersection(&candidates)
            .filter(|path| may_exist(&self.tree, path))
            .cloned()
            .collect();
        let snapshot = self.save_all.then(|| std::mem::take(&mut self.snapshot));
        Kept {
            gitdirs,
            candidates,
            existing,
            snapshot,
            detached: self.detached.clone(),
            survivors,
            unfinished: self.findings.left_after > 0,
            found: List::default(),
            left_over: 0,
            quarantine: None,
        }
    }
}

/// Quarantines whatever replaced a `.git` entry, gitdir or link, and puts a
/// symlink or gitfile back. Notes the ones that are gone.
fn check_identities(
    pass: &mut Pass,
    identities: &mut BTreeMap<PathBuf, Tracked>,
    snapshot: &Snapshot,
    detached: &mut BTreeSet<PathBuf>,
    findings: &mut Findings,
) {
    for (path, tracked) in identities.iter_mut() {
        let now = pass.identity(path);
        if now == tracked.expected {
            continue;
        }
        let finding = |what, outcome| {
            Some(Finding {
                path: path.clone(),
                what,
                outcome,
            })
        };
        match now {
            Identity::Missing => {
                findings.gone.insert(path.clone());
                detached.insert(path.clone());
                tracked.expected = now;
                continue;
            }
            Identity::Unreachable => {
                findings
                    .after
                    .push(finding(What::Unreachable, pass.unreachable(path)));
                detached.insert(path.clone());
                tracked.expected = now;
                continue;
            }
            Identity::Symlink { .. } | Identity::Inode { .. } => {}
        }
        let moved = match pass.move_out(path) {
            None => continue,
            Some(Ok(moved)) => moved,
            Some(Err(err)) if nothing_there(&err) => {
                findings.gone.insert(path.clone());
                detached.insert(path.clone());
                tracked.expected = Identity::Missing;
                continue;
            }
            Some(Err(err)) => {
                let outcome = Outcome::Failed(format!("could not move it: {err}"));
                findings.after.push(finding(What::Replaced, outcome));
                detached.insert(path.clone());
                tracked.expected = now;
                continue;
            }
        };
        let restored = match &tracked.original {
            Identity::Symlink { target } => Some(pass.retry(path, |pass| {
                let (dir, name) = pass.tree.parent(path)?;
                dir.symlink(target, &name)
            })),
            Identity::Inode { dir: false, .. } if snapshot.saved(path) => {
                Some(pass.retry(path, |pass| snapshot.restore(pass.tree, path)))
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
            detached.remove(path);
        } else {
            detached.insert(path.clone());
        }
        tracked.expected = Identity::of(pass.tree, path);
        findings.after.push(finding(What::Replaced, outcome));
    }
}

/// One check: what it may still change, and the directories whose owner
/// permissions it gave back on the way.
struct Pass<'a> {
    tree: &'a Tree,
    quarantine: &'a mut Quarantine,
    /// Changes this check may still make.
    left: usize,
    /// Changes it did not make, past [`MAX_CHANGES`].
    refused: usize,
    unlocked: BTreeSet<PathBuf>,
}

impl<'a> Pass<'a> {
    fn new(tree: &'a Tree, quarantine: &'a mut Quarantine) -> Pass<'a> {
        quarantine.forget_sources();
        Pass {
            tree,
            quarantine,
            left: MAX_CHANGES,
            refused: 0,
            unlocked: BTreeSet::new(),
        }
    }

    /// Whether this check may make one more change.
    fn allow(&mut self) -> bool {
        match self.left.checked_sub(1) {
            Some(left) => {
                self.left = left;
                true
            }
            None => {
                self.refused += 1;
                false
            }
        }
    }

    /// `op` on `path`, and once more if it was refused for want of
    /// permission and the guard could give the owner of a directory on the
    /// way the permissions back.
    fn retry<T>(
        &mut self,
        path: &Path,
        mut op: impl FnMut(&mut Self) -> io::Result<T>,
    ) -> io::Result<T> {
        let first = op(self);
        if let Err(err) = &first
            && denied(err)
            && self.unlock(path)
        {
            return op(self);
        }
        first
    }

    /// Gives the owner read, write and search permission back on each
    /// directory from the workspace down to `path`, `path` included, where
    /// they lost any of them. Whether it changed anything.
    fn unlock(&mut self, path: &Path) -> bool {
        let root = self.tree.root();
        let Ok(rel) = path.strip_prefix(root) else {
            return false;
        };
        let mut changed = false;
        // The workspace itself, by its trusted path.
        if let Ok(meta) = std::fs::symlink_metadata(root)
            && meta.is_dir()
        {
            let mode = meta.permissions().mode() & 0o7777;
            if mode & 0o700 != 0o700
                && std::fs::set_permissions(root, PermissionsExt::from_mode(mode | 0o700)).is_ok()
            {
                self.unlocked.insert(root.to_path_buf());
                changed = true;
            }
        }
        let Ok(mut dir) = Dir::open(root) else {
            return changed;
        };
        let mut at = root.to_path_buf();
        for name in rel {
            let Ok(Stat {
                kind: Kind::Dir,
                mode,
                ..
            }) = dir.stat(name)
            else {
                break;
            };
            at.push(name);
            if mode & 0o700 != 0o700 {
                if dir.chmod(name, mode | 0o700).is_err() {
                    break;
                }
                self.unlocked.insert(at.clone());
                changed = true;
            }
            match dir.open_dir(name) {
                Ok(next) => dir = next,
                Err(_) => break,
            }
        }
        changed
    }

    /// Whether anything is at `path`, reached without following a symlink,
    /// giving the owner permissions back where that is what stops a look.
    fn exists(&mut self, path: &Path) -> bool {
        self.retry(path, |pass| pass.tree.stat(path)).is_ok()
    }

    /// What `path` is now, giving the owner permissions back where that is
    /// what stops a look.
    fn identity(&mut self, path: &Path) -> Identity {
        let identity = Identity::of(self.tree, path);
        if identity == Identity::Unreachable && self.unlock(path) {
            Identity::of(self.tree, path)
        } else {
            identity
        }
    }

    /// Why harness cannot look at `path`.
    fn unreachable(&self, path: &Path) -> Outcome {
        let why = match self.tree.stat(path) {
            Err(err) => err.to_string(),
            Ok(_) => "it changed while harness looked at it".into(),
        };
        Outcome::Failed(why)
    }

    /// Moves `path` to quarantine. `None` when this check may make no more
    /// changes.
    fn move_out(&mut self, path: &Path) -> Option<io::Result<PathBuf>> {
        if !self.allow() {
            return None;
        }
        Some(self.retry(path, |pass| pass.quarantine.take(path)))
    }

    /// Moves `path` to quarantine, if something is there.
    fn take(&mut self, path: &Path, what: What) -> Option<Finding> {
        let outcome = match self.move_out(path)? {
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

    /// Restores `path` from `snapshot`; what was there was `moved` to
    /// quarantine, if anything (which counted as the change).
    fn restore(
        &mut self,
        snapshot: &Snapshot,
        path: &Path,
        what: What,
        moved: Option<PathBuf>,
    ) -> Option<Finding> {
        if moved.is_none() && !self.allow() {
            return None;
        }
        let outcome = match self.retry(path, |pass| snapshot.restore(pass.tree, path)) {
            Ok(()) => Outcome::Restored(moved),
            Err(err) => match moved {
                Some(to) => Outcome::Failed(format!(
                    "moved to {}, but could not restore the earlier version: {err}",
                    to.display()
                )),
                None => Outcome::Failed(format!("could not restore the earlier version: {err}")),
            },
        };
        Some(Finding {
            path: path.to_path_buf(),
            what,
            outcome,
        })
    }

    /// Undoes each difference from `snapshot`, except below the paths `skip`
    /// picks: moves what is new or changed to quarantine, and restores what
    /// was there.
    fn undo(&mut self, snapshot: &Snapshot, skip: impl Fn(&Path) -> bool, found: &mut List) {
        let tree = self.tree;
        snapshot.walk(tree, skip, |path, difference| {
            let finding = match difference {
                Difference::Unreachable => {
                    if self.unlock(path) {
                        return Next::Again;
                    }
                    Some(Finding {
                        path: path.to_path_buf(),
                        what: What::Unreachable,
                        outcome: self.unreachable(path),
                    })
                }
                Difference::Added => self.take(path, What::Added),
                Difference::Changed => match self.move_out(path) {
                    None => None,
                    Some(Ok(moved)) => self.restore(snapshot, path, What::Changed, Some(moved)),
                    Some(Err(err)) if nothing_there(&err) => {
                        self.restore(snapshot, path, What::Changed, None)
                    }
                    Some(Err(err)) => Some(Finding {
                        path: path.to_path_buf(),
                        what: What::Changed,
                        outcome: Outcome::Failed(format!("could not move it: {err}")),
                    }),
                },
                // With the directory it was in gone as well, there is nowhere
                // to restore it to; what happened to the directory is
                // reported.
                Difference::Missing if tree.parent(path).is_err_and(|err| absent(&err)) => None,
                Difference::Missing => self.restore(snapshot, path, What::Deleted, None),
                Difference::Permissions => self.restore(snapshot, path, What::Changed, None),
            };
            found.push(finding);
            Next::Go
        });
    }

    /// Reports the directories whose owner permissions this check gave back.
    fn report_unlocked(&mut self, found: &mut List) {
        for dir in std::mem::take(&mut self.unlocked) {
            found.push(Some(Finding {
                path: dir,
                what: What::Locked,
                outcome: Outcome::Unlocked,
            }));
        }
    }
}

/// What the checks until the next command compare against: the state
/// before the last command, which its guard restored.
#[derive(Debug)]
struct Kept {
    gitdirs: BTreeSet<PathBuf>,
    candidates: BTreeSet<PathBuf>,
    existing: BTreeSet<PathBuf>,
    /// The protected files and gitfiles before the command (the basic tier
    /// only).
    snapshot: Option<Snapshot>,
    /// Below these, the snapshot is not compared: see [`State::detached`].
    detached: BTreeSet<PathBuf>,
    /// Whether processes a sandboxed command started were running when the
    /// guard finished, or at any check since.
    survivors: bool,
    /// Whether the last check stopped at [`MAX_CHANGES`] with changes left:
    /// the next one goes on with them.
    unfinished: bool,
    /// What the checks since found.
    found: List,
    /// Changes the last check did not make, past [`MAX_CHANGES`].
    left_over: usize,
    /// Where the checks between commands move things.
    quarantine: Option<Quarantine>,
}

impl Kept {
    /// Moves protected names planted since the command ended to quarantine,
    /// and if processes it left running may have changed things, or the
    /// command's guard stopped with changes left, undoes the changes to the
    /// protected files and gitfiles.
    fn check(&mut self, tree: &Tree, quarantine: &mut Quarantine, survivors: bool) {
        self.survivors |= survivors;
        let mut pass = Pass::new(tree, quarantine);
        let new: Vec<PathBuf> = self
            .candidates
            .difference(&self.existing)
            .filter(|path| pass.exists(path))
            .cloned()
            .collect();
        for path in new {
            let taken = pass.take(&path, What::New);
            self.found.push(taken);
        }
        if (self.survivors || self.unfinished)
            && let Some(snapshot) = &self.snapshot
        {
            let detached = &self.detached;
            pass.undo(snapshot, |path| below_any(detached, path), &mut self.found);
            self.unfinished = pass.refused > 0;
        }
        self.left_over = pass.refused;
        pass.report_unlocked(&mut self.found);
    }
}

/// The linked-worktree and submodule gitdirs in `gitdirs`, and in those,
/// and so on, that `gitdirs` lacks: at most [`MAX_NESTED`].
fn nested_in(gitdirs: &BTreeSet<PathBuf>) -> BTreeSet<PathBuf> {
    let mut nested = BTreeSet::new();
    let mut pending: Vec<PathBuf> = gitdirs.iter().cloned().collect();
    while let Some(gitdir) = pending.pop() {
        for found in nested_gitdirs(&gitdir) {
            if nested.len() == MAX_NESTED {
                return nested;
            }
            if !gitdirs.contains(&found) && nested.insert(found.clone()) {
                pending.push(found);
            }
        }
    }
    nested
}

/// Whether `err` means nothing is at the path. A symlink on the way is not
/// nothing: something is there, and it cannot be reached safely.
fn nothing_there(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    )
}

/// Whether `err` is a refusal for want of permission.
fn denied(err: &io::Error) -> bool {
    matches!(err.raw_os_error(), Some(libc::EACCES | libc::EPERM))
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

/// Whether something may be at `path`: it is there, or harness cannot look
/// (a directory above it cannot be read). What harness could not see is not
/// taken for new once it can.
fn may_exist(tree: &Tree, path: &Path) -> bool {
    tree.stat(path).map_or_else(|err| !absent(&err), |_| true)
}

/// Whether `path` is one of `paths` or below one.
fn below_any(paths: &BTreeSet<PathBuf>, path: &Path) -> bool {
    paths.iter().any(|above| path.starts_with(above))
}

/// See [`WatchHandle::dirs`]. Each is looked at without following a symlink.
fn watched_dirs(
    tree: &Tree,
    gitdirs: &BTreeSet<PathBuf>,
    snapshot: Option<&Snapshot>,
) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for gitdir in gitdirs {
        dirs.push(gitdir.clone());
        dirs.push(gitdir.join("worktrees"));
        dirs.push(gitdir.join("modules"));
    }
    if let Some(snapshot) = snapshot {
        dirs.extend(snapshot.dirs().map(Path::to_path_buf));
    }
    dirs.retain(|dir| tree.stat(dir).is_ok_and(|stat| stat.kind == Kind::Dir));
    dirs.push(tree.root().to_path_buf());
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
