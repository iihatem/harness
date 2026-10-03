//! Text from the model, tools, files and the user, made safe to draw, and wrapped to a width.

use ratatui::{
    style::Style,
    text::{Line, Span},
};
use unicode_segmentation::{GraphemeCursor, UnicodeSegmentation};
use unicode_width::UnicodeWidthStr;

/// Columns a tab takes.
const TAB: &str = "    ";

/// `text` made safe to draw: every control character, every character that reorders text on
/// screen (bidirectional marks, embeddings, overrides and isolates), and every invisible format
/// character (zero-width spaces and joiners, the soft hyphen, the byte-order mark, tag
/// characters, line and paragraph separators) is shown as an escape such as `\u{1b}`, so it
/// cannot move the cursor, or disguise what is shown by reordering it or hiding in it. Joiners
/// inside an emoji sequence or after a letter of a script written with them, and the tag
/// characters of a subdivision flag, are kept. A tab becomes four spaces; `\n` is kept, and
/// `\r\n` becomes `\n`.
pub fn sanitize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        match c {
            '\n' => out.push('\n'),
            '\t' => out.push_str(TAB),
            '\r' if chars.peek().is_some_and(|(_, next)| *next == '\n') => {}
            c if hidden(text, at, c) => out.extend(c.escape_default()),
            c => out.push(c),
        }
    }
    out
}

/// `text` without what [`sanitize`] shows as escapes, line breaks and tabs included: for text
/// that cannot show an escape, such as a desktop notification's.
pub fn strip(text: &str) -> String {
    text.char_indices()
        .filter(|&(at, c)| !hidden(text, at, c))
        .map(|(_, c)| c)
        .collect()
}

/// Whether `c`, at byte `at` of `text`, is not drawn as it is: a control character, one that
/// reorders text, or an invisible format character that the emoji or the script around it does
/// not need.
fn hidden(text: &str, at: usize, c: char) -> bool {
    if c.is_control() || is_bidi_control(c) {
        return true;
    }
    if !is_invisible(c) || is_visible_format(c) {
        return false;
    }
    match c {
        // A zero-width joiner inside a grapheme cluster joins it to what follows (an emoji ZWJ
        // sequence, an Indic conjunct). Both joiners shape the letters of the scripts written
        // with them (Persian, Arabic, the Indic scripts).
        '\u{200c}' | '\u{200d}' => {
            let joins = c == '\u{200d}' && !is_boundary(text, at + c.len_utf8());
            let base = cluster_base(text, at);
            !(joins || (base.is_alphabetic() && !base.is_ascii()))
        }
        // Tag characters spell a subdivision flag after a black flag.
        '\u{e0020}'..='\u{e007f}' => cluster_base(text, at) != '\u{1f3f4}',
        _ => true,
    }
}

fn is_bidi_control(c: char) -> bool {
    matches!(
        c,
        '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
    )
}

/// Whether `c` draws as nothing, or not where it is: format characters (Unicode's `Cf`: zero-width
/// spaces and joiners, the soft hyphen, the byte-order mark, tag characters and the rest) and
/// the line and paragraph separators. The terminal drops them, so two strings that differ only
/// in them look the same.
fn is_invisible(c: char) -> bool {
    matches!(
        c,
        '\u{00ad}'
            | '\u{0600}'..='\u{0605}'
            | '\u{061c}'
            | '\u{06dd}'
            | '\u{070f}'
            | '\u{0890}'..='\u{0891}'
            | '\u{08e2}'
            | '\u{180e}'
            | '\u{200b}'..='\u{200f}'
            | '\u{2028}'..='\u{202e}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206f}'
            | '\u{feff}'
            | '\u{fff9}'..='\u{fffb}'
            | '\u{110bd}'
            | '\u{110cd}'
            | '\u{13430}'..='\u{1343f}'
            | '\u{1bca0}'..='\u{1bca3}'
            | '\u{1d173}'..='\u{1d17a}'
            | '\u{e0001}'
            | '\u{e0020}'..='\u{e007f}'
    )
}

/// The format characters that do draw: the signs written before a number in Arabic, Syriac and
/// Kaithi (the Arabic number sign, the end of an ayah).
fn is_visible_format(c: char) -> bool {
    matches!(
        c,
        '\u{0600}'..='\u{0605}'
            | '\u{06dd}'
            | '\u{070f}'
            | '\u{0890}'..='\u{0891}'
            | '\u{08e2}'
            | '\u{110bd}'
            | '\u{110cd}'
    )
}

