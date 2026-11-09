//! Looking at what agents left in their worktrees: which results are waiting, the diff of one,
//! and the question of what to do with it (apply, keep the branch, discard).

use harness_core::{
    agents::AgentStatus,
    worktree::{Diff, Overlap, Record},
};
use ratatui::text::{Line, Span};

use crate::{picker::Item, style::Theme, text::sanitize};

/// The most diff lines put in the transcript.
const DIFF_LINES: usize = 400;
/// The most bytes of patch asked of git.
pub const PATCH_BYTES: usize = 200_000;

/// What the user asked to do with a result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Review {
    /// Fetch its diff and the siblings that touched the same files, to show.
    Show(String),
    Apply(String),
    Keep(String),
    Discard(String),
}

/// What came of a [`Review`].
#[derive(Debug, Clone, PartialEq)]
pub enum Reviewed {
    Shown(Box<Shown>),
    /// A sentence for the transcript.
    Applied(String),
    Kept(String),
    Discarded(String),
    /// Why it did not happen; nothing was changed.
    Failed(String),
}

/// A result's diff, ready to show.
#[derive(Debug, Clone, PartialEq)]
pub struct Shown {
    pub record: Record,
    pub diff: Diff,
    pub overlaps: Vec<Overlap>,
}

/// Whether the agent of `record` has ended and left changes: the results to review.
pub fn is_ready(record: &Record) -> bool {
    record.status.is_final() && record.outcome.is_some()
}

/// How a result is described by its state.
pub fn state(record: &Record) -> &'static str {
    match record.status {
        AgentStatus::Done => "ready for review",
        AgentStatus::Failed => "failed, its changes kept",
        AgentStatus::Cancelled => "stopped, its changes kept",
        _ => "working",
    }
}

fn cut(text: &str, cap: usize) -> String {
    let safe = sanitize(text).replace('\n', " ");
    match safe.char_indices().nth(cap) {
        Some((end, _)) => format!("{}…", &safe[..end]),
        None => safe,
    }
}

/// `3 files, +10 -2`.
pub fn counts(record: &Record) -> String {
    match &record.outcome {
        Some(o) => format!(
            "{} file{}, +{} -{}",
            o.files.len(),
            if o.files.len() == 1 { "" } else { "s" },
            o.insertions,
            o.deletions
        ),
        None => "no changes".into(),
    }
}

/// The result of `target`: its name, its branch, or the id of its agent (the newest first).
pub fn find<'a>(records: &'a [Record], target: &str) -> Option<&'a Record> {
    let target = target.trim();
    let ready: Vec<&Record> = records.iter().filter(|r| is_ready(r)).collect();
    ready
        .iter()
        .find(|r| r.name == target || r.branch == target)
        .or_else(|| ready.iter().rev().find(|r| r.agent == target))
        .copied()
}

/// One line for the list of results: who, the task, how it ended.
pub fn describe(record: &Record) -> String {
    format!(
        "{} {} · {} · {} · {}",
        cut(&record.agent, 12),
        cut(&record.label, 24),
        state(record),
        record.branch,
        counts(record)
    )
}

/// The items of the question for a result.
pub fn choices() -> Vec<Item> {
    vec![
        Item::new(
            "Apply",
            "bring the commit into the current branch of the main checkout",
        ),
        Item::new(
            "Keep branch",
            "remove the worktree, keep the branch for later",
        ),
        Item::new("Discard", "remove the worktree and the branch"),
        Item::new("Not now", "decide later"),
    ]
}

/// The items for choosing among results.
pub fn which_items(records: &[&Record]) -> Vec<Item> {
    records
        .iter()
        .map(|r| {
            Item::new(
                &format!("{} · {}", cut(&r.label, 24), cut(&r.task, 50)),
                &format!("{} · {} · {}", state(r), r.branch, counts(r)),
            )
        })
        .collect()
}

/// The diff as lines for the transcript: a header, the changed files, the patch coloured, and
/// when it was cut, how to see the rest.
pub fn lines(shown: &Shown, theme: &Theme) -> Vec<Line<'static>> {
    let record = &shown.record;
    let mut out = vec![
        Line::from(vec![
            Span::styled("Review ", theme.bold()),
            Span::styled(record.branch.clone(), theme.accent()),
            Span::styled(
                format!(" · {} · {}", cut(&record.label, 24), counts(record)),
                theme.dim(),
            ),
        ]),
        Line::from(Span::styled(
            format!("task: {}", cut(&record.task, 160)),
            theme.dim(),
        )),
    ];
    if let Some(summary) = &record.summary {
        out.push(Line::from(Span::styled(
            format!("it said: {}", cut(summary, 200)),
            theme.dim(),
        )));
    }
    for line in shown.diff.stat.lines().take(40) {
        out.push(Line::from(Span::styled(sanitize(line), theme.dim())));
    }
    let patch: Vec<&str> = shown.diff.patch.lines().collect();
    for line in patch.iter().take(DIFF_LINES) {
        let style =
            if line.starts_with("+++") || line.starts_with("---") || line.starts_with("index ") {
                theme.dim()
            } else if line.starts_with("diff --git") {
                theme.bold()
            } else if line.starts_with("@@") {
                theme.hunk()
            } else if line.starts_with('+') {
                theme.added()
            } else if line.starts_with('-') {
                theme.removed()
            } else {
                theme.dim()
            };
        out.push(Line::from(Span::styled(sanitize(line), style)));
    }
    if patch.len() > DIFF_LINES || shown.diff.cut {
        let more = patch.len().saturating_sub(DIFF_LINES);
        out.push(Line::from(Span::styled(
            format!(
                "… {more}{} more lines: `git diff {}..{}` in the repository shows all of it",
                if shown.diff.cut { "+" } else { "" },
                &record.base[..record.base.len().min(10)],
                record.branch
            ),
            theme.warning(),
        )));
    }
    out
}

/// The dim lines under the question: the warning about siblings that touched the same files.
pub fn footer(shown: &Shown) -> Vec<String> {
    let mut out = vec![format!(
        "{} · {}",
        shown.record.branch,
        counts(&shown.record)
    )];
    for overlap in &shown.overlaps {
        let files: Vec<String> = overlap.files.iter().take(5).map(|f| cut(f, 60)).collect();
        out.push(format!(
            "warning: also changed by harness/{}: {}{}",
            overlap.other,
            files.join(", "),
            if overlap.files.len() > 5 { ", …" } else { "" }
        ));
    }
    out
}

/// Lines for the transcript about the siblings, as the diff is shown.
pub fn overlap_lines(shown: &Shown, theme: &Theme) -> Vec<Line<'static>> {
    shown
        .overlaps
        .iter()
        .map(|o| {
            Line::from(Span::styled(
                format!(
                    "warning: harness/{} also changed {}: applying both may conflict",
                    o.other,
                    o.files
                        .iter()
                        .take(5)
                        .map(|f| cut(f, 60))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                theme.warning(),
            ))
        })
        .collect()
}
