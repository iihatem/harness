//! What becomes of an agent's work in a worktree: harness commits it on the agent's branch, the
//! user looks at the diff, and then applies it to the main checkout, keeps the branch, or
//! throws it away. Applying never forces anything: where it cannot be done cleanly it changes
//! nothing.

use serde::{Deserialize, Serialize};

use super::{Record, Worktree, WorktreeError, Worktrees};

/// The most files an outcome lists.
const FILES_CAP: usize = 500;
/// The longest a commit subject is, in characters.
const SUBJECT_CAP: usize = 72;
/// The longest the task is, in a commit's body.
const BODY_CAP: usize = 600;

/// What an agent left in its worktree, committed on its branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outcome {
    pub branch: String,
    pub commit: String,
    pub files: Vec<String>,
    pub insertions: u32,
    pub deletions: u32,
}

/// A branch's changes, for looking at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diff {
    pub stat: String,
    pub patch: String,
    /// The patch was cut at the cap.
    pub cut: bool,
}

/// Another agent's result (or work in progress) that changed some of the same files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overlap {
    pub other: String,
    pub files: Vec<String>,
}

/// A result applied to the main checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    /// The commit now at the tip of the branch.
    pub commit: String,
    /// The branch it was applied to.
    pub branch: String,
    pub files: Vec<String>,
}

/// The name and email commits are made as: the user's, or harness's when git has none.
pub fn author_of(identity: Option<(String, String)>) -> (String, String) {
    identity.unwrap_or_else(|| ("harness".to_string(), "harness@localhost".to_string()))
}

fn one_line(text: &str, cap: usize) -> String {
    let first = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    first
        .chars()
        .filter(|c| !c.is_control())
        .take(cap)
        .collect::<String>()
}

impl Worktrees {
    fn paths(&self, args: &[&str]) -> Result<Vec<String>, WorktreeError> {
        Ok(self
            .git
            .run(args)?
            .split('\0')
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect())
    }

    /// Commits what the agent of `wt` changed on its branch, as the user, without running the
    /// repository's hooks; `None` when it changed nothing (the worktree is left as it is).
    pub fn commit_result(
        &self,
        wt: &Worktree,
        label: &str,
        task: &str,
    ) -> Result<Option<Outcome>, WorktreeError> {
        let git = self.git.at(&wt.path);
        git.run(&["add", "-A"])?;
        match git.run(&["diff", "--cached", "--quiet"]) {
            Ok(_) => return Ok(None),
            Err(e) if e.code == Some(1) => {}
            Err(e) => return Err(e.into()),
        }
        let (name, email) = author_of(self.git.identity());
        let subject = format!(
            "harness: {} \u{2014} {}",
            one_line(label, 24),
            one_line(task, SUBJECT_CAP)
        );
        let body: String = task
            .chars()
            .filter(|c| !c.is_control() || *c == '\n')
            .take(BODY_CAP)
            .collect();
        let body = format!(
            "Agent: {} ({})\nBranch: {}\n\nTask:\n{}\n",
            one_line(label, 40),
            wt.agent,
            wt.branch,
            body.trim()
        );
        git.run_with_env(
            &[
                "commit",
                "--no-verify",
                "--no-gpg-sign",
                "-q",
                "-m",
                &subject,
                "-m",
                &body,
            ],
            &[
                ("GIT_AUTHOR_NAME", &name),
                ("GIT_AUTHOR_EMAIL", &email),
                ("GIT_COMMITTER_NAME", &name),
                ("GIT_COMMITTER_EMAIL", &email),
            ],
        )?;
        let commit = git.run(&["rev-parse", "HEAD"])?.trim().to_string();
        let numstat = git.run(&[
            "diff",
            "--numstat",
            "--no-renames",
            "--no-ext-diff",
            "--no-textconv",
            "-z",
            &wt.base,
            &commit,
        ])?;
        let (mut files, mut insertions, mut deletions) = (Vec::new(), 0u32, 0u32);
        for entry in numstat.split('\0').filter(|e| !e.is_empty()) {
            let mut parts = entry.splitn(3, '\t');
            let (Some(added), Some(removed), Some(path)) =
                (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            insertions += added.parse::<u32>().unwrap_or(0);
            deletions += removed.parse::<u32>().unwrap_or(0);
            if files.len() < FILES_CAP {
                files.push(path.to_string());
            }
        }
        Ok(Some(Outcome {
            branch: wt.branch.clone(),
            commit,
            files,
            insertions,
            deletions,
        }))
    }

    /// The changes of the branch of `name` since its base, at most `cap` bytes of patch.
    pub fn diff(&self, name: &str, cap: usize) -> Result<Diff, WorktreeError> {
        let record = self.own(name)?;
        let range = format!("{}..{}", record.base, record.branch);
        let common = ["--no-ext-diff", "--no-textconv", "--no-color"];
        let stat = self
            .git
            .run_capped(
                &[&["diff", "--stat"][..], &common, &[&range, "--"]].concat(),
                20_000,
            )?
            .0;
        let (patch, cut) = self
            .git
            .run_capped(&[&["diff"][..], &common, &[&range, "--"]].concat(), cap)?;
        Ok(Diff { stat, patch, cut })
    }

    /// The files changed by the branch of `record` (committed), and by its worktree meanwhile
    /// (not yet committed).
    fn changed_files(&self, record: &Record) -> Vec<String> {
        let mut files = Vec::new();
        let range = format!("{}..{}", record.base, record.branch);
        if let Ok(committed) =
            self.paths(&["diff", "--name-only", "--no-renames", "-z", &range, "--"])
        {
            files = committed;
        }
        if record.path.exists()
            && let Ok(status) = self
                .git
                .at(&record.path)
                .run(&["status", "--porcelain=v1", "-z"])
        {
            let mut parts = status.split('\0').filter(|p| !p.is_empty());
            while let Some(part) = parts.next() {
                if part.len() > 3 {
                    files.push(part[3..].to_string());
                }
                if part.starts_with(['R', 'C']) {
                    files.extend(parts.next().map(str::to_string));
                }
            }
        }
        files.sort();
        files.dedup();
        files
    }

    /// The other agents' work that changed files the branch of `name` changed too.
    pub fn overlaps(&self, name: &str) -> Vec<Overlap> {
        let records = self.records();
        let Some(mine) = records.iter().find(|r| r.name == name) else {
            return Vec::new();
        };
        let mine = self.changed_files(mine);
        records
            .iter()
            .filter(|r| r.name != name)
            .filter_map(|other| {
                let theirs = self.changed_files(other);
                let shared: Vec<String> = mine
                    .iter()
                    .filter(|f| theirs.contains(f))
                    .cloned()
                    .collect();
                (!shared.is_empty()).then(|| Overlap {
                    other: other.name.clone(),
                    files: shared,
                })
            })
            .collect()
    }

    /// Whether the main checkout is in the middle of something an apply would tangle with.
    fn unfinished_operation(&self) -> Option<&'static str> {
        const THINGS: [(&str, &str); 5] = [
            ("MERGE_HEAD", "merge"),
            ("CHERRY_PICK_HEAD", "cherry-pick"),
            ("REVERT_HEAD", "revert"),
            ("rebase-merge", "rebase"),
            ("rebase-apply", "rebase"),
        ];
        THINGS.into_iter().find_map(|(file, what)| {
            let path = self.git.run(&["rev-parse", "--git-path", file]).ok()?;
            let path = std::path::Path::new(path.trim());
            let path = if path.is_absolute() {
                path.to_path_buf()
            } else {
                self.root.join(path)
            };
            path.exists().then_some(what)
        })
    }

