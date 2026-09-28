//! Checkpoints: snapshots of the workspace in a shadow git repository in harness's data
//! directory. The shadow repository has its own `GIT_DIR` and index, so the user's repository,
//! index, branches and history are never touched, and directories that are not repositories work
//! too. git runs without the user's global and system configuration, so no configured filter,
//! hook or file-system monitor runs.

use std::{
    collections::{BTreeSet, HashSet},
    ffi::{OsStr, OsString},
    io::Read,
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::OpenOptionsExt,
    },
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
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
const BUILTIN_EXCLUDES: &str = ".git\nnode_modules/\ntarget/\n";
/// Directories left out of snapshots wherever they are.
const EXCLUDED_DIRS: [&[u8]; 2] = [b"node_modules", b"target"];
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
    /// The ref that keeps this session's snapshots reachable.
    reference: String,
    timeout: Duration,
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
            reference: format!("refs/harness/{session_id}"),
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

    /// Replaces the time a restore may take (for tests).
    pub fn with_restore_timeout(mut self, timeout: Duration) -> Self {
        self.restore_timeout = timeout;
        self
    }

    /// Snapshots the workspace and returns the commit. Files over [`MAX_FILE_SIZE`], git-ignored
    /// files, `.git`, `node_modules` and `target` are left out.
    pub fn snapshot(&self, message: &str) -> Result<String, CheckpointError> {
        self.unlock_after(self.snapshot_within(message, self.timeout))
    }

    /// Passes `result` on, first removing the locks git leaves when it is killed at a time limit
    /// (on this session's indexes and ref), so later snapshots and restores still work. Only this
    /// process uses them: the session is locked to it.
    fn unlock_after<T>(&self, result: Result<T, CheckpointError>) -> Result<T, CheckpointError> {
        if matches!(result, Err(CheckpointError::TooSlow)) {
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
        let mut excluded: BTreeSet<&[u8]> = BTreeSet::new();
        for path in &listed {
            if let Some(dir) = excluded_dir(path) {
                excluded.insert(dir);
            } else if std::fs::symlink_metadata(self.root.join(OsStr::from_bytes(path)))
                .is_ok_and(|m| m.is_file() && m.len() > MAX_FILE_SIZE)
            {
                excluded.insert(path);
            }
        }
        // Files an earlier snapshot holds that are now git-ignored or excluded leave the snapshots.
        let ignored = self.list(
            &["--cached", "--ignored", "--exclude-standard", "--"],
            &within,
            deadline,
        )?;
        let leaving: Vec<Vec<u8>> = ignored
            .iter()
            .map(Vec::as_slice)
            .chain(excluded.iter().copied())
            .map(|path| pathspec("top,literal", path))
            .collect();
        if !leaving.is_empty() {
            self.write_pathspecs(&leaving)?;
            self.git_with_pathspecs(
                &["rm", "--cached", "-r", "-f", "-q", "--ignore-unmatch"],
                deadline,
            )?;
        }
        let mut adding = vec![within];
        adding.extend(
            excluded
                .iter()
                .map(|path| pathspec("top,exclude,literal", path)),
        );
        self.write_pathspecs(&adding)?;
        let mut add = self.command();
        add.args(["add", "-A", "--ignore-errors"]);
        add.arg(self.pathspec_file_arg()).arg("--pathspec-file-nul");
        // Exit code 1 means some files could not be read; everything else was added.
        match output_within(&mut add, remaining(deadline)?)? {
            None => return Err(CheckpointError::TooSlow),
            Some(out) if out.status.code().is_some_and(|c| c <= 1) => {}
            Some(out) => return Err(failure("add", &out.stderr)),
        }
        let tree = self.git(&["write-tree"], remaining(deadline)?)?;
        let parent = self
            .git(
                &["rev-parse", "-q", "--verify", &self.reference],
                remaining(deadline)?,
            )
            .ok();
        let mut commit_args = vec!["commit-tree", "--no-gpg-sign", tree.as_str(), "-m", message];
        if let Some(parent) = &parent {
            commit_args.extend(["-p", parent.as_str()]);
        }
        let commit = self.git(&commit_args, remaining(deadline)?)?;
        self.git(
            &["update-ref", &self.reference, &commit],
            remaining(deadline)?,
        )?;
        let _ = std::fs::copy(&self.index, self.gitdir.join("index"));
        Ok(commit)
    }

    /// Restores the workspace to `commit`: modified files are reverted, deleted files recreated,
    /// and files created since removed. Files that snapshots leave out (large or git-ignored) are
    /// left alone. Returns a snapshot of the workspace as it was just before, which restores it
    /// again.
    pub fn restore(&self, commit: &str) -> Result<String, CheckpointError> {
        check_commit(commit)?;
        self.unlock_after(self.restore_unchecked(commit))
    }

    fn restore_unchecked(&self, commit: &str) -> Result<String, CheckpointError> {
        let before = self.snapshot_within("before a rewind", self.restore_timeout)?;
        // Paths `commit` has that exist now but are not in `before` are files snapshots leave
        // out; restoring them would overwrite something no snapshot holds.
        let now: HashSet<Vec<u8>> = self.tree_paths(&before)?.into_iter().collect();
        let keep: Vec<OsString> = self
            .tree_paths(commit)?
            .into_iter()
            .filter(|p| !now.contains(p))
            .map(OsString::from_vec)
            .filter(|p| std::fs::symlink_metadata(self.root.join(p)).is_ok())
            .collect();
        let tree = if keep.is_empty() {
            format!("{commit}^{{tree}}")
        } else {
            let scratch = &self.scratch;
            let with_index = |args: &[OsString]| -> Result<String, CheckpointError> {
                let mut cmd = self.command();
                cmd.env("GIT_INDEX_FILE", scratch).args(args);
                self.run(cmd, "update-index", Instant::now() + self.restore_timeout)
            };
            with_index(&["read-tree".into(), commit.into()])?;
            for chunk in keep.chunks(500) {
                let mut args = vec![
                    OsString::from("update-index"),
                    "--force-remove".into(),
                    "--".into(),
                ];
                args.extend(chunk.iter().cloned());
                with_index(&args)?;
            }
            let tree = with_index(&["write-tree".into()])?;
            let _ = std::fs::remove_file(scratch);
            tree
        };
        self.git(&["read-tree", "--reset", "-u", &tree], self.restore_timeout)?;
        Ok(before)
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
