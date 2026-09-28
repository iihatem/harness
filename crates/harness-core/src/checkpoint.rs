//! Checkpoints: snapshots of the workspace in a shadow git repository in harness's data
//! directory. The shadow repository has its own `GIT_DIR` and index, so the user's repository,
//! index, branches and history are never touched, and directories that are not repositories work
//! too. git runs without the user's global and system configuration, and with settings and
//! attributes that outrank the shadow repository's own, so no filter, hook or file-system monitor
//! runs and files are stored and restored byte for byte.
//!
//! What a snapshot leaves out (large, ignored or unreadable files, and the directories always left
//! out) is recorded with it, so a restore never deletes what existed then, and never overwrites or
//! removes to make room what the snapshot taken just before it leaves out.

use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    ffi::{OsStr, OsString},
    io::Read,
    os::unix::{
        ffi::OsStrExt,
        fs::{OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    process::Command,
    sync::Mutex,
    time::{Duration, Instant, SystemTime},
};

use crate::subprocess::output_within;

/// Files larger than this are left out of snapshots, and a rewind leaves them alone.
pub const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024;
/// A snapshot that takes longer than this is abandoned.
pub const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long restoring files may take.
const RESTORE_TIMEOUT: Duration = Duration::from_secs(120);
/// Always left out of snapshots, besides what `.gitignore` files exclude. git never walks past
/// these directories, which keeps snapshots fast; [`EXCLUDED_DIRS`] keeps them out even where a
/// `.gitignore` brings them back.
const BUILTIN_EXCLUDES: &str = ".git\nnode_modules/\ntarget/\n/.harness/\n/HEAD\n";
/// Directories left out of snapshots wherever they are.
const EXCLUDED_DIRS: [&[u8]; 2] = [b"node_modules", b"target"];
/// The snapshots of a session that is gone are pruned once its last one is this old: a younger
/// one may belong to a session whose file is not written yet. Objects nothing reaches are pruned
/// once this old too, since another session may be writing a snapshot that will reach them.
const PRUNE_AGE: Duration = Duration::from_secs(24 * 60 * 60);
/// How many sessions one prune removes at most, and how long it may take.
const PRUNE_AT_MOST: usize = 20;
const PRUNE_TIMEOUT: Duration = Duration::from_secs(10);
/// The first item of a snapshot's record, which says how to read the rest.
const RECORD_MAGIC: &[u8] = b"harness snapshot record 1";
/// How many private files a snapshot records the mode of at most. (Past that many, the user's
/// umask most likely makes every new file private anyway.)
const MAX_PRIVATE_MODES: usize = 10_000;
/// How many paths, and how many bytes of them, one git command line carries at most.
const ARGS_PER_COMMAND: (usize, usize) = (500, 64 * 1024);
/// Names left out at the top of the workspace, in any case: harness's project settings, and a
/// `HEAD` that would make git take the workspace for a repository. The permission engine and the
/// sandbox protect them, so a rewind must neither recreate nor remove them.
const PROTECTED: [&[u8]; 2] = [b".harness", b"HEAD"];
/// Attributes that outrank the workspace's `.gitattributes`, so every file is stored and restored
/// byte for byte: no end-of-line conversion, keyword expansion, filter or re-encoding.
const ATTRIBUTES: &str = "* -text -ident -filter !eol !working-tree-encoding\n";
/// Settings given on every git command line, where they outrank the shadow repository's own
/// configuration: whatever that file says, git runs no file-system monitor or hook. (Commits are
/// made with `--no-gpg-sign`, so no signing program runs either.)
const OVERRIDES: [&str; 2] = ["core.fsmonitor=false", "core.hooksPath=/dev/null"];

#[derive(Debug, thiserror::Error)]
pub enum CheckpointError {
    #[error("git was not found on PATH")]
    GitMissing,
    #[error("a snapshot took longer than {} seconds", SNAPSHOT_TIMEOUT.as_secs())]
    TooSlow,
    #[error("git {command} failed: {message}")]
    Git { command: String, message: String },
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("{0:?} is not a session id")]
    InvalidSession(String),
    #[error("{0:?} is not a snapshot id")]
    InvalidCommit(String),
    #[error(
        "snapshot {0} has no record of the files it left out (an older harness took it), so restoring it could delete them"
    )]
    NoRecord(String),
    #[error(
        "this snapshot was taken for {}, not for this directory; run harness there to restore it",
        taken.display()
    )]
    OtherWorkspace { taken: PathBuf },
    #[error(
        "the checkpoint repository {} is inside {}, where commands can change it; move harness's data directory (HARNESS_HOME or XDG_DATA_HOME) out of it",
        gitdir.display(),
        root.display()
    )]
    Exposed { gitdir: PathBuf, root: PathBuf },
    #[error("{source}; snapshot {before} holds the files as they were just before")]
    Restore {
        before: String,
        source: Box<CheckpointError>,
    },
}

/// One session's checkpoints of a workspace.
///
/// In a repository, git's work tree is the repository's root, so the repository's own ignore rules
/// apply, and every command is limited to the workspace; paths in snapshots are relative to that
/// root.
#[derive(Debug)]
pub struct Checkpoints {
    git: PathBuf,
    gitdir: PathBuf,
    workspace: PathBuf,
    /// The repository the workspace is in, or the workspace outside one.
    root: PathBuf,
    /// The workspace relative to `root`; empty when they are the same.
    scope: Vec<u8>,
    /// The repository's own `info/exclude`, read as one more excludes file.
    repository_excludes: Option<PathBuf>,
    /// This session's index, so sessions in one project never share one.
    index: PathBuf,
    /// The index a restore builds its target in.
    scratch: PathBuf,
    /// Where this session writes the pathspecs it gives git, one per NUL-terminated line.
    pathspecs: PathBuf,
    /// Where this session writes a snapshot's record before git stores it.
    record: PathBuf,
    /// The ref that keeps this session's snapshots reachable.
    reference: String,
    session: String,
    /// The session's last snapshot, as this process took it.
    last: Mutex<Option<Last>>,
    timeout: Duration,
    prune_age: Duration,
    restore_timeout: Duration,
}

