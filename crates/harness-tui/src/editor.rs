//! The input editor: multi-line text with a cursor, recall of earlier inputs, and large pastes
//! collapsed to a placeholder whose full text is sent with the message.

use ratatui::{
    crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    layout::Position,
    text::{Line, Span},
};
use unicode_width::UnicodeWidthChar;

use crate::{style::Theme, text::sanitize};

/// A paste with more lines than this is collapsed.
pub const PASTE_MAX_LINES: usize = 10;
/// A paste with more characters than this is collapsed.
pub const PASTE_MAX_CHARS: usize = 1_000;

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
    /// A byte offset into `text`, always at a character boundary and never inside a placeholder.
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
    /// An empty editor that recalls `history` (oldest first) with the Up key.
    pub fn new(history: Vec<String>) -> Editor {
        Editor {
            history,
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

    /// Empties the editor.
    pub fn clear(&mut self) {
        self.set_text("");
    }

    /// Takes the input to send: the text as shown, and in full. It is added to the history.
    pub fn submit(&mut self) -> (String, String) {
        let shown = self.text.clone();
        let full = self.expanded();
        if !full.trim().is_empty() && self.history.last() != Some(&full) {
            self.history.push(full.clone());
        }
        self.clear();
        (shown, full)
    }

    /// Adds `text` to the history without sending it (input sent some other way).
    pub fn remember(&mut self, text: &str) {
        if !text.trim().is_empty() && self.history.last().map(String::as_str) != Some(text) {
            self.history.push(text.to_string());
        }
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

    /// Types `text` at the cursor.
    pub fn insert(&mut self, text: &str) {
        let at = self.cursor;
        self.splice(at, at, text);
        self.cursor = at + text.len();
    }

    /// Pastes `text`: collapsed to `[Pasted text #n, N lines]` when it has more than
    /// [`PASTE_MAX_LINES`] lines or [`PASTE_MAX_CHARS`] characters, typed in as it is otherwise.
    pub fn paste(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        let lines = text.lines().count();
        if lines <= PASTE_MAX_LINES && text.chars().count() <= PASTE_MAX_CHARS {
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

    fn previous_boundary(&self, offset: usize) -> usize {
        if let Some(i) = self.paste_ending_at(offset) {
            return self.pastes[i].start;
        }
        self.text[..offset]
            .char_indices()
            .next_back()
            .map_or(0, |(i, _)| i)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        if let Some(i) = self.paste_starting_at(offset) {
            return self.pastes[i].end;
        }
        self.text[offset..]
            .chars()
            .next()
            .map_or(offset, |c| offset + c.len_utf8())
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
        self.cursor = start;
    }

    /// Moves the cursor a row up (`-1`) or down (`1`), keeping its column; `false` at the first
    /// or last row.
    fn move_row(&mut self, direction: isize) -> bool {
        let start = self.line_start(self.cursor);
        let column = self.text[start..self.cursor].chars().count();
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
        let offset = self.text[target..end]
            .char_indices()
            .nth(column)
            .map_or(end, |(i, _)| target + i);
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
        self.paste(&text);
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
    /// indented as far, and where the cursor is in them. Long lines wrap at the width.
    pub fn render(
        &self,
        prompt: &str,
        width: usize,
        theme: &Theme,
    ) -> (Vec<Line<'static>>, Position) {
        let prompt_width = crate::text::width(prompt);
        let indent = " ".repeat(prompt_width);
        let width = width.max(prompt_width + 2);
        // Each row: its prefix, then its text.
        let mut rows: Vec<(Span<'static>, Vec<Span<'static>>)> =
            vec![(Span::styled(prompt.to_string(), theme.accent()), Vec::new())];
        let mut column = prompt_width;
        let mut cursor = None;
        let mut offset = 0;
        let new_row = |rows: &mut Vec<(Span<'static>, Vec<Span<'static>>)>| {
            rows.push((Span::raw(indent.clone()), Vec::new()));
        };
        loop {
            if offset == self.cursor && cursor.is_none() {
                if column >= width {
                    new_row(&mut rows);
                    column = prompt_width;
                }
                cursor = Some(Position::new(column as u16, (rows.len() - 1) as u16));
            }
            if offset >= self.text.len() {
                break;
            }
            if let Some(paste) = self.pastes.iter().find(|p| p.start == offset) {
                let label = self.text[paste.start..paste.end].to_string();
                let w = crate::text::width(&label);
                if column + w > width && column > prompt_width {
                    new_row(&mut rows);
                    column = prompt_width;
                }
                if let Some((_, content)) = rows.last_mut() {
                    content.push(Span::styled(label, theme.dim()));
                }
                column += w;
                offset = paste.end;
                continue;
            }
            let c = self.text[offset..].chars().next().unwrap_or(' ');
            offset += c.len_utf8();
            if c == '\n' {
                new_row(&mut rows);
                column = prompt_width;
                continue;
            }
            let shown = sanitize(c.encode_utf8(&mut [0; 4]));
            let w: usize = shown.chars().map(|c| c.width().unwrap_or(0)).sum();
            if column + w > width {
                new_row(&mut rows);
                column = prompt_width;
            }
            if let Some((_, content)) = rows.last_mut() {
                match content.last_mut() {
                    Some(last) if last.style == theme.plain() => {
                        last.content.to_mut().push_str(&shown)
                    }
                    _ => content.push(Span::styled(shown, theme.plain())),
                }
            }
            column += w;
        }
        let lines = rows
            .into_iter()
            .map(|(prefix, mut content)| {
                content.insert(0, prefix);
                Line::from(content)
            })
            .collect();
        (lines, cursor.unwrap_or_default())
    }
}
