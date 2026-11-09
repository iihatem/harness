//! Git worktrees for agents that edit: made, locked, committed, reviewed and removed by harness.
//!
//! Every git command here is run by harness itself, outside any sandbox, with the repository's
//! own code switched off ([`git`]). A worktree lives in `<home>/<name>` (the CLI's `home` is
//! `<data>/worktrees/<project>`) on the branch `harness/<name>`, made from the local HEAD,
//! locked while its agent runs, and marked in its git admin directory so that cleanup removes
//! nothing harness did not make.

pub mod git;
pub mod include;
pub mod record;
pub mod review;
pub mod testing;

#[cfg(test)]
mod review_tests;
#[cfg(test)]
mod tests;

use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

pub use git::{Git, GitError};
pub use include::Included;
pub use record::Record;
pub use review::{Applied, Diff, Outcome, Overlap, author_of};

/// The file in a worktree's git admin directory that says harness made it.
const MARKER: &str = "harness-managed";
/// The first words of a lock's reason when harness holds it: `harness pid <pid>`.
const LOCK_PREFIX: &str = "harness pid ";
/// Longest worktree name, in characters.
const NAME_CAP: usize = 40;
/// Port offsets are `100 * slot`, for slots 1 to this.
const PORT_SLOTS: u16 = 99;

/// Why a worktree operation did not happen.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WorktreeError {
    #[error("{0} is not in a git repository, so agents cannot be isolated in worktrees")]
    NotARepository(String),
    #[error("the repository has no commit yet, so there is nothing to make a worktree from")]
    NoCommits,
    #[error(transparent)]
    Git(#[from] GitError),
    #[error("{0}")]
    Io(String),
    /// Held by a running agent (or by someone who is not harness).
    #[error("worktree {name} is locked ({reason}), so it was left alone")]
    Locked { name: String, reason: String },
    /// Applying the branch to the main checkout conflicts; nothing was changed.
    #[error(
        "applying {branch} conflicts in {}; nothing was changed, and the branch and its worktree are kept",
        files.join(", ")
    )]
    Conflict { branch: String, files: Vec<String> },
    /// The main checkout is in a state where the result cannot be applied now.
    #[error("{0}")]
    Refused(String),
    /// Not something harness made, as far as its own records and markers say.
    #[error("{name} is not a worktree harness made ({why}), so it was left alone")]
    NotOurs { name: String, why: String },
}

fn io(context: &str, e: std::io::Error) -> WorktreeError {
    WorktreeError::Io(format!("{context}: {e}"))
}

/// A worktree that harness made. Only [`Worktrees::create`] makes one.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Worktree {
    pub name: String,
    pub branch: String,
    /// The worktree's root.
    pub path: PathBuf,
    /// The commit it was made from.
    pub base: String,
    pub port_offset: u16,
    /// The id of the agent it was made for.
    pub agent: String,
    /// Where the agent works in it: the root, plus the directory the session started in below
    /// the repository's root.
    prefix: PathBuf,
}

impl Worktree {
    /// The directory the agent works in.
    pub fn workspace(&self) -> PathBuf {
        self.path.join(&self.prefix)
    }

    /// What commands the agent runs in it are told: who it is, and a port offset to add to the
    /// ports its servers listen on, so two worktrees' servers do not clash.
    pub fn env(&self) -> Vec<(String, String)> {
        vec![
            ("HARNESS_AGENT_ID".to_string(), self.agent.clone()),
            (
                "HARNESS_PORT_OFFSET".to_string(),
                self.port_offset.to_string(),
            ),
        ]
    }
}

/// What a worktree is made for.
#[derive(Debug, Clone, Copy)]
pub struct Request<'a> {
    /// The agent's id (`a1`).
    pub agent: &'a str,
    pub label: &'a str,
    pub task: &'a str,
}

/// A worktree just made.
#[derive(Debug, Clone)]
pub struct Created {
    pub worktree: Worktree,
    /// Files with changes not committed in the main checkout, which the worktree does not have.
    pub uncommitted: usize,
    /// This is the first worktree that was made while there were such changes: the caller says
    /// so, once.
    pub said_uncommitted: bool,
    /// What `.worktreeinclude` copied.
    pub included: Included,
}

/// The worktrees of one repository, in one directory.
pub struct Worktrees {
    git: Git,
    /// Where the repository's working tree starts.
    root: PathBuf,
    /// Where the directory harness was started in is, below `root`.
    prefix: PathBuf,
    home: PathBuf,
    said: AtomicBool,
}