impl Checkpoints {
    /// Checkpoints of `workspace` for session `session_id`, kept in the shadow repository
    /// `gitdir`, which is created when missing.
    pub fn open(
        gitdir: &Path,
        workspace: &Path,
        session_id: &str,
    ) -> Result<Checkpoints, CheckpointError> {
        Checkpoints::open_with_git(Path::new("git"), gitdir, workspace, session_id)
    }

    /// Like [`open`](Self::open), running `git` instead of the `git` on `PATH`.
    pub fn open_with_git(
        git: &Path,
        gitdir: &Path,
        workspace: &Path,
        session_id: &str,
    ) -> Result<Checkpoints, CheckpointError> {
        // The id names this session's index and ref.
        if !crate::session::is_valid_id(session_id) {
            return Err(CheckpointError::InvalidSession(session_id.to_string()));
        }
        check_location(gitdir, &[workspace.to_path_buf()])?;
        let mut version = Command::new(git);
        version
            .arg("--version")
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default());
        match output_within(&mut version, SNAPSHOT_TIMEOUT) {
            Ok(Some(out)) if out.status.success() => {}
            // A git that cannot say its version in the time a whole snapshot may take is no use.
            Ok(None) => {
                return Err(CheckpointError::Git {
                    command: "--version".into(),
                    message: format!("no answer within {} seconds", SNAPSHOT_TIMEOUT.as_secs()),
                });
            }
            _ => return Err(CheckpointError::GitMissing),
        }
        let root = work_tree(workspace);
        let scope = workspace
            .strip_prefix(&root)
            .unwrap_or(Path::new(""))
            .as_os_str()
            .as_bytes()
            .to_vec();
        let checkpoints = Checkpoints {
            git: git.to_path_buf(),
            gitdir: gitdir.to_path_buf(),
            workspace: workspace.to_path_buf(),
            repository_excludes: repository_excludes(&root),
            root,
            scope,
            index: gitdir.join("indexes").join(session_id),
            scratch: gitdir.join("indexes").join(format!("restore-{session_id}")),
            pathspecs: gitdir.join("pathspecs").join(session_id),
            record: gitdir.join("records").join(session_id),
            reference: format!("refs/harness/{session_id}"),
            session: session_id.to_string(),
            last: Mutex::new(None),
            prune_age: PRUNE_AGE,
            timeout: SNAPSHOT_TIMEOUT,
            restore_timeout: RESTORE_TIMEOUT,
        };
        if !gitdir.join("HEAD").is_file() {
            std::fs::create_dir_all(gitdir)?;
            let mut init = checkpoints.command();
            init.env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .args(["init", "-q", "--bare"])
                .arg(gitdir);
            checkpoints.run(init, "init", Instant::now() + RESTORE_TIMEOUT)?;
            checkpoints.git(&["config", "gc.auto", "0"], RESTORE_TIMEOUT)?;
        }
        std::fs::create_dir_all(gitdir.join("info"))?;
        std::fs::write(gitdir.join("info/exclude"), BUILTIN_EXCLUDES)?;
        std::fs::write(gitdir.join("info/attributes"), ATTRIBUTES)?;
        std::fs::create_dir_all(gitdir.join("indexes"))?;
        std::fs::create_dir_all(gitdir.join("pathspecs"))?;
        std::fs::create_dir_all(gitdir.join("records"))?;
        // Start from the last index any session wrote, so unchanged files are not hashed again.
        let shared = gitdir.join("index");
        if !checkpoints.index.exists() && shared.is_file() {
            let _ = std::fs::copy(&shared, &checkpoints.index);
        }
        // That index, or this session's own from a run elsewhere in the repository, may hold
        // paths outside the workspace: they must never enter its snapshots, and a restore would
        // delete them.
        if !checkpoints.scope.is_empty() && checkpoints.index.exists() {
            let outside = [
                b":(top)".to_vec(),
                pathspec("top,exclude,literal", &checkpoints.scope),
            ];
            checkpoints.write_pathspecs(&outside)?;
            checkpoints.git_with_pathspecs(
                &["rm", "--cached", "-r", "-f", "-q", "--ignore-unmatch"],
                Instant::now() + RESTORE_TIMEOUT,
            )?;
        }
        Ok(checkpoints)
    }

    /// The directory these checkpoints are of.
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// Replaces the time a snapshot may take (for tests).
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Replaces how old a gone session's snapshots must be to be pruned (for tests).
    pub fn with_prune_age(mut self, age: Duration) -> Self {
        self.prune_age = age;
        self
    }

    /// Removes the snapshots of sessions that no longer exist (`live` says which session ids
    /// still do): their refs and their files here, then every object nothing reaches any more.
    /// A session whose last snapshot is less than a day old is kept, and one call removes at most
    /// 20 sessions within 10 seconds, so it stays quick. Returns how many sessions it removed.
    pub fn prune(&self, live: impl Fn(&str) -> bool) -> Result<usize, CheckpointError> {
        let deadline = Instant::now() + PRUNE_TIMEOUT;
        let listed = self.git_bytes(
            &[
                "for-each-ref",
                "--format=%(refname)%00%(committerdate:unix)",
                "refs/harness/",
            ],
            deadline,
        )?;
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let mut removed = 0;
        for line in String::from_utf8_lossy(&listed).lines() {
            let Some((name, date)) = line.split_once('\0') else {
                continue;
            };
            let Some(id) = name.strip_prefix("refs/harness/") else {
                continue;
            };
            let age = now.saturating_sub(date.parse().unwrap_or(now));
            if id == self.session
                || !crate::session::is_valid_id(id)
                || live(id)
                || age < self.prune_age.as_secs()
            {
                continue;
            }
            if removed == PRUNE_AT_MOST {
                break;
            }
            self.git(&["update-ref", "-d", name], remaining(deadline)?)?;
            for file in [
                self.gitdir.join("indexes").join(id),
                self.gitdir.join("indexes").join(format!("restore-{id}")),
                self.gitdir.join("pathspecs").join(id),
                self.gitdir.join("records").join(id),
                self.gitdir.join("records").join(format!("{id}.tree")),
            ] {
                let _ = std::fs::remove_file(file);
            }
            removed += 1;
        }
        if removed > 0 {
            // The index new sessions start from keeps what it names.
            let mut prune = self.command();
            prune
                .env("GIT_INDEX_FILE", self.gitdir.join("index"))
                .arg("prune")
                .arg(format!("--expire={}.seconds.ago", self.prune_age.as_secs()));
            self.run(prune, "prune", deadline)?;
        }
        Ok(removed)
    }

    /// Replaces the time a restore may take (for tests).
    pub fn with_restore_timeout(mut self, timeout: Duration) -> Self {
        self.restore_timeout = timeout;
        self
    }

    /// Snapshots the workspace and returns the commit. Files over [`MAX_FILE_SIZE`], git-ignored
    /// files (in a repository, by the repository's rules), `.git`, `node_modules` and `target`, and
    /// `.harness` and a `HEAD` at the top of the workspace are left out. A workspace whose first
    /// snapshot takes longer than the time limit gets no index to start from, so later snapshots
    /// there are no faster.
    pub fn snapshot(&self, message: &str) -> Result<String, CheckpointError> {
        self.unlock_after(self.snapshot_within(message, self.timeout))
    }

    /// Passes `result` on, first removing the locks git leaves when it is killed at a time limit
    /// (on this session's indexes and ref), so later snapshots and restores still work. Only this
    /// process uses them: the session is locked to it.
    fn unlock_after<T>(&self, result: Result<T, CheckpointError>) -> Result<T, CheckpointError> {
        let timed_out = match &result {
            Err(CheckpointError::TooSlow) => true,
            Err(CheckpointError::Restore { source, .. }) => {
                matches!(**source, CheckpointError::TooSlow)
            }
            _ => false,
        };
        if timed_out {
            for path in [
                &self.index,
                &self.scratch,
                &self.gitdir.join(&self.reference),
            ] {
                let mut lock = path.clone().into_os_string();
                lock.push(".lock");
                let _ = std::fs::remove_file(lock);
            }
        }
        result
    }

    fn snapshot_within(&self, message: &str, timeout: Duration) -> Result<String, CheckpointError> {
        let deadline = Instant::now() + timeout;
        match self.snapshot_until(message, deadline) {
            // The index, or the last snapshot, names objects pruned since (the index may come
            // from a session that is gone; a session whose file could not be written looks gone):
            // start again from an empty index and what the session's ref says.
            Err(CheckpointError::Git { command, .. })
                if command == "write-tree" || command == "commit-tree" =>
            {
                let _ = std::fs::remove_file(&self.index);
                *self.last.lock().expect("last snapshot lock") = None;
                self.snapshot_until(message, deadline)
            }
            result => result,
        }
    }

    fn snapshot_until(&self, message: &str, deadline: Instant) -> Result<String, CheckpointError> {
        // Everything in the index, and every file git would add.
        let within = self.within();
        let listed = self.list(
            &["--cached", "--others", "--exclude-standard", "--"],
            &within,
            deadline,
        )?;
        // What is left out whatever `.gitignore` files say: large files, and the directories
        // always left out. They are given to git as literal pathspecs, which outrank ignore rules
        // and match any name, a newline in it included.
        let mut excluded_dirs: BTreeSet<&[u8]> = BTreeSet::new();
        let mut large: BTreeSet<&[u8]> = BTreeSet::new();
        // git stores only whether a file is executable, and restores the rest from the umask:
        // the snapshot records the mode of files only their owner may read.
        let mut record = Record {
            workspace: Some(self.workspace.clone()),
            ..Record::default()
        };
        for path in &listed {
            if let Some(dir) = excluded_dir(path) {
                excluded_dirs.insert(dir);
                continue;
            }
            let Ok(meta) = std::fs::symlink_metadata(self.root.join(OsStr::from_bytes(path)))
            else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            let mode = meta.permissions().mode() & 0o7777;
            if meta.len() > MAX_FILE_SIZE {
                large.insert(path);
            } else if mode & 0o077 == 0 && record.modes.len() < MAX_PRIVATE_MODES {
                record.modes.insert(path.clone(), mode);
            }
        }
        let excluded: Vec<&[u8]> = excluded_dirs.iter().chain(&large).copied().collect();
        // Files an earlier snapshot holds that are now git-ignored or excluded leave the snapshots.
        let ignored = self.list(
            &["--cached", "--ignored", "--exclude-standard", "--"],
            &within,
            deadline,
        )?;
        let protected = self.protected();
        let held_protected = listed.iter().any(|path| self.is_protected(path));
        let leaving: Vec<Vec<u8>> = ignored
            .iter()
            .map(Vec::as_slice)
            .chain(excluded.iter().copied())
            .map(|path| pathspec("top,literal", path))
            .chain(
                protected
                    .iter()
                    .filter(|_| held_protected)
                    .map(|path| pathspec("top,literal,icase", path)),
            )
            .collect();
        if !leaving.is_empty() {
            self.write_pathspecs(&leaving)?;
            self.git_with_pathspecs(
                &["rm", "--cached", "-r", "-f", "-q", "--ignore-unmatch"],
                deadline,
            )?;
        }
        let mut adding = vec![within.clone()];
        adding.extend(
            excluded
                .iter()
                .map(|path| pathspec("top,exclude,literal", path)),
        );
        adding.extend(
            protected
                .iter()
                .map(|path| pathspec("top,exclude,literal,icase", path)),
        );
        self.write_pathspecs(&adding)?;
        let mut add = self.command();
        add.args(["add", "-A", "--ignore-errors"]);
        add.arg(self.pathspec_file_arg()).arg("--pathspec-file-nul");
        // Exit code 1 means some files could not be read; everything else was added.
        let unreadable = match output_within(&mut add, remaining(deadline)?)? {
            None => return Err(CheckpointError::TooSlow),
            Some(out) if out.status.success() => false,
            Some(out) if out.status.code() == Some(1) => true,
            Some(out) => return Err(failure("add", &out.stderr)),
        };
        // What exists but the snapshot leaves out, so that a restore to it never removes that:
        // large files, the directories always left out, what git ignores, and what it could not
        // read.
        record
            .left_out
            .extend(large.iter().map(|path| path.to_vec()));
        record
            .left_out
            .extend(excluded_dirs.iter().map(|dir| [dir, &b"/"[..]].concat()));
        record.left_out.extend(self.list(
            &[
                "--others",
                "--ignored",
                "--exclude-standard",
                "--directory",
                "--",
            ],
            &within,
            deadline,
        )?);
        if unreadable {
            let added: HashSet<Vec<u8>> = self
                .list(&["--cached", "--"], &within, deadline)?
                .into_iter()
                .collect();
            for path in &listed {
                // A nested repository is listed as a directory, with a slash.
                let name = path.strip_suffix(b"/").unwrap_or(path);
                let gone = std::fs::symlink_metadata(self.root.join(OsStr::from_bytes(name)))
                    .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound);
                if !added.contains(name)
                    && !gone
                    && !large.contains(name)
                    && excluded_dir(name).is_none()
                    && !self.is_protected(name)
                {
                    record.left_out.insert(path.clone());
                }
            }
        }
        let tree = self.git(&["write-tree"], remaining(deadline)?)?;
        let record = record.encode();
        let last = self.last.lock().expect("last snapshot lock").clone();
        // The snapshot's first parent holds its record; the snapshot before it is reachable from
        // there, or, when the record is the same as last time and its commit is reused, is the
        // second parent.
        let parents = match last {
            Some(last) if last.record == record => vec![last.record_commit, last.commit],
            Some(last) => vec![self.write_record(&record, Some(&last.commit), deadline)?],
            None => {
                let previous = self
                    .git(
                        &["rev-parse", "-q", "--verify", &self.reference],
                        remaining(deadline)?,
                    )
                    .ok();
                vec![self.write_record(&record, previous.as_deref(), deadline)?]
            }
        };
        let mut args = vec!["commit-tree", "--no-gpg-sign", &tree];
        for parent in &parents {
            args.extend(["-p", parent]);
        }
        args.extend(["-m", message]);
        let commit = self.git(&args, remaining(deadline)?)?;
        self.git(
            &["update-ref", &self.reference, &commit],
            remaining(deadline)?,
        )?;
        *self.last.lock().expect("last snapshot lock") = Some(Last {
            commit: commit.clone(),
            record,
            record_commit: parents[0].clone(),
        });
        let _ = std::fs::copy(&self.index, self.gitdir.join("index"));
        Ok(commit)
    }

    /// Stores the encoded `record` as the file `record` of a commit of its own, whose parent is
    /// `previous` (the session's last snapshot), and returns that commit. The snapshot's commit has
    /// it as its first parent, so the session's ref keeps every snapshot, and each one's record,
    /// reachable.
    fn write_record(
        &self,
        record: &[u8],
        previous: Option<&str>,
        deadline: Instant,
    ) -> Result<String, CheckpointError> {
        std::fs::write(&self.record, record)?;
        let blob = self.hash_object("blob", &self.record, deadline)?;
        let blob = hex::decode(&blob).map_err(|_| failure("hash-object", blob.as_bytes()))?;
        let mut tree = b"100644 record\0".to_vec();
        tree.extend_from_slice(&blob);
        let mut tree_file = self.record.clone().into_os_string();
        tree_file.push(".tree");
        std::fs::write(&tree_file, tree)?;
        let tree = self.hash_object("tree", Path::new(&tree_file), deadline)?;
        let mut args = vec![
            "commit-tree",
            "--no-gpg-sign",
            &tree,
            "-m",
            "what the next snapshot left out",
        ];
        if let Some(previous) = previous {
            args.extend(["-p", previous]);
        }
        self.git(&args, remaining(deadline)?)
    }

    /// Stores the file at `path` as an object of `kind`, byte for byte, and returns its id.
    fn hash_object(
        &self,
        kind: &str,
        path: &Path,
        deadline: Instant,
    ) -> Result<String, CheckpointError> {
        let mut cmd = self.command();
        cmd.args(["hash-object", "-w", "--no-filters", "-t", kind, "--"])
            .arg(path);
        self.run(cmd, "hash-object", deadline)
    }

    /// The record of the snapshot `commit`, in its first parent.
    fn record_of(&self, commit: &str) -> Result<Record, CheckpointError> {
        let blob = format!("{commit}^1:record");
        let deadline = Instant::now() + self.restore_timeout;
        match self.git_bytes(&["cat-file", "blob", &blob], deadline) {
            Ok(bytes) => Record::decode(&bytes),
            Err(CheckpointError::TooSlow) => return Err(CheckpointError::TooSlow),
            Err(_) => None,
        }
        .ok_or_else(|| CheckpointError::NoRecord(commit.to_string()))
    }

    /// Restores the workspace to `commit`: modified files are reverted, deleted files recreated,
    /// and files created since removed. What snapshots leave out is left alone: a file the
    /// snapshot taken just before leaves out is never overwritten, and one `commit` left out
    /// (because it was too large, ignored or unreadable then) is never removed. Returns a snapshot
    /// of the workspace as it was just before, which restores it again.
    pub fn restore(&self, commit: &str) -> Result<String, CheckpointError> {
        check_commit(commit)?;
        self.unlock_after(self.restore_unchecked(commit))
    }

    fn restore_unchecked(&self, commit: &str) -> Result<String, CheckpointError> {
        let target = self.record_of(commit)?;
        // Its paths are relative to the root, and cover only the workspace it was taken for.
        match &target.workspace {
            Some(taken) if *taken == self.workspace => {}
            taken => {
                return Err(CheckpointError::OtherWorkspace {
                    taken: taken.clone().unwrap_or_default(),
                });
            }
        }
        let before = self.snapshot_within("before a rewind", self.restore_timeout)?;
        // From here on, files may be half restored: the error names the snapshot that holds them
        // as they were.
        self.restore_to(commit, &target, &before)
            .map_err(|source| CheckpointError::Restore {
                before: before.clone(),
                source: Box::new(source),
            })?;
        Ok(before)
    }

    /// Makes the workspace match the snapshot `commit`, whose record is `target`, from the
    /// snapshot `before` just taken.
    fn restore_to(
        &self,
        commit: &str,
        target: &Record,
        before: &str,
    ) -> Result<(), CheckpointError> {
        let deadline = Instant::now() + self.restore_timeout;
        let now = self.tree_entries(before, deadline)?;
        let then = self.tree_entries(commit, deadline)?;
        let now_paths: HashSet<&[u8]> = now.iter().map(|e| e.path.as_slice()).collect();
        let then_paths: HashSet<&[u8]> = then.iter().map(|e| e.path.as_slice()).collect();
        let current = Current {
            paths: &now_paths,
            record: &self.record_of(before)?,
            gitlinks: now
                .iter()
                .filter(|e| e.mode == GITLINK)
                .map(|e| e.path.as_slice())
                .collect(),
        };
        // Paths `commit` holds that `before` does not are written only where that removes
        // nothing that no snapshot holds.
        let skipped: HashSet<&[u8]> = then
            .iter()
            .map(|e| e.path.as_slice())
            .filter(|p| !now_paths.contains(p) && self.blocked(p, &current))
            .collect();
        // Paths `before` holds that `commit` does not would be removed as created since. Those
        // `commit` left out existed then, though: they stay as they are.
        let kept: Vec<&TreeEntry> = now
            .iter()
            .filter(|e| !then_paths.contains(e.path.as_slice()) && target.covers(&e.path))
            .collect();
        let tree = if skipped.is_empty() && kept.is_empty() {
            format!("{commit}^{{tree}}")
        } else {
            let skipped: Vec<&[u8]> = skipped.iter().copied().collect();
            self.build_tree(commit, &skipped, &kept, deadline)?
        };
        self.git(&["read-tree", "--reset", "-u", &tree], remaining(deadline)?)?;
        for (path, mode) in &target.modes {
            if then_paths.contains(path.as_slice()) && !skipped.contains(path.as_slice()) {
                self.set_mode(path, *mode);
            }
        }
        // git only warns when it cannot remove a file. (A nested repository's directory is never
        // removed, and a directory may now stand where a removed file was.)
        let kept: HashSet<&[u8]> = kept.iter().map(|e| e.path.as_slice()).collect();
        let left: Vec<&[u8]> = now
            .iter()
            .filter(|e| e.mode != GITLINK)
            .map(|e| e.path.as_slice())
            .filter(|p| !then_paths.contains(p) && !kept.contains(p))
            .filter(|p| {
                std::fs::symlink_metadata(self.root.join(OsStr::from_bytes(p)))
                    .is_ok_and(|m| !m.is_dir())
            })
            .collect();
        if let Some(first) = left.first() {
            return Err(CheckpointError::Git {
                command: "read-tree".into(),
                message: format!(
                    "could not remove {} file(s) created since the snapshot, such as {}",
                    left.len(),
                    String::from_utf8_lossy(first)
                ),
            });
        }
        Ok(())
    }

    /// Gives the regular file at `path` the permissions `mode`; never follows a symlink there.
    fn set_mode(&self, path: &[u8], mode: u32) {
        let Ok(file) = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY)
            .open(self.root.join(OsStr::from_bytes(path)))
        else {
            return;
        };
        if file
            .metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o7777 != mode)
        {
            let _ = file.set_permissions(std::fs::Permissions::from_mode(mode));
        }
    }

    /// Whether writing the target's file at `path`, which `before` does not hold, would remove
    /// something no snapshot holds. Each component is looked at without following symlinks. A
    /// file, symlink or anything else but a directory on the way, or at `path`, would be removed:
    /// that is fine only when `before` holds it (a symlink the agent made, say), not when
    /// snapshots leave it out (large, ignored or unreadable). A directory at `path` would be
    /// removed with everything in it: that is fine only when `before` holds all of it.
    fn blocked(&self, path: &[u8], current: &Current) -> bool {
        let mut at = self.root.clone();
        let mut start = 0;
        loop {
            let end = path[start..]
                .iter()
                .position(|b| *b == b'/')
                .map_or(path.len(), |i| start + i);
            at.push(OsStr::from_bytes(&path[start..end]));
            let here = &path[..end];
            let meta = match std::fs::symlink_metadata(&at) {
                Ok(meta) => meta,
                // Nothing there: git makes what the target has.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return false,
                // Whatever it is, it cannot be told apart: leave it.
                Err(_) => return true,
            };
            if !meta.is_dir() {
                return !current.paths.contains(here);
            }
            if end == path.len() {
                return current.holds_what_snapshots_leave_out(here);
            }
            start = end + 1;
        }
    }

    /// The tree of `commit` without the paths `skipped` and with the entries `kept`, built in the
    /// restore index.
    fn build_tree(
        &self,
        commit: &str,
        skipped: &[&[u8]],
        kept: &[&TreeEntry],
        deadline: Instant,
    ) -> Result<String, CheckpointError> {
        let in_scratch = |args: Vec<OsString>| -> Result<String, CheckpointError> {
            let mut cmd = self.command();
            cmd.env("GIT_INDEX_FILE", &self.scratch).args(&args);
            self.run(cmd, &args[0].to_string_lossy(), deadline)
        };
        in_scratch(vec!["read-tree".into(), commit.into()])?;
        let removals = skipped.iter().map(|p| OsStr::from_bytes(p).to_os_string());
        for chunk in arg_chunks(removals) {
            let mut args = vec!["update-index".into(), "--force-remove".into(), "--".into()];
            args.extend(chunk);
            in_scratch(args)?;
        }
        let additions = kept.iter().map(|e| {
            let mut info = OsString::from(format!("{},{},", e.mode, e.oid));
            info.push(OsStr::from_bytes(&e.path));
            info
        });
        for chunk in arg_chunks(additions) {
            let mut args = vec![OsString::from("update-index"), "--add".into()];
            for info in chunk {
                args.push("--cacheinfo".into());
                args.push(info);
            }
            in_scratch(args)?;
        }
        let tree = in_scratch(vec!["write-tree".into()])?;
        let _ = std::fs::remove_file(&self.scratch);
        Ok(tree)
    }

    /// The entries of the snapshot `commit`, with paths relative to the root.
    fn tree_entries(
        &self,
        commit: &str,
        deadline: Instant,
    ) -> Result<Vec<TreeEntry>, CheckpointError> {
        let listed = self.git_bytes(&["ls-tree", "-r", "-z", "--full-tree", commit], deadline)?;
        split_nul(&listed)
            .into_iter()
            .map(|item| TreeEntry::parse(&item))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| failure("ls-tree", b"unexpected output"))
    }

    /// The files a snapshot holds, relative to the workspace.
    pub fn files(&self, commit: &str) -> Result<Vec<PathBuf>, CheckpointError> {
        check_commit(commit)?;
        Ok(self
            .tree_paths(commit)?
            .iter()
            .filter_map(|p| self.in_workspace(p))
            .map(|p| PathBuf::from(OsStr::from_bytes(p)))
            .collect())
    }

    fn tree_paths(&self, commit: &str) -> Result<Vec<Vec<u8>>, CheckpointError> {
        let listed = self.git_bytes(
            &["ls-tree", "-r", "-z", "--name-only", "--full-tree", commit],
            Instant::now() + self.restore_timeout,
        )?;
        Ok(listed
            .split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            .map(<[u8]>::to_vec)
            .collect())
    }

    /// The pathspec that limits git to the workspace.
    fn within(&self) -> Vec<u8> {
        if self.scope.is_empty() {
            b":(top)".to_vec()
        } else {
            pathspec("top,literal", &self.scope)
        }
    }

    /// The [`PROTECTED`] names at the top of the workspace, relative to the root.
    fn protected(&self) -> Vec<Vec<u8>> {
        PROTECTED
            .iter()
            .map(|name| {
                let mut path = self.scope.clone();
                if !path.is_empty() {
                    path.push(b'/');
                }
                path.extend_from_slice(name);
                path
            })
            .collect()
    }

    /// Whether `path` is a [`PROTECTED`] name at the top of the workspace, or inside one.
    fn is_protected(&self, path: &[u8]) -> bool {
        self.protected().iter().any(|name| {
            path.len() >= name.len()
                && path[..name.len()].eq_ignore_ascii_case(name)
                && matches!(path.get(name.len()), None | Some(b'/'))
        })
    }

    /// The paths `ls-files -z` lists with `options` and then `pathspec`, relative to the root.
    fn list(
        &self,
        options: &[&str],
        pathspec: &[u8],
        deadline: Instant,
    ) -> Result<Vec<Vec<u8>>, CheckpointError> {
        let mut cmd = self.command();
        cmd.args(["ls-files", "-z"])
            .args(options)
            .arg(OsStr::from_bytes(pathspec));
        match output_within(&mut cmd, remaining(deadline)?)? {
            None => Err(CheckpointError::TooSlow),
            Some(out) if out.status.success() => Ok(split_nul(&out.stdout)),
            Some(out) => Err(failure("ls-files", &out.stderr)),
        }
    }

    /// `path`, relative to the root, as a path relative to the workspace; `None` outside it.
    fn in_workspace<'a>(&self, path: &'a [u8]) -> Option<&'a [u8]> {
        if self.scope.is_empty() {
            return Some(path);
        }
        path.strip_prefix(self.scope.as_slice())?.strip_prefix(b"/")
    }

    /// Writes `pathspecs` where [`pathspec_file_arg`](Self::pathspec_file_arg) points git.
    fn write_pathspecs(&self, pathspecs: &[Vec<u8>]) -> Result<(), CheckpointError> {
        let mut bytes = Vec::new();
        for pathspec in pathspecs {
            bytes.extend_from_slice(pathspec);
            bytes.push(0);
        }
        std::fs::write(&self.pathspecs, bytes)?;
        Ok(())
    }

    fn pathspec_file_arg(&self) -> OsString {
        let mut arg = OsString::from("--pathspec-from-file=");
        arg.push(&self.pathspecs);
        arg
    }

    /// Runs git with `args` and the pathspecs last written.
    fn git_with_pathspecs(
        &self,
        args: &[&str],
        deadline: Instant,
    ) -> Result<String, CheckpointError> {
        let mut cmd = self.command();
        cmd.args(args)
            .arg(self.pathspec_file_arg())
            .arg("--pathspec-file-nul");
        self.run(cmd, args[0], deadline)
    }

    /// `git` with a clean environment: the shadow repository, the root as its work tree, this
    /// session's index, and no user or system configuration.
    fn command(&self) -> Command {
        let mut cmd = Command::new(&self.git);
        for setting in OVERRIDES {
            cmd.args(["-c", setting]);
        }
        if let Some(excludes) = &self.repository_excludes {
            let mut setting = OsString::from("core.excludesFile=");
            setting.push(excludes);
            cmd.arg("-c").arg(setting);
        }
        cmd.env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.gitdir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_DIR", &self.gitdir)
            .env("GIT_WORK_TREE", &self.root)
            .env("GIT_INDEX_FILE", &self.index)
            .env("GIT_AUTHOR_NAME", "harness")
            .env("GIT_AUTHOR_EMAIL", "harness@localhost")
            .env("GIT_COMMITTER_NAME", "harness")
            .env("GIT_COMMITTER_EMAIL", "harness@localhost")
            .env("GIT_TERMINAL_PROMPT", "0")
            .current_dir(&self.root);
        cmd
    }

    fn git(&self, args: &[&str], timeout: Duration) -> Result<String, CheckpointError> {
        let mut cmd = self.command();
        cmd.args(args);
        self.run(cmd, args[0], Instant::now() + timeout)
    }

    fn git_bytes(&self, args: &[&str], deadline: Instant) -> Result<Vec<u8>, CheckpointError> {
        let mut cmd = self.command();
        cmd.args(args);
        match output_within(&mut cmd, remaining(deadline)?)? {
            None => Err(CheckpointError::TooSlow),
            Some(out) if out.status.success() => Ok(out.stdout),
            Some(out) => Err(failure(args[0], &out.stderr)),
        }
    }

    /// Runs `cmd` and returns its trimmed output.
    fn run(
        &self,
        mut cmd: Command,
        name: &str,
        deadline: Instant,
    ) -> Result<String, CheckpointError> {
        match output_within(&mut cmd, remaining(deadline)?)? {
            None => Err(CheckpointError::TooSlow),
            Some(out) if out.status.success() => {
                Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
            }
            Some(out) => Err(failure(name, &out.stderr)),
        }
    }
}

