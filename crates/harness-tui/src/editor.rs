//! The input editor: multi-line text with a cursor, recall of earlier inputs, and large pastes
//! collapsed to a placeholder whose full text is sent with the message.

use ratatui::{
    crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    layout::Position,
    text::{Line, Span},
};
use unicode_segmentation::{GraphemeCursor, UnicodeSegmentation};

use crate::{
    style::Theme,
    text::{grapheme_width, sanitize, width as text_width},
};

/// A paste with more lines than this is collapsed.
pub const PASTE_MAX_LINES: usize = 10;
/// A paste with more characters than this is collapsed.
pub const PASTE_MAX_CHARS: usize = 1_000;
/// A paste larger than this (about a million tokens, past any model's window) is refused.
pub const PASTE_MAX_BYTES: usize = 4 * 1024 * 1024;
/// Earlier inputs kept for Up, at most: the latest.
pub const HISTORY_MAX: usize = 1_000;
/// Bytes of earlier inputs kept for Up, at most.
const HISTORY_MAX_BYTES: usize = 16 * 1024 * 1024;

/// A collapsed paste.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Paste {
    /// Where its placeholder is in the text, in bytes.
    start: usize,
    end: usize,
    text: String,
}

/// What a key did to the editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edit {
    /// The key changed the text or the cursor.
    Handled,
    /// Enter: the input is ready to send.
    Submit,
    /// Not an editing key.
    Ignored,
}

/// The text being typed.
#[derive(Debug, Clone, Default)]
pub struct Editor {
    text: String,
    /// A byte offset into `text`, always between grapheme clusters and never inside a
    /// placeholder.
    cursor: usize,
    pastes: Vec<Paste>,
    /// Pastes collapsed so far, for their numbers.
    pasted: usize,
    /// Earlier inputs, oldest first.
    history: Vec<String>,
    /// While recalling: the entry shown, and the text that was being typed before.
    recall: Option<(usize, String)>,
}

impl Editor {
    /// An empty editor that recalls `history` (oldest first) with the Up key: its latest
    /// [`HISTORY_MAX`] entries, as far as they fit in 16 MiB, leaving out any too large to paste.
    pub fn new(history: Vec<String>) -> Editor {
        let mut kept = Vec::new();
        let mut bytes = 0;
        for entry in history.into_iter().rev() {
            if entry.len() > PASTE_MAX_BYTES {
                continue;
            }
            bytes += entry.len();
            if kept.len() == HISTORY_MAX || bytes > HISTORY_MAX_BYTES {
                break;
            }
            kept.push(entry);
        }
        kept.reverse();
        Editor {
            history: kept,
            ..Editor::default()
        }
    }

    /// The text as shown, placeholders included.
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// The cursor, as a byte offset into [`text`](Self::text).
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// The text with every placeholder replaced by what was pasted.
    pub fn expanded(&self) -> String {
        let mut out = String::new();
        let mut at = 0;
        for paste in &self.pastes {
            out.push_str(&self.text[at..paste.start]);
            out.push_str(&paste.text);
            at = paste.end;
        }
        out.push_str(&self.text[at..]);
        out
    }

    /// Replaces the text, with the cursor at its end.
    pub fn set_text(&mut self, text: &str) {
        self.text = text.to_string();
        self.cursor = self.text.len();
        self.pastes.clear();
        self.recall = None;
    }

    /// Replaces the bytes `range` of the text (a word being completed) with `with` and a space,
    /// dropping spaces that followed the word, and puts the cursor after the space. Collapsed
    /// pastes elsewhere in the text stay collapsed.
    pub fn replace_word(&mut self, range: std::ops::Range<usize>, with: &str) {
        let end = range.end.min(self.text.len());
        let spaces = self.text[end..].len() - self.text[end..].trim_start_matches(' ').len();
        let start = range.start.min(end);
        self.splice(start, end + spaces, &format!("{with} "));
        self.cursor = start + with.len() + 1;
    }

    /// Empties the editor.
    pub fn clear(&mut self) {
        self.set_text("");
    }

