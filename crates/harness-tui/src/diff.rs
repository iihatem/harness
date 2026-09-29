//! Diffs of a file's old and new text, as a unified diff with coloured `-` and `+` lines.

use std::time::Duration;

use ratatui::text::{Line, Span};
use similar::{ChangeTag, TextDiff};

use crate::{style::Theme, text::sanitize};

/// How long the diff may take before it settles for a coarser answer.
const TIMEOUT: Duration = Duration::from_millis(500);

/// A unified diff of `old` and `new`, with `context` unchanged lines around each change:
/// `@@ -a,b +c,d @@` headers, then lines marked ` `, `-` and `+`. Empty when they are equal.
pub fn unified(old: &str, new: &str, context: usize, theme: &Theme) -> Vec<Line<'static>> {
    let diff = TextDiff::configure().timeout(TIMEOUT).diff_lines(old, new);
    let mut out = Vec::new();
    for hunk in diff.unified_diff().context_radius(context).iter_hunks() {
        out.push(Line::from(Span::styled(
            hunk.header().to_string(),
            theme.hunk(),
        )));
        for change in hunk.iter_changes() {
            let (marker, style) = match change.tag() {
                ChangeTag::Delete => ("-", theme.removed()),
                ChangeTag::Insert => ("+", theme.added()),
                ChangeTag::Equal => (" ", theme.dim()),
            };
            let text = sanitize(change.value().trim_end_matches(['\n', '\r']));
            out.push(Line::from(Span::styled(format!("{marker}{text}"), style)));
        }
    }
    out
}

/// How many lines `new` adds to `old`, and how many it removes.
pub fn counts(old: &str, new: &str) -> (usize, usize) {
    let diff = TextDiff::configure().timeout(TIMEOUT).diff_lines(old, new);
    let mut added = 0;
    let mut removed = 0;
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => added += 1,
            ChangeTag::Delete => removed += 1,
            ChangeTag::Equal => {}
        }
    }
    (added, removed)
}