/// The time left before `deadline`, or `TooSlow` when it has passed.
fn remaining(deadline: Instant) -> Result<Duration, CheckpointError> {
    let now = Instant::now();
    if now >= deadline {
        return Err(CheckpointError::TooSlow);
    }
    Ok(deadline - now)
}

/// Fails unless `commit` is a full commit id as git prints it, so a session file can name only a
/// snapshot, never a ref, a revision expression or an option.
fn check_commit(commit: &str) -> Result<(), CheckpointError> {
    let hex = commit
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if hex && matches!(commit.len(), 40 | 64) {
        Ok(())
    } else {
        Err(CheckpointError::InvalidCommit(commit.to_string()))
    }
}

fn failure(command: &str, stderr: &[u8]) -> CheckpointError {
    CheckpointError::Git {
        command: command.to_string(),
        message: String::from_utf8_lossy(stderr).trim().to_string(),
    }
}

/// Fails when the shadow repository `gitdir` is inside one of `roots`, directories that commands
/// can write to: a command could then change the repository's configuration, which checkpoint git
/// reads outside any sandbox, and the snapshots themselves.
pub fn check_location(gitdir: &Path, roots: &[PathBuf]) -> Result<(), CheckpointError> {
    let gitdir = resolve(gitdir);
    for root in roots {
        if gitdir.starts_with(resolve(root)) {
            return Err(CheckpointError::Exposed {
                gitdir,
                root: root.clone(),
            });
        }
    }
    Ok(())
}