    /// Takes the input to send: the text as shown, and in full. It is added to the history.
    pub fn submit(&mut self) -> (String, String) {
        let shown = self.text.clone();
        let full = self.expanded();
        self.remember(&full);
        self.clear();
        (shown, full)
    }

    /// Adds `text` to the history without sending it (input sent some other way). As for the
    /// history the editor starts with, the latest [`HISTORY_MAX`] entries are kept, as far as
    /// they fit in 16 MiB, and one too large to paste is left out.
    pub fn remember(&mut self, text: &str) {
        if text.trim().is_empty()
            || text.len() > PASTE_MAX_BYTES
            || self.history.last().map(String::as_str) == Some(text)
        {
            return;
        }
        self.history.push(text.to_string());
        let mut dropped = self.history.len().saturating_sub(HISTORY_MAX);
        let mut bytes: usize = self.history[dropped..].iter().map(String::len).sum();
        // The latest entry fits on its own.
        while bytes > HISTORY_MAX_BYTES {
            bytes -= self.history[dropped].len();
            dropped += 1;
        }
        self.history.drain(..dropped);
    }

    /// The paste whose placeholder contains `offset` strictly inside, or ends at it.
    fn paste_ending_at(&self, offset: usize) -> Option<usize> {
        self.pastes
            .iter()
            .position(|p| p.start < offset && offset <= p.end)
    }

    fn paste_starting_at(&self, offset: usize) -> Option<usize> {
        self.pastes
            .iter()
            .position(|p| p.start <= offset && offset < p.end)
    }

    /// Replaces `start..end` of the text with `with`, moving the pastes after it.
    fn splice(&mut self, start: usize, end: usize, with: &str) {
        self.text.replace_range(start..end, with);
        let delta = with.len() as isize - (end - start) as isize;
        self.pastes.retain(|p| p.end <= start || p.start >= end);
        for paste in &mut self.pastes {
            if paste.start >= end {
                paste.start = (paste.start as isize + delta) as usize;
                paste.end = (paste.end as isize + delta) as usize;
            }
        }
        self.recall = None;
    }

    /// Types `text` at the cursor, which goes after it: after the grapheme cluster it ends in,
    /// when it joins the text after it into one (a ZWJ typed between two emoji).
    pub fn insert(&mut self, text: &str) {
        let at = self.cursor;
        self.splice(at, at, text);
        self.cursor = self.cluster_end(at + text.len());
    }

    /// Pastes `text`: collapsed to `[Pasted text #n, N lines]` when it has more than
    /// [`PASTE_MAX_LINES`] lines or [`PASTE_MAX_CHARS`] characters, typed in as it is otherwise.
    /// A paste larger than [`PASTE_MAX_BYTES`] is refused (`false`), and the input left as it was.
    pub fn paste(&mut self, text: &str) -> bool {
        if text.len() > PASTE_MAX_BYTES {
            return false;
        }
        self.put(text);
        true
    }

    /// Puts `text` in as a paste, whatever its size (an earlier input recalled). It is copied
    /// once, and counted in as few passes as can tell.
    fn put(&mut self, text: &str) {
        let text = if text.contains('\r') {
            text.replace("\r\n", "\n").replace('\r', "\n")
        } else {
            text.to_string()
        };
        let lines = text.lines().count();
        let short = text.len() <= PASTE_MAX_CHARS || text.chars().nth(PASTE_MAX_CHARS).is_none();
        if lines <= PASTE_MAX_LINES && short {
            self.insert(&text);
            return;
        }
        self.pasted += 1;
        let placeholder = format!(
            "[Pasted text #{}, {lines} line{}]",
            self.pasted,
            if lines == 1 { "" } else { "s" }
        );
        let at = self.cursor;
        self.splice(at, at, &placeholder);
        self.pastes.push(Paste {
            start: at,
            end: at + placeholder.len(),
            text,
        });
        self.pastes.sort_by_key(|p| p.start);
        self.cursor = at + placeholder.len();
    }