    /// Brings the commit of `name` into the current branch of the main checkout (a cherry-pick),
    /// and then removes its worktree and branch. When it cannot be done cleanly (a conflict,
    /// uncommitted work in the way, a merge or rebase under way, no branch checked out) nothing
    /// is changed and the branch is kept.
    pub fn apply(&self, name: &str) -> Result<Applied, WorktreeError> {
        let record = self.own(name)?;
        if record.path.exists()
            && let Some(reason) = self.lock_reason_at(&record.path)
        {
            return Err(WorktreeError::Locked {
                name: name.to_string(),
                reason,
            });
        }
        let tip = self
            .git
            .run(&[
                "rev-parse",
                "--verify",
                "-q",
                &format!("refs/heads/{}^{{commit}}", record.branch),
            ])
            .map_err(|_| WorktreeError::Refused(format!("the branch {} is gone", record.branch)))?
            .trim()
            .to_string();
        if tip == record.base {
            return Err(WorktreeError::Refused(format!(
                "{} has no changes to apply",
                record.branch
            )));
        }
        let current = self
            .git
            .run(&["symbolic-ref", "--quiet", "--short", "HEAD"])
            .map_err(|_| {
                WorktreeError::Refused(
                    "the main checkout is on a detached HEAD: check out a branch to apply to"
                        .into(),
                )
            })?
            .trim()
            .to_string();
        if let Some(what) = self.unfinished_operation() {
            return Err(WorktreeError::Refused(format!(
                "a {what} is not finished in the main checkout: finish or abort it, then apply"
            )));
        }
        let before = self.git.run(&["rev-parse", "HEAD"])?.trim().to_string();
        let range = format!("{}..{}", record.base, record.branch);
        let files = self.paths(&["diff", "--name-only", "--no-renames", "-z", &range, "--"])?;
        let (who, email) = author_of(self.git.identity());
        let picked = self.git.run_with_env(
            &["cherry-pick", &range],
            &[
                ("GIT_COMMITTER_NAME", &who),
                ("GIT_COMMITTER_EMAIL", &email),
                ("GIT_AUTHOR_NAME", &who),
                ("GIT_AUTHOR_EMAIL", &email),
            ],
        );
        if let Err(error) = picked {
            let conflicts = self
                .paths(&["diff", "--name-only", "--diff-filter=U", "-z"])
                .unwrap_or_default();
            if self.unfinished_operation() == Some("cherry-pick") {
                let _ = self.git.run(&["cherry-pick", "--abort"]);
            }
            let after = self.git.run(&["rev-parse", "HEAD"])?.trim().to_string();
            if after != before || self.unfinished_operation().is_some() {
                return Err(WorktreeError::Refused(format!(
                    "applying {} failed ({error}) and could not be undone cleanly: check `git status` in the main checkout",
                    record.branch
                )));
            }
            return Err(if conflicts.is_empty() {
                WorktreeError::Git(error)
            } else {
                WorktreeError::Conflict {
                    branch: record.branch.clone(),
                    files: conflicts,
                }
            });
        }
        let commit = self.git.run(&["rev-parse", "HEAD"])?.trim().to_string();
        // Its change is on the branch now: the worktree and branch have nothing more to hold.
        self.remove(name)?;
        Ok(Applied {
            commit,
            branch: current,
            files,
        })
    }
}