/// `path` made absolute, with symlinks resolved as far as it exists, and `.` and `..` resolved in
/// the rest, which does not exist yet and so holds no symlink.
fn resolve(path: &Path) -> PathBuf {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut existing = absolute.as_path();
    let mut rest = Vec::new();
    let mut resolved = loop {
        if let Ok(canonical) = existing.canonicalize() {
            break canonical;
        }
        match (existing.parent(), existing.components().next_back()) {
            (Some(parent), Some(last)) => {
                rest.push(last.as_os_str().to_os_string());
                existing = parent;
            }
            _ => return absolute,
        }
    };
    for part in rest.iter().rev() {
        match part.to_str() {
            Some(".") => {}
            Some("..") => {
                resolved.pop();
            }
            _ => resolved.push(part),
        }
    }
    resolved
}

/// The work tree snapshots of `workspace` are taken in: the repository it is in, found as
/// `harness_context::project::repo_root` finds it (the nearest directory at or above it holding a
/// `.git`), so that sessions and snapshots belong to the same project; or the workspace itself.
fn work_tree(workspace: &Path) -> PathBuf {
    workspace
        .ancestors()
        .find(|dir| dir.join(".git").exists())
        .unwrap_or(workspace)
        .to_path_buf()
}

/// The `info/exclude` of the repository at `root`, when it has one: from its `.git` directory, or,
/// for a `.git` file (a linked worktree or a submodule), from the repository that file names.
/// Only that file is read, as more ignore rules; nothing else of the repository's is used.
fn repository_excludes(root: &Path) -> Option<PathBuf> {
    let dotgit = root.join(".git");
    let gitdir = if dotgit.is_dir() {
        dotgit
    } else {
        let text = read_small_file(&dotgit)?;
        let target = text.lines().next()?.strip_prefix("gitdir:")?.trim();
        root.join(target)
    };
    // A linked worktree's gitdir names the repository's common directory, which holds `info`.
    let common = match read_small_file(&gitdir.join("commondir")) {
        Some(text) => gitdir.join(text.trim()),
        None => gitdir,
    };
    let excludes = common.join("info").join("exclude");
    excludes.is_file().then_some(excludes)
}