    /// Replaces the placeholder at the cursor (or the last one before it) with what was pasted,
    /// so it can be edited. Returns whether there was one.
    pub fn expand_paste(&mut self) -> bool {
        let index = self
            .paste_ending_at(self.cursor)
            .or_else(|| self.paste_starting_at(self.cursor))
            .or_else(|| self.pastes.iter().rposition(|p| p.end <= self.cursor));
        let Some(index) = index else {
            return false;
        };
        let paste = self.pastes.remove(index);
        let (start, end) = (paste.start, paste.end);
        self.splice(start, end, &paste.text);
        self.cursor = start + paste.text.len();
        true
    }

    /// Where the grapheme cluster (or placeholder) before `offset` starts: what Left moves over
    /// and Backspace deletes.
    fn previous_boundary(&self, offset: usize) -> usize {
        if let Some(i) = self.paste_ending_at(offset) {
            return self.pastes[i].start;
        }
        GraphemeCursor::new(offset, self.text.len(), true)
            .prev_boundary(&self.text, 0)
            .ok()
            .flatten()
            .unwrap_or(0)
    }

    /// Where the grapheme cluster (or placeholder) after `offset` ends: what Right moves over and
    /// Delete deletes.
    fn next_boundary(&self, offset: usize) -> usize {
        if let Some(i) = self.paste_starting_at(offset) {
            return self.pastes[i].end;
        }
        GraphemeCursor::new(offset, self.text.len(), true)
            .next_boundary(&self.text, 0)
            .ok()
            .flatten()
            .unwrap_or(self.text.len())
    }

    /// `offset`, or the end of the grapheme cluster it is inside.
    fn cluster_end(&self, offset: usize) -> usize {
        let boundary = GraphemeCursor::new(offset, self.text.len(), true)
            .is_boundary(&self.text, 0)
            .unwrap_or(true);
        if boundary {
            offset
        } else {
            self.next_boundary(offset)
        }
    }

    fn line_start(&self, offset: usize) -> usize {
        self.text[..offset].rfind('\n').map_or(0, |i| i + 1)
    }

    fn line_end(&self, offset: usize) -> usize {
        self.text[offset..]
            .find('\n')
            .map_or(self.text.len(), |i| offset + i)
    }

    /// Where the word before `offset` starts.
    fn word_start(&self, offset: usize) -> usize {
        let before = &self.text[..offset];
        let trimmed = before.trim_end_matches(char::is_whitespace);
        trimmed.rfind(char::is_whitespace).map_or(0, |i| {
            i + trimmed[i..].chars().next().map_or(1, char::len_utf8)
        })
    }

    /// Where the word after `offset` ends.
    fn word_end(&self, offset: usize) -> usize {
        let after = &self.text[offset..];
        let skipped = after.len() - after.trim_start_matches(char::is_whitespace).len();
        let rest = &after[skipped..];
        offset + skipped + rest.find(char::is_whitespace).unwrap_or(rest.len())
    }

    /// A cursor position never inside a placeholder.
    fn settle(&self, offset: usize) -> usize {
        match self.paste_starting_at(offset) {
            Some(i) if self.pastes[i].start != offset => self.pastes[i].end,
            _ => offset,
        }
    }

    fn delete(&mut self, start: usize, end: usize) {
        let start =
            self.pastes.iter().fold(
                start,
                |s, p| {
                    if p.start < s && s < p.end { p.start } else { s }
                },
            );
        let end = self
            .pastes
            .iter()
            .fold(end, |e, p| if p.start < e && e < p.end { p.end } else { e });
        self.splice(start, end, "");
        self.cursor = self.cluster_end(start);
    }

