//! A list to choose from, drawn in a full-screen view: the models, the sessions, the messages
//! to rewind to, the modes; on its own, before a session starts, the first-run model choice. Typing filters it, fuzzily, as `@` completion matches files; the
//! arrow keys, Page Up, Page Down, Home and End move; Enter chooses; Esc closes it.

use std::{cell::Cell, io, time::Instant};

use futures::{Stream, StreamExt};
use nucleo_matcher::{
    Config, Matcher, Utf32Str,
    pattern::{CaseMatching, Normalization, Pattern},
};
use ratatui::{
    backend::Backend,
    buffer::Buffer,
    crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    layout::{Position, Rect},
    text::{Line, Span},
};

use crate::{
    approval::Arming,
    inline::{CursorReport, InlineTerminal},
    input::Timed,
    style::Theme,
    text::{sanitize, width as text_width, wrap},
};

/// Rows Page Up and Page Down move before the list was first drawn.
const PAGE: usize = 10;

/// One thing to choose: what it is, and a dim detail beside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub label: String,
    pub detail: String,
}

impl Item {
    pub fn new(label: &str, detail: &str) -> Item {
        Item {
            label: label.to_string(),
            detail: detail.to_string(),
        }
    }
}

/// The item for the model `id` in a list of models: marked when it is the `current` one, and
/// when it is one a ChatGPT plan includes (a `chatgpt/` model).
pub fn model_item(id: &str, current: bool) -> Item {
    let marks: Vec<&str> = [
        current.then_some("(current)"),
        id.starts_with("chatgpt/").then_some("ChatGPT plan"),
    ]
    .into_iter()
    .flatten()
    .collect();
    Item::new(id, &marks.join(" "))
}

/// What a key did to the picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Picked {
    /// The item at this index of the picker's items was chosen.
    Chosen(usize),
    /// The picker was closed without a choice.
    Cancelled,
}

pub struct Picker {
    title: String,
    /// `None` while they are being looked for.
    items: Option<Vec<Item>>,
    /// Shown while the items are looked for.
    waiting: String,
    /// Shown when there is nothing to choose from.
    empty: String,
    /// Dim lines under the list, such as what a choice cannot undo.
    footer: Vec<String>,
    filter: String,
    /// The items the filter keeps, as indices into `items`, best first.
    shown: Vec<usize>,
    /// The chosen position in `shown`.
    selected: usize,
    /// The item to start at, once the items are there.
    preferred: Option<usize>,
    /// The first position of `shown` on screen, and how many rows the list had, as last drawn.
    top: Cell<usize>,
    rows: Cell<usize>,
    matcher: Matcher,
}

impl Picker {
    pub fn new(title: &str, items: Vec<Item>) -> Picker {
        let mut picker = Picker::loading(title, "");
        picker.set_items(items);
        picker
    }

    /// A picker whose items are still being looked for: it says `waiting` until
    /// [`set_items`](Self::set_items).
    pub fn loading(title: &str, waiting: &str) -> Picker {
        Picker {
            title: title.to_string(),
            items: None,
            waiting: waiting.to_string(),
            empty: "nothing to choose from".into(),
            footer: Vec::new(),
            filter: String::new(),
            shown: Vec::new(),
            selected: 0,
            preferred: None,
            top: Cell::new(0),
            rows: Cell::new(PAGE),
            matcher: Matcher::new(Config::DEFAULT),
        }
    }

    /// Lines shown under the list.
    pub fn with_footer(mut self, footer: Vec<String>) -> Self {
        self.footer = footer;
        self
    }

    /// What the picker says when there is nothing to choose from.
    pub fn with_empty(mut self, empty: &str) -> Self {
        self.empty = empty.to_string();
        self
    }

    /// Starts at the item at `index` (the current model, say).
    pub fn with_selected(mut self, index: usize) -> Self {
        self.select(index);
        self
    }

    /// Selects the item at `index`, and starts there again when the filter is cleared.
    pub fn select(&mut self, index: usize) {
        self.preferred = Some(index);
        self.refilter();
    }

    /// The items, once found.
    pub fn set_items(&mut self, items: Vec<Item>) {
        self.items = Some(items);
        self.refilter();
    }

