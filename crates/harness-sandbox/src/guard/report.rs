//! What the guard found and did around one command, and how the tool result
//! reports it.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use harness_core::tool::GuardReport;

/// How many repositories the report lists that it could not check; the rest
/// are counted.
const MAX_LISTED: usize = 10;

const BEFORE: &str = "[before this command ran, harness found protected git metadata created or changed after the previous command ended, probably by a process it left running:";

const AFTER: &str = "[the sandbox undid changes this command made to protected git metadata (hooks, config, commondir, repositories, .harness/, a top-level HEAD), which git outside the sandbox would otherwise use:";

const UNCHECKABLE: &str = "[this command left the workspace too large or unreadable for harness to scan completely (a directory, gitfile or commondir file it cannot read, a modules/ tree over 64 levels deep, or more than 200,000 entries or 5 seconds of scanning), so harness cannot tell whether it created a repository, and the command counts as blocked. harness reads .gitignore rules once per session: rules that skip directories created since then apply after harness restarts.]\n";

const UNCHECKED: &str = "[found, not checked: harness could not scan the whole workspace, so it cannot tell whether these are new:";

const GONE: &str = "[git metadata harness protected before this command:";

const GONE_LINE: &str =
    "no longer at this path: moved or deleted; harness does not protect it where it went";

const INCOMPLETE: &str = "[harness could not scan the whole workspace for git metadata (a directory, gitfile or commondir file it cannot read, an ignore file it cannot use, a modules/ tree over 64 levels deep, or more than 200,000 entries or 5 seconds of scanning), so it does not protect repositories in the part it missed, and lists new repositories it finds after a command instead of moving them to quarantine. It says this once per session.]\n";

/// One thing the guard found, and what it did about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Finding {
    pub(super) path: PathBuf,
    pub(super) what: What,
    pub(super) outcome: Outcome,
}