/// Whether process `pid` is alive.
pub fn process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    match nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None) {
        Ok(()) => true,
        Err(nix::errno::Errno::EPERM) => true,
        Err(_) => false,
    }
}

/// `text` as a name made of lowercase letters, digits and single dashes.
fn slug(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    out.trim_end_matches('-').to_string()
}

/// The name for an agent's worktree: its label, then the first words of its task.
fn base_name(label: &str, task: &str) -> String {
    let label: String = slug(label).chars().take(16).collect();
    let words: Vec<String> = task
        .split_whitespace()
        .map(slug)
        .filter(|w| !w.is_empty())
        .take(4)
        .collect();
    let mut name = [label.trim_end_matches('-').to_string(), words.join("-")]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    name = name.chars().take(NAME_CAP).collect();
    let name = name.trim_matches('-');
    if name.is_empty() {
        "agent".to_string()
    } else {
        name.to_string()
    }
}

impl Worktrees {
    /// The worktrees of the repository `dir` is in, kept in `home`.
    pub fn new(dir: &Path, home: PathBuf) -> Result<Worktrees, WorktreeError> {
        let not_repo = || WorktreeError::NotARepository(dir.display().to_string());
        let probe = Git::open(dir).map_err(|_| not_repo())?;
        let root = probe
            .run(&["rev-parse", "--show-toplevel"])
            .map_err(|_| not_repo())?;
        let root = PathBuf::from(root.trim());
        let prefix = probe
            .run(&["rev-parse", "--show-prefix"])
            .map_err(|_| not_repo())?;
        // The settings to override are those of the repository's own configuration.
        let git = Git::open(&root)?;
        Ok(Worktrees {
            git,
            root,
            prefix: PathBuf::from(prefix.trim()),
            home,
            said: AtomicBool::new(false),
        })
    }

    /// The directory the worktrees are in.
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// The root of the repository's working tree.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The runner for the main checkout.
    pub fn git(&self) -> &Git {
        &self.git
    }

    fn record_path(&self, name: &str) -> PathBuf {
        self.home.join(format!("{name}.json"))
    }

    /// The records of the worktrees harness made here, oldest first.
    pub fn records(&self) -> Vec<Record> {
        let Ok(entries) = std::fs::read_dir(&self.home) else {
            return Vec::new();
        };
        let mut records: Vec<Record> = entries
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .filter_map(|e| std::fs::read_to_string(e.path()).ok())
            .filter_map(|text| serde_json::from_str(&text).ok())
            .collect();
        records
            .sort_by(|a: &Record, b: &Record| a.created.cmp(&b.created).then(a.name.cmp(&b.name)));
        records
    }

    /// Writes `record`, replacing the one of its name.
    pub fn write_record(&self, record: &Record) -> Result<(), WorktreeError> {
        std::fs::create_dir_all(&self.home).map_err(|e| io("making the worktrees directory", e))?;
        let text =
            serde_json::to_string_pretty(record).map_err(|e| WorktreeError::Io(e.to_string()))?;
        let tmp = self.home.join(format!(".{}.json.tmp", record.name));
        std::fs::write(&tmp, text).map_err(|e| io("writing a worktree record", e))?;
        std::fs::rename(&tmp, self.record_path(&record.name))
            .map_err(|e| io("writing a worktree record", e))
    }

    fn branch_exists(&self, branch: &str) -> bool {
        self.git
            .run(&[
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ])
            .is_ok()
    }

    fn unique_name(&self, request: &Request) -> String {
        let base = base_name(request.label, request.task);
        let taken = |name: &str| {
            self.record_path(name).exists()
                || self.home.join(name).exists()
                || self.branch_exists(&format!("harness/{name}"))
        };
        if !taken(&base) {
            return base;
        }
        (2..)
            .map(|n| format!("{base}-{n}"))
            .find(|name| !taken(name))
            .expect("some suffix is free")
    }

    /// How many files have changes not committed in the main checkout.
    fn uncommitted(&self) -> usize {
        let Ok(status) = self.git.run(&["status", "--porcelain=v1", "-z"]) else {
            return 0;
        };
        let mut count = 0;
        let mut parts = status.split('\0').filter(|p| !p.is_empty());
        while let Some(part) = parts.next() {
            count += 1;
            // A rename or copy is followed by the path it came from.
            if part.starts_with(['R', 'C']) {
                parts.next();
            }
        }
        count
    }

