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
    /// Something that did not go as it should after the commit landed (the worktree or branch
    /// could not be removed): the change is applied all the same.
    pub note: Option<String>,
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

/// The paths a `status --porcelain=v1 -z` lists, but for the untracked files that
/// `.worktreeinclude` copied in.
pub(super) fn changes_in(
    status: &str,
    included: &std::collections::HashSet<String>,
) -> Vec<String> {
    let mut out = Vec::new();
    let mut parts = status.split('\0').filter(|p| !p.is_empty());
    while let Some(part) = parts.next() {
        if part.len() > 3 {
            let path = &part[3..];
            if !(part.starts_with("??") && included.contains(path)) {
                out.push(path.to_string());
            }
        }
        if part.starts_with(['R', 'C']) {
            out.extend(parts.next().map(str::to_string));
        }
    }
    out
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
    /// repository's hooks. `None` only when it changed nothing: the tree is clean and its branch
    /// is still at the commit it was made from. Commits the agent made itself are a result too.
    /// Fails, changing nothing, when the worktree is no longer on its branch.
    pub fn commit_result(
        &self,
        wt: &Worktree,
        label: &str,
        task: &str,
    ) -> Result<Option<Outcome>, WorktreeError> {
        self.check_gitdir(&wt.path)?;
        let git = self.git.at(&wt.path);
        let on = git
            .run(&["symbolic-ref", "--quiet", "HEAD"])
            .map(|head| head.trim().to_string())
            .unwrap_or_default();
        if on != format!("refs/heads/{}", wt.branch) {
            return Err(WorktreeError::Refused(format!(
                "the worktree is no longer on its branch {}, so nothing was committed",
                wt.branch
            )));
        }
        git.run(&["add", "-A"])?;
        // What `.worktreeinclude` copied in is never committed, whatever ignore rules the
        // worktree has (the rule that ignores it in the main checkout may be uncommitted).
        let copied: Vec<String> = self.included_paths(&wt.path).into_iter().collect();
        for chunk in copied.chunks(200) {
            let mut args = vec!["reset", "-q", "--"];
            args.extend(chunk.iter().map(String::as_str));
            git.run_with_env(&args, &[("GIT_LITERAL_PATHSPECS", "1")])?;
        }
        let staged = match git.run(&[
            "diff",
            "--cached",
            "--quiet",
            "--no-ext-diff",
            "--no-textconv",
        ]) {
            Ok(_) => false,
            Err(e) if e.code == Some(1) => true,
            Err(e) => return Err(e.into()),
        };
        if !staged {
            // Nothing new to commit: the agent's own commits, if it made any, are the result.
            let tip = git.run(&["rev-parse", "HEAD"])?.trim().to_string();
            if tip == wt.base {
                return Ok(None);
            }
            return self.outcome_of(&git, wt, tip).map(Some);
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
        self.outcome_of(&git, wt, commit).map(Some)
    }

    /// What the commits from the base of `wt` to `commit` changed.
    fn outcome_of(
        &self,
        git: &super::Git,
        wt: &Worktree,
        commit: String,
    ) -> Result<Outcome, WorktreeError> {
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
        Ok(Outcome {
            branch: wt.branch.clone(),
            commit,
            files,
            insertions,
            deletions,
        })
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
            && self.check_gitdir(&record.path).is_ok()
            && let Ok(status) =
                self.git
                    .at(&record.path)
                    .run(&["status", "--porcelain=v1", "-z", "-uall"])
        {
            files.extend(changes_in(&status, &self.included_paths(&record.path)));
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
        // (The commit has landed, so a cleanup that fails is said, not an error.)
        let note = self
            .remove_unchecked(name)
            .err()
            .map(|e| format!("applied, but its worktree or branch could not be removed: {e}"));
        Ok(Applied {
            commit,
            branch: current,
            files,
            note,
        })
    }
}
