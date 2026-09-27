//! What the guard found and did around one command, and how the tool result
//! reports it.

use std::collections::{BTreeSet, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use harness_core::tool::GuardReport;

/// How many repositories the report lists that it could not check; the rest
/// are counted.
const MAX_LISTED: usize = 10;

/// How many lines one section of the report lists; the rest are counted.
const MAX_LINES: usize = 50;

pub(super) const BEFORE: &str = "[before this command ran, harness found protected git metadata created or changed after the previous command ended, probably by a process it left running:";

const AFTER: &str = "[the sandbox undid changes this command made to protected git metadata (hooks, config, commondir, repositories, .harness/, a top-level HEAD), which git outside the sandbox would otherwise use:";

const UNCHECKABLE: &str = "[this command left the workspace too large or unreadable for harness to scan completely (a directory, gitfile or commondir file it cannot read, a modules/ tree over 64 levels deep, or more than 200,000 entries or 5 seconds of scanning), so harness cannot tell whether it created a repository, and the command counts as blocked. harness reads .gitignore rules once per session: rules that skip directories created since then apply after harness restarts.]\n";

const UNCHECKED: &str = "[found, not checked: harness could not scan the whole workspace, so it cannot tell whether these are new:";

const GONE: &str = "[git metadata harness protected before this command:";

const GONE_LINE: &str =
    "no longer at this path: moved or deleted; harness does not protect it where it went";

const INCOMPLETE: &str = "[harness could not scan the whole workspace for git metadata (a directory, gitfile or commondir file it cannot read, an ignore file it cannot use, a modules/ tree over 64 levels deep, or more than 200,000 entries or 5 seconds of scanning), so it does not protect repositories in the part it missed, and lists new repositories it finds after a command instead of moving them to quarantine. It says this once per session.]\n";

/// One thing the guard found, and what it did about it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
    /// A directory whose owner lost read, write or search permission, which
    /// kept the guard from undoing a change.
    Locked,
    /// Something stored in quarantine that git may still take for a
    /// repository.
    Live,
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
            What::Locked => "its owner lost read, write or search permission",
            What::Live => "stored in quarantine, but not fully neutralized",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum Outcome {
    Moved(PathBuf),
    /// Put back as it was; the changed version, if any, is in quarantine.
    Restored(Option<PathBuf>),
    /// The owner's read, write and search permission given back.
    Unlocked,
    /// Said once, and left: it does not block the command.
    Note(String),
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
            Outcome::Unlocked => "gave them back".into(),
            Outcome::Note(why) => why.clone(),
            Outcome::Failed(why) => why.clone(),
        }
    }
}

/// Findings in the order found, each once: a check that runs again while a
/// failure lasts finds it again.
#[derive(Debug, Default)]
pub(super) struct List {
    items: Vec<Finding>,
    seen: HashSet<Finding>,
}

impl List {
    pub(super) fn push(&mut self, finding: Option<Finding>) {
        if let Some(finding) = finding
            && self.seen.insert(finding.clone())
        {
            self.items.push(finding);
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.items.len()
    }
}

/// Everything one command's guard has to report.
#[derive(Debug, Default)]
pub(super) struct Findings {
    /// Found before the command ran: not the command's doing.
    pub(super) before: List,
    /// Found while the command ran or after it ended.
    pub(super) after: List,
    /// `.git` entries, gitdirs and links that were there before the command
    /// and are no longer where they were.
    pub(super) gone: BTreeSet<PathBuf>,
    /// New `.git` entries and gitdirs left in place because the scan before
    /// the command was incomplete.
    pub(super) unchecked: BTreeSet<PathBuf>,
    /// The scan before the command was complete, and the one after it not.
    pub(super) uncheckable: bool,
    /// The scan before the command was incomplete, and the session has not
    /// said so yet.
    pub(super) incomplete: bool,
    /// What the checks left as it is, past the most changes one check
    /// makes: moved or restored before the next command.
    pub(super) undone: Vec<(PathBuf, What)>,
    /// The most changes one check makes.
    pub(super) max_changes: usize,
    /// What an earlier command could not move or restore, and this one
    /// could not either: recalled, without blocking.
    pub(super) stuck: Vec<PathBuf>,
}

impl Findings {
    /// Whether the command counts as blocked: the guard undid something it
    /// did, it made the workspace uncheckable, or something could not be
    /// handled.
    pub(super) fn blocked(&self) -> bool {
        !self.after.is_empty()
            || self.uncheckable
            || !self.undone.is_empty()
            || self.before.items.iter().any(Finding::failed)
    }