    /// Moves the cursor a row up (`-1`) or down (`1`), keeping its column on screen; `false` at
    /// the first or last row.
    fn move_row(&mut self, direction: isize) -> bool {
        let start = self.line_start(self.cursor);
        let column = text_width(&self.text[start..self.cursor]);
        let target = if direction < 0 {
            if start == 0 {
                return false;
            }
            self.line_start(start - 1)
        } else {
            let end = self.line_end(self.cursor);
            if end == self.text.len() {
                return false;
            }
            end + 1
        };
        let end = self.line_end(target);
        let mut offset = target;
        let mut used = 0;
        for (i, grapheme) in self.text[target..end].grapheme_indices(true) {
            used += grapheme_width(grapheme);
            if used > column {
                break;
            }
            offset = target + i + grapheme.len();
        }
        self.cursor = self.settle(offset);
        true
    }

    /// Shows the previous (`-1`) or next (`1`) history entry.
    fn recall(&mut self, direction: isize) -> bool {
        let (index, draft) = match self.recall.take() {
            Some(state) => state,
            None if direction < 0 => (self.history.len(), self.expanded()),
            None => return false,
        };
        let next = index as isize + direction;
        if next < 0 {
            self.recall = Some((index, draft));
            return false;
        }
        let next = next as usize;
        let text = if next >= self.history.len() {
            draft.clone()
        } else {
            self.history[next].clone()
        };
        self.set_text("");
        self.put(&text);
        if next < self.history.len() {
            self.recall = Some((next, draft));
        }
        true
    }

    /// Handles an editing key. Enter asks to submit, unless the character before the cursor is a
    /// backslash, which it turns into a new line.
    pub fn key(&mut self, key: KeyEvent) -> Edit {
        if key.kind == KeyEventKind::Release {
            return Edit::Ignored;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Enter if alt || shift => self.insert("\n"),
            KeyCode::Char('j') if ctrl => self.insert("\n"),
            KeyCode::Enter if self.text[..self.cursor].ends_with('\\') => {
                let at = self.cursor - 1;
                self.splice(at, at + 1, "\n");
            }
            KeyCode::Enter => return Edit::Submit,
            KeyCode::Char('a') if ctrl => self.cursor = self.line_start(self.cursor),
            KeyCode::Char('e') if ctrl => self.cursor = self.line_end(self.cursor),
            KeyCode::Char('b') if alt => self.cursor = self.settle(self.word_start(self.cursor)),
            KeyCode::Char('f') if alt => self.cursor = self.settle(self.word_end(self.cursor)),
            KeyCode::Char('w') if ctrl => {
                let start = self.word_start(self.cursor);
                self.delete(start, self.cursor);
            }
            KeyCode::Char('u') if ctrl => {
                let start = self.line_start(self.cursor);
                let start = if start == self.cursor && start > 0 {
                    start - 1
                } else {
                    start
                };
                self.delete(start, self.cursor);
            }
            KeyCode::Char('k') if ctrl => {
                let end = self.line_end(self.cursor);
                let end = if end == self.cursor && end < self.text.len() {
                    end + 1
                } else {
                    end
                };
                self.delete(self.cursor, end);
            }
            KeyCode::Char('o') if ctrl => {
                self.expand_paste();
            }
            KeyCode::Char(_) if ctrl => return Edit::Ignored,
            KeyCode::Char(c) => self.insert(c.encode_utf8(&mut [0; 4])),
            KeyCode::Backspace if alt => {
                let start = self.word_start(self.cursor);
                self.delete(start, self.cursor);
            }
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    let start = self.previous_boundary(self.cursor);
                    self.delete(start, self.cursor);
                }
            }
            KeyCode::Delete => {
                if self.cursor < self.text.len() {
                    let end = self.next_boundary(self.cursor);
                    self.delete(self.cursor, end);
                }
            }
            KeyCode::Left if alt || ctrl => self.cursor = self.settle(self.word_start(self.cursor)),
            KeyCode::Right if alt || ctrl => self.cursor = self.settle(self.word_end(self.cursor)),
            KeyCode::Left => self.cursor = self.previous_boundary(self.cursor),
            KeyCode::Right => self.cursor = self.next_boundary(self.cursor),
            KeyCode::Home => self.cursor = self.line_start(self.cursor),
            KeyCode::End => self.cursor = self.line_end(self.cursor),
            KeyCode::Up => {
                if !self.move_row(-1) && !self.recall(-1) {
                    return Edit::Ignored;
                }
            }
            KeyCode::Down => {
                if !self.move_row(1) && !self.recall(1) {
                    return Edit::Ignored;
                }
            }
            _ => return Edit::Ignored,
        }
        Edit::Handled
    }

    /// The editor as lines `width` columns wide, the first starting with `prompt` and the rest
    /// indented as far, and where the cursor is in them. Long lines wrap at the width, between
    /// grapheme clusters.
    pub fn render(
        &self,
        prompt: &str,
        width: usize,
        theme: &Theme,
    ) -> (Vec<Line<'static>>, Position) {
        let prompt_width = text_width(prompt);
        let indent = " ".repeat(prompt_width);
        let width = width.max(prompt_width + 2);
        let mut rows = Rows {
            rows: vec![(Span::styled(prompt.to_string(), theme.accent()), Vec::new())],
            indent,
            column: prompt_width,
            prompt_width,
            width,
            cursor: None,
        };
        let mut at = 0;
        let mut pastes = self.pastes.iter();
        loop {
            let paste = pastes.next();
            let end = paste.map_or(self.text.len(), |p| p.start);
            for (i, grapheme) in self.text[at..end].grapheme_indices(true) {
                rows.cursor_before(at + i, self.cursor);
                if grapheme == "\n" {
                    rows.new_row();
                    continue;
                }
                let shown = sanitize(grapheme);
                let w = text_width(&shown);
                if rows.column + w > width {
                    rows.new_row();
                }
                rows.push(&shown, w, theme.plain());
            }
            let Some(paste) = paste else {
                break;
            };
            rows.cursor_before(paste.start, self.cursor);
            let label = fit(&self.text[paste.start..paste.end], width - prompt_width);
            let w = text_width(&label);
            if rows.column + w > width && rows.column > prompt_width {
                rows.new_row();
            }
            rows.push(&label, w, theme.dim());
            at = paste.end;
        }
        rows.cursor_before(self.text.len(), self.cursor);
        let cursor = rows.cursor.unwrap_or_default();
        let lines = rows
            .rows
            .into_iter()
            .map(|(prefix, mut content)| {
                content.insert(0, prefix);
                Line::from(content)
            })
            .collect();
        (lines, cursor)
    }
}