    fn lowest_free_offset(&self) -> u16 {
        let used: Vec<u16> = self.records().iter().map(|r| r.port_offset).collect();
        (1..=PORT_SLOTS)
            .map(|slot| slot * 100)
            .find(|offset| !used.contains(offset))
            .unwrap_or(100)
    }

    /// Makes a worktree for an agent, on a new branch from the local HEAD, locked, marked, and
    /// recorded.
    pub fn create(&self, request: &Request) -> Result<Created, WorktreeError> {
        let base = self
            .git
            .run(&["rev-parse", "--verify", "-q", "HEAD^{commit}"])
            .map_err(|_| WorktreeError::NoCommits)?
            .trim()
            .to_string();
        std::fs::create_dir_all(&self.home).map_err(|e| io("making the worktrees directory", e))?;
        let name = self.unique_name(request);
        let branch = format!("harness/{name}");
        let path = self.home.join(&name);
        let uncommitted = self.uncommitted();
        let reason = format!("{LOCK_PREFIX}{}", std::process::id());
        let path_text = path.to_string_lossy().into_owned();
        self.git.run(&[
            "worktree", "add", "--lock", "--reason", &reason, "-b", &branch, &path_text, &base,
        ])?;
        let finished = (|| {
            let admin = self.admin_dir(&path)?;
            std::fs::write(
                admin.join(MARKER),
                format!(
                    "harness 1\npid {}\npath {}\n",
                    std::process::id(),
                    path.display()
                ),
            )
            .map_err(|e| io("marking the worktree", e))?;
            let included = include::copy_included(&self.git, &self.root, &path);
            let offset = self.lowest_free_offset();
            let mut record = Record::new(
                &name,
                &branch,
                &path,
                &base,
                request.agent,
                request.label,
                request.task,
            );
            record.port_offset = offset;
            self.write_record(&record)?;
            Ok::<_, WorktreeError>((offset, included))
        })();
        let (offset, included) = match finished {
            Ok(done) => done,
            Err(e) => {
                // Do not leave a half-made worktree behind.
                let _ = self.git.run(&["worktree", "unlock", &path_text]);
                let _ = self.git.run(&["worktree", "remove", "--force", &path_text]);
                let _ = self.git.run(&["branch", "-D", &branch]);
                return Err(e);
            }
        };
        let said_uncommitted = uncommitted > 0
            && self
                .said
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok();
        Ok(Created {
            worktree: Worktree {
                name,
                branch,
                path,
                base,
                port_offset: offset,
                agent: request.agent.to_string(),
                prefix: self.prefix.clone(),
            },
            uncommitted,
            said_uncommitted,
            included,
        })
    }

    /// The git admin directory of the worktree at `path`.
    fn admin_dir(&self, path: &Path) -> Result<PathBuf, WorktreeError> {
        let text = self
            .git
            .at(path)
            .run(&["rev-parse", "--absolute-git-dir"])?;
        Ok(PathBuf::from(text.trim()))
    }

    /// Whether the worktree at `path` carries harness's marker.
    pub fn is_marked(&self, path: &Path) -> bool {
        let Ok(dot_git) = std::fs::read_to_string(path.join(".git")) else {
            return false;
        };
        let Some(admin) = dot_git
            .lines()
            .next()
            .and_then(|l| l.strip_prefix("gitdir:"))
        else {
            return false;
        };
        std::fs::read_to_string(Path::new(admin.trim()).join(MARKER))
            .is_ok_and(|text| text.starts_with("harness 1"))
    }

    /// The locks git has on worktrees: (path, reason).
    fn locks(&self) -> Vec<(PathBuf, String)> {
        let Ok(text) = self.git.run(&["worktree", "list", "--porcelain"]) else {
            return Vec::new();
        };
        let mut found = Vec::new();
        let mut current: Option<PathBuf> = None;
        for line in text.lines() {
            if let Some(path) = line.strip_prefix("worktree ") {
                current = Some(PathBuf::from(path));
            } else if (line == "locked" || line.starts_with("locked "))
                && let Some(path) = &current
            {
                found.push((
                    path.clone(),
                    line.strip_prefix("locked ").unwrap_or("").to_string(),
                ));
            }
        }
        found
    }