    /// The report for the tool result, with paths relative to `workspace`.
    pub(super) fn report(&self, workspace: &Path) -> Option<GuardReport> {
        let rel = |path: &Path| match path.strip_prefix(workspace) {
            Ok(rel) if rel.as_os_str().is_empty() => ".".to_string(),
            Ok(rel) => rel.display().to_string(),
            Err(_) => path.display().to_string(),
        };
        let mut message = String::new();
        for (header, findings) in [(BEFORE, &self.before), (AFTER, &self.after)] {
            if !findings.is_empty() {
                let lines = findings.items.iter().map(|finding| {
                    format!(
                        "{}: {}; {}",
                        rel(&finding.path),
                        finding.what.describe(),
                        finding.outcome.describe()
                    )
                });
                section(&mut message, header, lines, findings.items.len(), MAX_LINES);
            }
        }
        if !self.undone.is_empty() {
            let header = format!(
                "[harness stops at {} changes in one check, and left these as they are; it goes on with them before the next command, and the command counts as blocked:",
                self.max_changes
            );
            let lines = self
                .undone
                .iter()
                .map(|(path, what)| format!("{}: {}", rel(path), what.describe()));
            section(&mut message, &header, lines, self.undone.len(), MAX_LINES);
        }
        if self.uncheckable {
            message.push_str(UNCHECKABLE);
        }
        if !self.unchecked.is_empty() {
            let lines = self.unchecked.iter().map(|path| rel(path));
            section(
                &mut message,
                UNCHECKED,
                lines,
                self.unchecked.len(),
                MAX_LISTED,
            );
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
            let lines = gone
                .iter()
                .map(|path| format!("{}: {GONE_LINE}", rel(path)));
            section(&mut message, GONE, lines, gone.len(), MAX_LINES);
        }
        if !self.stuck.is_empty() {
            let count = self.stuck.len();
            let (entries, is, was) = if count == 1 {
                ("entry", "is", "it was")
            } else {
                ("entries", "are", "they were")
            };
            let mut listed: Vec<String> = self
                .stuck
                .iter()
                .take(MAX_LISTED)
                .map(|path| rel(path))
                .collect();
            if count > MAX_LISTED {
                listed.push(format!("and {} more", count - MAX_LISTED));
            }
            let _ = writeln!(
                message,
                "[{count} {entries} harness could not move or restore {is} still as {was}: {}]",
                listed.join(", ")
            );
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

/// Adds `header`, then the first `max` of the `count` `lines`, then how many
/// more there are.
fn section(
    message: &mut String,
    header: &str,
    lines: impl Iterator<Item = String>,
    count: usize,
    max: usize,
) {
    message.push_str(header);
    for line in lines.take(max) {
        let _ = write!(message, "\n- {line}");
    }
    if count > max {
        let _ = write!(message, "\n- and {} more", count - max);
    }
    message.push_str("]\n");
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
        findings
            .before
            .push(Some(finding("HEAD", What::New, moved)));
        let report = findings.report(Path::new("/ws")).unwrap();
        assert_eq!(
            report.message,
            format!("{BEFORE}\n- HEAD: new; moved to /q/1/HEAD]\n")
        );
        assert!(!report.blocked);
        let failed = Outcome::Failed("could not move it: busy".into());
        findings
            .before
            .push(Some(finding("pid", What::New, failed)));
        assert!(findings.report(Path::new("/ws")).unwrap().blocked);
    }

    #[test]
    fn a_finding_is_listed_once() {
        let mut findings = Findings::default();
        let failed = || Outcome::Failed("could not restore the earlier version: busy".into());
        for _ in 0..3 {
            findings
                .after
                .push(Some(finding(".git/config", What::Deleted, failed())));
        }
        assert_eq!(findings.after.len(), 1);
    }

    #[test]
    fn a_section_lists_fifty_lines_then_a_count() {
        let mut findings = Findings::default();
        for i in 0..120 {
            let moved = Outcome::Moved(PathBuf::from(format!("/q/1/h{i}")));
            findings
                .after
                .push(Some(finding(&format!("h{i}"), What::Added, moved)));
        }
        let report = findings.report(Path::new("/ws")).unwrap();
        assert_eq!(report.message.matches("\n- h").count(), 50);
        assert!(
            report.message.ends_with("\n- and 70 more]\n"),
            "{}",
            report.message
        );
    }

    #[test]
    fn what_was_left_undone_is_named_and_blocks_the_command() {
        let findings = Findings {
            undone: vec![(PathBuf::from("/ws/zz/.git"), What::Repository)],
            max_changes: 3,
            ..Findings::default()
        };
        let report = findings.report(Path::new("/ws")).unwrap();
        assert!(report.blocked);
        assert_eq!(
            report.message,
            "[harness stops at 3 changes in one check, and left these as they are; it goes on with them before the next command, and the command counts as blocked:\n- zz/.git: a new repository]\n"
        );
    }

    #[test]
    fn the_workspace_itself_is_shown_as_a_dot() {
        let mut findings = Findings::default();
        let locked = Finding {
            path: PathBuf::from("/ws"),
            what: What::Locked,
            outcome: Outcome::Unlocked,
        };
        findings.after.push(Some(locked));
        let report = findings.report(Path::new("/ws")).unwrap();
        assert_eq!(
            report.message,
            format!(
                "{AFTER}\n- .: its owner lost read, write or search permission; gave them back]\n"
            )
        );
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