/// `label`, cut to fit in `width` columns, with an ellipsis when it is cut.
fn fit(label: &str, width: usize) -> String {
    if text_width(label) <= width {
        return label.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for grapheme in label.graphemes(true) {
        used += grapheme_width(grapheme);
        if used + 1 > width {
            break;
        }
        out.push_str(grapheme);
    }
    out.push('…');
    out
}

/// The editor's rows as they are laid out: each row's prefix, then its text.
struct Rows {
    rows: Vec<(Span<'static>, Vec<Span<'static>>)>,
    indent: String,
    /// The column the next text goes in.
    column: usize,
    prompt_width: usize,
    width: usize,
    cursor: Option<Position>,
}

impl Rows {
    fn new_row(&mut self) {
        self.rows.push((Span::raw(self.indent.clone()), Vec::new()));
        self.column = self.prompt_width;
    }

    /// Places the cursor here, before the text at `offset`, if it is at or before `offset` and
    /// not placed yet. A cursor at the end of a full row goes to the start of the next.
    fn cursor_before(&mut self, offset: usize, cursor: usize) {
        if self.cursor.is_some() || cursor > offset {
            return;
        }
        if self.column >= self.width {
            self.new_row();
        }
        self.cursor = Some(Position::new(
            self.column as u16,
            (self.rows.len() - 1) as u16,
        ));
    }

    /// Adds `text`, `width` columns wide, to the last row.
    fn push(&mut self, text: &str, width: usize, style: ratatui::style::Style) {
        if let Some((_, content)) = self.rows.last_mut() {
            match content.last_mut() {
                Some(last) if last.style == style => last.content.to_mut().push_str(text),
                _ => content.push(Span::styled(text.to_string(), style)),
            }
        }
        self.column += width;
    }
}