/// The text of the regular file at `path`, when it is one of at most 4 KiB; never waits on a FIFO.
fn read_small_file(path: &Path) -> Option<String> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path)
        .ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() || meta.len() > 4096 {
        return None;
    }
    let mut text = String::new();
    file.read_to_string(&mut text).ok()?;
    Some(text)
}

/// What a snapshot knows besides its files: the workspace it was taken for and, with paths
/// relative to the root, the paths that existed but were left out (large, ignored or unreadable
/// files, and whole directories, which end with `/`), and the permissions of its private files.
#[derive(Debug, Default)]
struct Record {
    workspace: Option<PathBuf>,
    left_out: BTreeSet<Vec<u8>>,
    modes: BTreeMap<Vec<u8>, u32>,
}

impl Record {
    /// NUL-terminated items: [`RECORD_MAGIC`], `workspace <path>`, then `left-out <path>` for
    /// each path left out, and `mode <octal> <path>` for each private file.
    fn encode(&self) -> Vec<u8> {
        let mut out = RECORD_MAGIC.to_vec();
        out.push(0);
        if let Some(workspace) = &self.workspace {
            out.extend_from_slice(b"workspace ");
            out.extend_from_slice(workspace.as_os_str().as_bytes());
            out.push(0);
        }
        for path in &self.left_out {
            out.extend_from_slice(b"left-out ");
            out.extend_from_slice(path);
            out.push(0);
        }
        for (path, mode) in &self.modes {
            out.extend_from_slice(format!("mode {mode:o} ").as_bytes());
            out.extend_from_slice(path);
            out.push(0);
        }
        out
    }