/// `text`, already [sanitized](sanitize), with what would not show exactly as it is shown as an
/// escape too: the [invisible](is_invisible) characters `sanitize` keeps for an emoji or a script
/// (a joiner in an emoji, a non-joiner in Persian), and `\n`, which one line cannot show. For what
/// the user approves, where what they see must be what runs.
pub fn reveal(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c == '\n' || is_invisible(c) {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out
}

/// Whether a grapheme cluster starts at byte `at` of `text`.
fn is_boundary(text: &str, at: usize) -> bool {
    GraphemeCursor::new(at, text.len(), true)
        .is_boundary(text, 0)
        .unwrap_or(true)
}

/// The first character of the grapheme cluster the character at byte `at` of `text` is in.
fn cluster_base(text: &str, at: usize) -> char {
    let start = if is_boundary(text, at) {
        at
    } else {
        GraphemeCursor::new(at, text.len(), true)
            .prev_boundary(text, 0)
            .ok()
            .flatten()
            .unwrap_or(0)
    };
    text[start..].chars().next().unwrap_or_default()
}

/// Columns `text` takes on screen, counted as ratatui draws it: by grapheme cluster, so an
/// emoji with a variation selector (⚠️), a ZWJ sequence (👨‍👩‍👧‍👦) or a flag is one character two
/// columns wide, and a letter with combining marks is one column.
pub fn width(text: &str) -> usize {
    text.graphemes(true).map(grapheme_width).sum()
}

/// Columns one grapheme cluster takes, as ratatui's buffer counts them: a cluster with a
/// control character in it is not drawn at all.
pub fn grapheme_width(grapheme: &str) -> usize {
    if grapheme.contains(char::is_control) {
        return 0;
    }
    if grapheme.len() == 1 {
        return 1;
    }
    // unicode-width counts the halfwidth (han)dakuten as combining; terminals, and ratatui,
    // give each a column.
    let marks = grapheme
        .chars()
        .filter(|c| matches!(c, '\u{ff9e}' | '\u{ff9f}'))
        .count();
    grapheme.width() + marks
}

/// One line per line of `text` (sanitized), each in `style`.
pub fn lines(text: &str, style: Style) -> Vec<Line<'static>> {
    sanitize(text)
        .split('\n')
        .map(|l| Line::from(Span::styled(l.to_string(), style)))
        .collect()
}

/// `line` broken into lines at most `width` columns wide: between words where it can, inside a
/// word (between grapheme clusters) where it must. The first line starts with `first`, the others
/// with `rest`, which count towards the width; a prefix wider than half the width (quotes and
/// lists nested deep) is cut to half. Spaces where a line breaks are dropped.
pub fn wrap(
    line: &Line<'_>,
    width: usize,
    first: &[Span<'static>],
    rest: &[Span<'static>],
) -> Vec<Line<'static>> {
    let first = &fit(first, width / 2);
    let rest = &fit(rest, width / 2);
    let prefix_width = |p: &[Span<'static>]| p.iter().map(|s| self::width(&s.content)).sum();
    let cells: Vec<(&str, Style)> = line
        .spans
        .iter()
        .flat_map(|span| {
            let style = line.style.patch(span.style);
            span.content.graphemes(true).map(move |g| (g, style))
        })
        .collect();
    let mut out = Vec::new();
    let mut current: Vec<(&str, Style)> = Vec::new();
    let mut used = 0;
    let mut available = width.saturating_sub(prefix_width(first)).max(1);
    let finish = |current: &mut Vec<(&str, Style)>, out: &mut Vec<Line<'static>>| {
        while current.last().is_some_and(|(g, _)| *g == " ") {
            current.pop();
        }
        let prefix = if out.is_empty() { first } else { rest };
        let mut spans = prefix.to_vec();
        spans.extend(merge(current));
        out.push(Line::from(spans));
        current.clear();
    };
    let mut i = 0;
    while i < cells.len() {
        // The next word and the spaces after it.
        let start = i;
        while i < cells.len() && cells[i].0 != " " {
            i += 1;
        }
        let word_end = i;
        while i < cells.len() && cells[i].0 == " " {
            i += 1;
        }
        let word: usize = cells[start..word_end]
            .iter()
            .map(|(g, _)| grapheme_width(g))
            .sum();
        if used + word > available && used > 0 {
            finish(&mut current, &mut out);
            used = 0;
            available = width.saturating_sub(prefix_width(rest)).max(1);
        }
        for &(g, style) in &cells[start..i] {
            let w = grapheme_width(g);
            if g == " " && used == 0 && !current.is_empty() {
                continue;
            }
            if used + w > available {
                if g == " " {
                    continue;
                }
                finish(&mut current, &mut out);
                used = 0;
                available = width.saturating_sub(prefix_width(rest)).max(1);
            }
            current.push((g, style));
            used += w;
        }
    }
    finish(&mut current, &mut out);
    out
}

/// `prefix`, cut to at most `width` columns.
fn fit(prefix: &[Span<'static>], width: usize) -> Vec<Span<'static>> {
    let mut left = width;
    let mut out = Vec::new();
    for span in prefix {
        let mut content = String::new();
        for grapheme in span.content.graphemes(true) {
            let w = grapheme_width(grapheme);
            if w > left {
                left = 0;
                break;
            }
            left -= w;
            content.push_str(grapheme);
        }
        if !content.is_empty() {
            out.push(Span::styled(content, span.style));
        }
        if left == 0 {
            break;
        }
    }
    out
}

/// Consecutive grapheme clusters of the same style as one span each.
fn merge(cells: &[(&str, Style)]) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    for &(g, style) in cells {
        match spans.last_mut() {
            Some(last) if last.style == style => last.content.to_mut().push_str(g),
            _ => spans.push(Span::styled(g.to_string(), style)),
        }
    }
    spans
}

/// The text of `line`, without styles.
pub fn plain(line: &Line<'_>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}