    pub fn items(&self) -> &[Item] {
        self.items.as_deref().unwrap_or_default()
    }

    /// Whether the items are still being looked for.
    pub fn is_loading(&self) -> bool {
        self.items.is_none()
    }

    /// The index of the selected item, if any.
    pub fn selected(&self) -> Option<usize> {
        self.shown.get(self.selected).copied()
    }

    /// Keeps the items that match the filter, the best matches first, and selects the first,
    /// or, without a filter, the preferred item.
    fn refilter(&mut self) {
        let items = self.items.as_deref().unwrap_or_default();
        self.shown = if self.filter.is_empty() {
            (0..items.len()).collect()
        } else {
            let pattern = Pattern::parse(&self.filter, CaseMatching::Smart, Normalization::Smart);
            let mut buf = Vec::new();
            let mut scored: Vec<(usize, u32)> = items
                .iter()
                .enumerate()
                .filter_map(|(i, item)| {
                    let haystack = format!("{} {}", item.label, item.detail);
                    pattern
                        .score(Utf32Str::new(&haystack, &mut buf), &mut self.matcher)
                        .map(|score| (i, score))
                })
                .collect();
            scored.sort_by(|(a, x), (b, y)| y.cmp(x).then(a.cmp(b)));
            scored.into_iter().map(|(i, _)| i).collect()
        };
        self.selected = match self.preferred {
            Some(preferred) if self.filter.is_empty() => {
                self.shown.iter().position(|&i| i == preferred).unwrap_or(0)
            }
            _ => 0,
        };
        self.top.set(0);
    }

    /// Handles a key.
    pub fn key(&mut self, key: KeyEvent) -> Option<Picked> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let last = self.shown.len().saturating_sub(1);
        let page = self.rows.get().max(1);
        match key.code {
            KeyCode::Esc => return Some(Picked::Cancelled),
            KeyCode::Enter => return self.selected().map(Picked::Chosen),
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Char('p') if ctrl => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => self.selected = (self.selected + 1).min(last),
            KeyCode::Char('n') if ctrl => self.selected = (self.selected + 1).min(last),
            KeyCode::PageUp => self.selected = self.selected.saturating_sub(page),
            KeyCode::PageDown => self.selected = (self.selected + page).min(last),
            KeyCode::Home => self.selected = 0,
            KeyCode::End => self.selected = last,
            KeyCode::Char('u') if ctrl => {
                self.filter.clear();
                self.refilter();
            }
            KeyCode::Backspace => {
                self.filter.pop();
                self.refilter();
            }
            KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                self.filter.push(c);
                self.refilter();
            }
            _ => {}
        }
        None
    }

    /// Draws the picker over `area`, and returns where the cursor goes: after the filter.
    pub fn render(&self, area: Rect, buf: &mut Buffer, theme: &Theme) -> Option<Position> {
        let width = area.width as usize;
        let mut lines: Vec<Line<'static>> = Vec::new();
        lines.extend(wrap(
            &Line::from(Span::styled(sanitize(&self.title), theme.bold())),
            width,
            &[],
            &[],
        ));
        let filter_row = lines.len();
        let filter = sanitize(&self.filter).replace('\n', " ");
        lines.push(if filter.is_empty() {
            Line::from(vec![
                Span::styled("› ", theme.accent()),
                Span::styled("type to filter", theme.dim()),
            ])
        } else {
            Line::from(vec![
                Span::styled("› ", theme.accent()),
                Span::raw(filter.clone()),
            ])
        });
        lines.push(Line::default());
        let mut below: Vec<Line<'static>> = Vec::new();
        if !self.footer.is_empty() {
            below.push(Line::default());
            for text in &self.footer {
                below.extend(wrap(
                    &Line::from(Span::styled(sanitize(text), theme.dim())),
                    width,
                    &[],
                    &[],
                ));
            }
        }
        below.push(Line::from(Span::styled(
            "↑↓ move · Enter choose · Esc close",
            theme.dim(),
        )));
        let rows = (area.height as usize)
            .saturating_sub(lines.len() + below.len())
            .max(1);
        self.rows.set(rows);
        lines.extend(self.list(rows, width, theme));
        let fill = (area.height as usize).saturating_sub(lines.len() + below.len());
        lines.extend(std::iter::repeat_n(Line::default(), fill));
        lines.extend(below);
        for (i, line) in lines.iter().take(area.height as usize).enumerate() {
            buf.set_line(area.x, area.y + i as u16, line, area.width);
        }
        let x = (2 + text_width(&filter)).min(width.saturating_sub(1));
        (filter_row < area.height as usize)
            .then(|| Position::new(area.x + x as u16, area.y + filter_row as u16))
    }

    /// The list's lines, at most `rows`, scrolled so that the selected item shows.
    fn list(&self, rows: usize, width: usize, theme: &Theme) -> Vec<Line<'static>> {
        let Some(items) = &self.items else {
            return vec![Line::from(Span::styled(
                sanitize(&self.waiting),
                theme.dim(),
            ))];
        };
        if self.shown.is_empty() {
            let text = if self.filter.is_empty() {
                &self.empty
            } else {
                "nothing matches"
            };
            return vec![Line::from(Span::styled(sanitize(text), theme.dim()))];
        }
        let mut top = self.top.get();
        if self.selected < top {
            top = self.selected;
        } else if self.selected >= top + rows {
            top = self.selected + 1 - rows;
        }
        self.top.set(top);
        let label_width = self
            .shown
            .iter()
            .map(|&i| text_width(&sanitize(&items[i].label)))
            .max()
            .unwrap_or(0)
            .min(width / 2);
        self.shown
            .iter()
            .enumerate()
            .skip(top)
            .take(rows)
            .map(|(position, &i)| {
                let item = &items[i];
                let style = if position == self.selected {
                    theme.selected()
                } else {
                    theme.plain()
                };
                let label: String = sanitize(&item.label).replace('\n', " ");
                let pad = label_width.saturating_sub(text_width(&label));
                let mut spans = vec![Span::styled(format!("  {label}{}", " ".repeat(pad)), style)];
                if !item.detail.is_empty() {
                    let room = width.saturating_sub(label_width + 4);
                    let detail: String = sanitize(&item.detail)
                        .replace('\n', " ")
                        .chars()
                        .take(room)
                        .collect();
                    spans.push(Span::styled(format!("  {detail}"), theme.dim()));
                }
                Line::from(spans)
            })
            .collect()
    }
}