    fn decode(bytes: &[u8]) -> Option<Record> {
        let mut items = bytes.split(|b| *b == 0);
        if items.next()? != RECORD_MAGIC {
            return None;
        }
        let mut record = Record::default();
        for item in items {
            if let Some(path) = item.strip_prefix(b"workspace ") {
                record.workspace = Some(PathBuf::from(OsStr::from_bytes(path)));
            } else if let Some(path) = item.strip_prefix(b"left-out ") {
                record.left_out.insert(path.to_vec());
            } else if let Some(rest) = item.strip_prefix(b"mode ") {
                let space = rest.iter().position(|b| *b == b' ')?;
                let mode = std::str::from_utf8(&rest[..space]).ok()?;
                let mode = u32::from_str_radix(mode, 8).ok()?;
                record
                    .modes
                    .insert(rest[space + 1..].to_vec(), mode & 0o7777);
            }
        }
        Some(record)
    }

    /// Whether the snapshot left `path` out, by itself or in a directory it left out.
    fn covers(&self, path: &[u8]) -> bool {
        self.left_out.contains(path)
            || path
                .iter()
                .enumerate()
                .any(|(i, b)| *b == b'/' && self.left_out.contains(&path[..=i]))
    }
}

/// A snapshot this process took, and its record, encoded, with the commit that holds it.
#[derive(Debug, Clone)]
struct Last {
    commit: String,
    record: Vec<u8>,
    record_commit: String,
}

