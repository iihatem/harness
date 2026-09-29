//! Text from the model, tools, files and the user, made safe to draw, and wrapped to a width.

use ratatui::{
    style::Style,
    text::{Line, Span},
};
use unicode_width::UnicodeWidthChar;

/// Columns a tab takes.
const TAB: &str = "    ";

/// `text` made safe to draw: every control character, and every character that reorders text on
/// screen (bidirectional marks, embeddings, overrides and isolates), is shown as an escape such as
/// `\u{1b}`, so it cannot move the cursor or disguise what is shown. A tab becomes four spaces;
/// `\n` is kept, and `\r\n` becomes `\n`.
pub fn sanitize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\n' => out.push('\n'),
            '\t' => out.push_str(TAB),
            '\r' if chars.peek() == Some(&'\n') => {}
            c if c.is_control() || is_bidi_control(c) => out.extend(c.escape_default()),
            c => out.push(c),
        }
    }
    out
}

fn is_bidi_control(c: char) -> bool {
    matches!(
        c,
        '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
    )
}

/// Columns `text` takes on screen.
pub fn width(text: &str) -> usize {
    text.chars().map(|c| c.width().unwrap_or(0)).sum()
}

/// One line per line of `text` (sanitized), each in `style`.
pub fn lines(text: &str, style: Style) -> Vec<Line<'static>> {
    sanitize(text)
        .split('\n')
        .map(|l| Line::from(Span::styled(l.to_string(), style)))
        .collect()
}

/// `line` broken into lines at most `width` columns wide: between words where it can, inside a
/// word where it must. The first line starts with `first`, the others with `rest`, which count
/// towards the width. Spaces where a line breaks are dropped.
pub fn wrap(
    line: &Line<'_>,
    width: usize,
    first: &[Span<'static>],
    rest: &[Span<'static>],
) -> Vec<Line<'static>> {
    let prefix_width = |p: &[Span<'static>]| p.iter().map(|s| self::width(&s.content)).sum();
    let chars: Vec<(char, Style)> = line
        .spans
        .iter()
        .flat_map(|span| {
            let style = line.style.patch(span.style);
            span.content.chars().map(move |c| (c, style))
        })
        .collect();
    let mut out = Vec::new();
    let mut current: Vec<(char, Style)> = Vec::new();
    let mut used = 0;
    let mut available = width.saturating_sub(prefix_width(first)).max(1);
    let finish = |current: &mut Vec<(char, Style)>, out: &mut Vec<Line<'static>>| {
        while current.last().is_some_and(|(c, _)| *c == ' ') {
            current.pop();
        }
        let prefix = if out.is_empty() { first } else { rest };
        let mut spans = prefix.to_vec();
        spans.extend(merge(current));
        out.push(Line::from(spans));
        current.clear();
    };
    let mut i = 0;
    while i < chars.len() {
        // The next word and the spaces after it.
        let start = i;
        while i < chars.len() && chars[i].0 != ' ' {
            i += 1;
        }
        let word_end = i;
        while i < chars.len() && chars[i].0 == ' ' {
            i += 1;
        }
        let word: usize = chars[start..word_end]
            .iter()
            .map(|(c, _)| c.width().unwrap_or(0))
            .sum();
        if used + word > available && used > 0 {
            finish(&mut current, &mut out);
            used = 0;
            available = width.saturating_sub(prefix_width(rest)).max(1);
        }
        for &(c, style) in &chars[start..i] {
            let w = c.width().unwrap_or(0);
            if c == ' ' && used == 0 && !current.is_empty() {
                continue;
            }
            if used + w > available {
                if c == ' ' {
                    continue;
                }
                finish(&mut current, &mut out);
                used = 0;
                available = width.saturating_sub(prefix_width(rest)).max(1);
            }
            current.push((c, style));
            used += w;
        }
    }
    finish(&mut current, &mut out);
    out
}

/// Consecutive characters of the same style as one span each.
fn merge(chars: &[(char, Style)]) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    for &(c, style) in chars {
        match spans.last_mut() {
            Some(last) if last.style == style => last.content.to_mut().push(c),
            _ => spans.push(Span::styled(c.to_string(), style)),
        }
    }
    spans
}

/// The text of `line`, without styles.
pub fn plain(line: &Line<'_>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}