    fn same_path(a: &Path, b: &Path) -> bool {
        match (a.canonicalize(), b.canonicalize()) {
            (Ok(a), Ok(b)) => a == b,
            _ => a == b,
        }
    }

    /// The reason `wt` is locked, when it is.
    pub fn lock_reason(&self, wt: &Worktree) -> Option<String> {
        self.lock_reason_at(&wt.path)
    }

    fn lock_reason_at(&self, path: &Path) -> Option<String> {
        self.locks()
            .into_iter()
            .find(|(p, _)| Self::same_path(p, path))
            .map(|(_, reason)| reason)
    }

    /// Ends harness's lock on `wt`: its agent has ended.
    pub fn release(&self, wt: &Worktree) -> Result<(), WorktreeError> {
        self.git
            .run(&["worktree", "unlock", &wt.path.to_string_lossy()])
            .map(|_| ())
            .map_err(Into::into)
    }

    /// Releases the locks of harness processes that no longer exist (an agent's worktree whose
    /// harness died while it ran); returns the names. Locks that are not harness's, and locks of
    /// a live harness, stay.
    pub fn release_stale_locks(&self) -> Vec<String> {
        let mut released = Vec::new();
        for (path, reason) in self.locks() {
            let Some(pid) = reason
                .strip_prefix(LOCK_PREFIX)
                .and_then(|p| p.trim().parse::<u32>().ok())
            else {
                continue;
            };
            let Some(name) = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .filter(|_| Self::same_path(path.parent().unwrap_or(&path), &self.home))
            else {
                continue;
            };
            if !process_alive(pid)
                && self
                    .git
                    .run(&["worktree", "unlock", &path.to_string_lossy()])
                    .is_ok()
            {
                released.push(name);
            }
        }
        released
    }

    /// The record of the worktree `name`, when it is one harness made: a name it gives, a
    /// record that names the places harness uses.
    fn own(&self, name: &str) -> Result<Record, WorktreeError> {
        let not_ours = |why: &str| WorktreeError::NotOurs {
            name: name.to_string(),
            why: why.to_string(),
        };
        if name.is_empty() || name != slug(name) {
            return Err(not_ours("not a name harness gives"));
        }
        let text = std::fs::read_to_string(self.record_path(name))
            .map_err(|_| not_ours("no record of it"))?;
        let record: Record =
            serde_json::from_str(&text).map_err(|_| not_ours("its record is unreadable"))?;
        if record.name != name
            || record.path != self.home.join(name)
            || record.branch != format!("harness/{name}")
        {
            return Err(not_ours("its record names another place"));
        }
        Ok(record)
    }

    /// Takes the directory of the worktree of `record` away, if harness made it and nothing
    /// holds it. Its branch and record stay.
    fn remove_directory(&self, record: &Record) -> Result<(), WorktreeError> {
        let path = &record.path;
        if path.exists() {
            if !self.is_marked(path) {
                return Err(WorktreeError::NotOurs {
                    name: record.name.clone(),
                    why: "it has no harness marker".into(),
                });
            }
            if let Some(reason) = self.lock_reason_at(path) {
                return Err(WorktreeError::Locked {
                    name: record.name.clone(),
                    reason,
                });
            }
            self.git
                .run(&["worktree", "remove", "--force", &path.to_string_lossy()])?;
        } else {
            let _ = self.git.run(&["worktree", "prune"]);
        }
        Ok(())
    }

    /// Removes the worktree `name`, its branch and its record, if harness made it and nothing
    /// holds it: a locked worktree (a running agent) and anything the records and the marker do
    /// not agree is harness's are left alone.
    pub fn remove(&self, name: &str) -> Result<(), WorktreeError> {
        let record = self.own(name)?;
        self.remove_directory(&record)?;
        if self.branch_exists(&record.branch) {
            self.git.run(&["branch", "-D", &record.branch])?;
        }
        let _ = std::fs::remove_file(self.record_path(name));
        Ok(())
    }

    /// Throws the result of `name` away: the worktree, the branch and the record.
    pub fn discard(&self, name: &str) -> Result<(), WorktreeError> {
        self.remove(name)
    }

    /// Keeps the branch of `name` for the user and removes the worktree and the record: the
    /// branch is the user's from now on, and harness never removes it.
    pub fn keep(&self, name: &str) -> Result<(), WorktreeError> {
        let record = self.own(name)?;
        self.remove_directory(&record)?;
        let _ = std::fs::remove_file(self.record_path(name));
        Ok(())
    }
}