/// The workspace as the snapshot just taken before a restore holds it.
struct Current<'a> {
    paths: &'a HashSet<&'a [u8]>,
    record: &'a Record,
    /// Its nested repositories, whose contents no snapshot holds.
    gitlinks: Vec<&'a [u8]>,
}

impl Current<'_> {
    /// Whether the directory `dir` holds something no snapshot holds: what the record says was
    /// left out, or a nested repository.
    fn holds_what_snapshots_leave_out(&self, dir: &[u8]) -> bool {
        let inside = [dir, &b"/"[..]].concat();
        self.record
            .left_out
            .range(inside.clone()..)
            .next()
            .is_some_and(|p| p.starts_with(&inside))
            || self.gitlinks.iter().any(|p| p.starts_with(&inside))
    }
}

/// The mode of a nested repository in a tree.
const GITLINK: &str = "160000";

/// One file of a snapshot, as `ls-tree` lists it.
#[derive(Debug)]
struct TreeEntry {
    mode: String,
    oid: String,
    path: Vec<u8>,
}

impl TreeEntry {
    /// Parses `<mode> <type> <oid>\t<path>`.
    fn parse(item: &[u8]) -> Option<TreeEntry> {
        let tab = item.iter().position(|b| *b == b'\t')?;
        let meta = std::str::from_utf8(&item[..tab]).ok()?;
        let mut fields = meta.split(' ');
        let (mode, _kind, oid) = (fields.next()?, fields.next()?, fields.next()?);
        Some(TreeEntry {
            mode: mode.to_string(),
            oid: oid.to_string(),
            path: item[tab + 1..].to_vec(),
        })
    }
}