/// Runs `picker` on its own on `term`, in a full-screen view, with the terminal's `events`, as
/// the first-run model choice does before the session starts: the index of the item chosen, or
/// `None` for Esc, Ctrl+C or the end of the events (the terminal hung up). The inline screen is
/// given back either way.
///
/// The picker takes keys as an approval does: once the user has paused for
/// [`ARMING_DELAY`](crate::approval::ARMING_DELAY), counted from when each key was read, so keys
/// typed ahead never choose an item (they are dropped: nothing here takes typed input). Ctrl+C
/// closes it at any time. The terminal is not read from here, nor asked where its cursor is
/// after a resize: `events` is its reader's.
pub async fn choose<B, S, E>(
    term: &mut InlineTerminal<B>,
    mut events: S,
    mut picker: Picker,
    theme: &Theme,
) -> io::Result<Option<usize>>
where
    B: Backend,
    B::Error: Send + Sync + 'static,
    S: Stream<Item = io::Result<E>> + Unpin,
    E: Into<Timed>,
{
    let mut arming = Arming::default();
    let chosen = loop {
        term.draw_full(|area, buf| picker.render(area, buf, theme))?;
        arming.drawn(Instant::now());
        let Some(next) = events.next().await else {
            break None;
        };
        let Timed { event, at } = match next {
            Ok(event) => event.into(),
            Err(e) => {
                term.leave_full()?;
                return Err(e);
            }
        };
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                    break None;
                }
                if !arming.armed(at) {
                    arming.typed(at);
                    continue;
                }
                match picker.key(key) {
                    Some(Picked::Chosen(index)) => break Some(index),
                    Some(Picked::Cancelled) => break None,
                    None => {}
                }
            }
            Event::Resize(..) => term.resized_to(CursorReport::Unasked)?,
            _ => {}
        }
    };
    term.leave_full()?;
    Ok(chosen)
}