impl Finding {
    fn failed(&self) -> bool {
        matches!(self.outcome, Outcome::Failed(_))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum What {
    /// A protected name that did not exist before.
    New,
    /// A new `.git` entry.
    Repository,
    /// A new linked-worktree or submodule gitdir.
    Gitdir,
    /// A `.git` entry, gitdir or link that is not what it was.
    Replaced,
    /// A protected file whose content, type or permissions changed.
    Changed,
    /// A protected file that is gone.
    Deleted,
    /// A new entry in a protected directory.
    Added,
    /// Protected metadata that harness can no longer look at.
    Unreachable,
}

impl What {
    fn describe(self) -> &'static str {
        match self {
            What::New => "new",
            What::Repository => "a new repository",
            What::Gitdir => "a new worktree or submodule gitdir",
            What::Replaced => "moved or replaced",
            What::Changed => "changed",
            What::Deleted => "deleted",
            What::Added => "new in a protected directory",
            What::Unreachable => "could not be checked",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Outcome {
    Moved(PathBuf),
    /// Put back as it was; the changed version, if any, is in quarantine.
    Restored(Option<PathBuf>),
    /// Not handled: what is there is left as it is.
    Failed(String),
}

impl Outcome {
    fn describe(&self) -> String {
        match self {
            Outcome::Moved(to) => format!("moved to {}", to.display()),
            Outcome::Restored(Some(to)) => format!(
                "restored the earlier version (the changed one is in {})",
                to.display()
            ),
            Outcome::Restored(None) => "restored the earlier version".into(),
            Outcome::Failed(why) => why.clone(),
        }
    }
}

/// Adds `finding` to `list` unless it is there already: a check that runs
/// again while a failure lasts finds it again.
pub(super) fn push(list: &mut Vec<Finding>, finding: Option<Finding>) {
    if let Some(finding) = finding
        && !list.contains(&finding)
    {
        list.push(finding);
    }
}

/// Everything one command's guard has to report.
#[derive(Debug, Default)]
pub(super) struct Findings {
    /// Found before the command ran: not the command's doing.
    pub(super) before: Vec<Finding>,
    /// Found while the command ran or after it ended.
    pub(super) after: Vec<Finding>,
    /// `.git` entries, gitdirs and links that were there before the command
    /// and are no longer where they were.
    pub(super) gone: BTreeSet<PathBuf>,
    /// New `.git` entries left in place because the scan before the command
    /// was incomplete.
    pub(super) unchecked: Vec<PathBuf>,
    /// The scan before the command was complete, and the one after it not.
    pub(super) uncheckable: bool,
    /// The scan before the command was incomplete, and the session has not
    /// said so yet.
    pub(super) incomplete: bool,
}

impl Findings {
    /// Whether the command counts as blocked: the guard undid something it
    /// did, it made the workspace uncheckable, or something could not be
    /// handled.
    pub(super) fn blocked(&self) -> bool {
        !self.after.is_empty() || self.uncheckable || self.before.iter().any(Finding::failed)
    }

    /// The report for the tool result, with paths relative to `workspace`.
    pub(super) fn report(&self, workspace: &Path) -> Option<GuardReport> {
        let rel = |path: &Path| {
            path.strip_prefix(workspace)
                .unwrap_or(path)
                .display()
                .to_string()
        };
        let mut message = String::new();
        for (header, findings) in [(BEFORE, &self.before), (AFTER, &self.after)] {
            if findings.is_empty() {
                continue;
            }
            message.push_str(header);
            for finding in findings {
                let _ = write!(
                    message,
                    "\n- {}: {}; {}",
                    rel(&finding.path),
                    finding.what.describe(),
                    finding.outcome.describe()
                );
            }
            message.push_str("]\n");
        }
        if self.uncheckable {
            message.push_str(UNCHECKABLE);
        }
        if !self.unchecked.is_empty() {
            message.push_str(UNCHECKED);
            for path in self.unchecked.iter().take(MAX_LISTED) {
                let _ = write!(message, "\n- {}", rel(path));
            }
            if self.unchecked.len() > MAX_LISTED {
                let _ = write!(
                    message,
                    "\n- and {} more",
                    self.unchecked.len() - MAX_LISTED
                );
            }
            message.push_str("]\n");
        }
        // Below a path that is gone, everything is.
        let gone: Vec<&PathBuf> = self
            .gone
            .iter()
            .filter(|path| {
                !path
                    .ancestors()
                    .skip(1)
                    .any(|above| self.gone.contains(above))
            })
            .collect();
        if !gone.is_empty() {
            message.push_str(GONE);
            for path in gone {
                let _ = write!(message, "\n- {}: {GONE_LINE}", rel(path));
            }
            message.push_str("]\n");
        }
        if self.incomplete {
            message.push_str(INCOMPLETE);
        }
        (!message.is_empty()).then(|| GuardReport {
            message,
            blocked: self.blocked(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(path: &str, what: What, outcome: Outcome) -> Finding {
        Finding {
            path: PathBuf::from("/ws").join(path),
            what,
            outcome,
        }
    }

    #[test]
    fn nothing_found_is_no_report() {
        assert_eq!(Findings::default().report(Path::new("/ws")), None);
    }

    #[test]
    fn what_was_found_before_the_command_does_not_block_it_unless_it_failed() {
        let mut findings = Findings::default();
        let moved = Outcome::Moved(PathBuf::from("/q/1/HEAD"));
        push(
            &mut findings.before,
            Some(finding("HEAD", What::New, moved)),
        );
        let report = findings.report(Path::new("/ws")).unwrap();
        assert_eq!(
            report.message,
            format!("{BEFORE}\n- HEAD: new; moved to /q/1/HEAD]\n")
        );
        assert!(!report.blocked);
        let failed = Outcome::Failed("could not move it: busy".into());
        push(
            &mut findings.before,
            Some(finding("pid", What::New, failed)),
        );
        assert!(findings.report(Path::new("/ws")).unwrap().blocked);
    }

    #[test]
    fn a_finding_is_listed_once() {
        let mut findings = Findings::default();
        let failed = || Outcome::Failed("could not restore the earlier version: busy".into());
        for _ in 0..3 {
            push(
                &mut findings.after,
                Some(finding(".git/config", What::Deleted, failed())),
            );
        }
        assert_eq!(findings.after.len(), 1);
    }

    #[test]
    fn only_the_top_of_what_is_gone_is_listed() {
        let mut findings = Findings::default();
        for path in ["a", "a/.git", "a/.git/modules/m", "b/.git"] {
            findings.gone.insert(PathBuf::from("/ws").join(path));
        }
        let report = findings.report(Path::new("/ws")).unwrap();
        assert_eq!(
            report.message,
            format!("{GONE}\n- a: {GONE_LINE}\n- b/.git: {GONE_LINE}]\n")
        );
        assert!(!report.blocked);
    }
}