/// `args` in groups small enough for one command line each.
fn arg_chunks(args: impl Iterator<Item = OsString>) -> Vec<Vec<OsString>> {
    let (most, most_bytes) = ARGS_PER_COMMAND;
    let mut chunks: Vec<Vec<OsString>> = Vec::new();
    let mut bytes = 0;
    for arg in args {
        let len = arg.len() + 1;
        match chunks.last_mut() {
            Some(chunk) if chunk.len() < most && bytes + len <= most_bytes => {
                bytes += len;
                chunk.push(arg);
            }
            _ => {
                bytes = len;
                chunks.push(vec![arg]);
            }
        }
    }
    chunks
}

/// The NUL-separated items of git's `-z` output.
fn split_nul(bytes: &[u8]) -> Vec<Vec<u8>> {
    bytes
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(<[u8]>::to_vec)
        .collect()
}

/// The pathspec `:(<magic>)<path>`.
fn pathspec(magic: &str, path: &[u8]) -> Vec<u8> {
    let mut out = format!(":({magic})").into_bytes();
    out.extend_from_slice(path);
    out
}

/// The directory always left out (see [`EXCLUDED_DIRS`]) that `path` lies in, if any.
fn excluded_dir(path: &[u8]) -> Option<&[u8]> {
    let mut start = 0;
    while let Some(slash) = path[start..].iter().position(|b| *b == b'/') {
        let end = start + slash;
        if EXCLUDED_DIRS.contains(&&path[start..end]) {
            return Some(&path[..end]);
        }
        start = end + 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_round_trips_and_covers_what_lies_in_its_directories() {
        let mut record = Record {
            workspace: Some(PathBuf::from("/work/a b")),
            ..Record::default()
        };
        for path in [&b".env"[..], b"build/", b"odd\nname", b"web/node_modules/"] {
            record.left_out.insert(path.to_vec());
        }
        record.modes.insert(b"id key".to_vec(), 0o600);
        let decoded = Record::decode(&record.encode()).unwrap();
        assert_eq!(decoded.left_out, record.left_out);
        assert_eq!(decoded.modes, record.modes);
        assert_eq!(decoded.workspace, record.workspace);
        assert!(decoded.covers(b".env"));
        assert!(decoded.covers(b"build/out.o"));
        assert!(decoded.covers(b"build/deep/er.o"));
        assert!(decoded.covers(b"odd\nname"));
        assert!(decoded.covers(b"web/node_modules/p/i.js"));
        assert!(!decoded.covers(b"build"));
        assert!(!decoded.covers(b"buildx/a"));
        assert!(!decoded.covers(b"src/.env"));
        assert!(!decoded.covers(b"web/x.js"));
        assert!(Record::decode(b"something else\0left-out a\0").is_none());
        assert!(Record::decode(b"").is_none());
    }

    #[test]
    fn arguments_are_split_by_count_and_size() {
        let many = (0..1200).map(|i| OsString::from(format!("f{i}")));
        let sizes: Vec<usize> = arg_chunks(many).iter().map(Vec::len).collect();
        assert_eq!(sizes, [500, 500, 200]);
        let long = (0..40).map(|_| OsString::from("x".repeat(4000)));
        let chunks = arg_chunks(long);
        assert!(chunks.len() > 1);
        assert!(
            chunks
                .iter()
                .all(|c| c.iter().map(|a| a.len() + 1).sum::<usize>() <= ARGS_PER_COMMAND.1)
        );
        assert_eq!(chunks.iter().map(Vec::len).sum::<usize>(), 40);
    }

    #[test]
    fn paths_in_always_excluded_directories_are_found() {
        assert_eq!(
            excluded_dir(b"node_modules/m.js"),
            Some(&b"node_modules"[..])
        );
        assert_eq!(
            excluded_dir(b"web/node_modules/p/i.js"),
            Some(&b"web/node_modules"[..])
        );
        assert_eq!(excluded_dir(b"a/target/debug/t"), Some(&b"a/target"[..]));
        assert_eq!(excluded_dir(b"src/target"), None);
        assert_eq!(excluded_dir(b"my-node_modules/x"), None);
        assert_eq!(excluded_dir(b"x"), None);
    }
}
