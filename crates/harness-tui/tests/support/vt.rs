//! A small terminal for tests: it runs the bytes ratatui's real `CrosstermBackend` writes the way
//! xterm does (the cursor advances by each character's width, a line feed on the last row scrolls
//! the top row into scrollback, a shorter window pushes the rows above the cursor into
//! scrollback), so a test sees what a user would, where `TestBackend` only places cells by
//! coordinate.

#![allow(dead_code)]

use std::{
    io::{self, Write},
    sync::{Arc, Mutex, MutexGuard},
};

use ratatui::{
    backend::{Backend, ClearType, CrosstermBackend, WindowSize},
    buffer::Cell,
    layout::{Position, Size},
};
use unicode_width::UnicodeWidthChar;

/// The screen, the scrollback and the cursor of an xterm-like terminal.
pub struct Vt {
    cols: u16,
    rows: u16,
    /// Each cell's text; empty for the column hidden under a wide character.
    grid: Vec<Vec<String>>,
    scrollback: Vec<Vec<String>>,
    x: u16,
    y: u16,
    /// The last column was written: the next character goes on the next row.
    pending_wrap: bool,
    state: State,
    params: String,
    /// The start of a character split between two writes.
    partial: Vec<u8>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Text,
    Escape,
    Csi,
    Osc,
    OscEscape,
}

impl Vt {
    pub fn new(cols: u16, rows: u16) -> Vt {
        Vt {
            cols,
            rows,
            grid: vec![blank_row(cols); rows as usize],
            scrollback: Vec::new(),
            x: 0,
            y: 0,
            pending_wrap: false,
            state: State::Text,
            params: String::new(),
            partial: Vec::new(),
        }
    }

    pub fn size(&self) -> Size {
        Size::new(self.cols, self.rows)
    }

    pub fn cursor(&self) -> Position {
        Position::new(self.x, self.y)
    }

    /// Prints `text` as a shell would before harness starts: `\n` goes to the start of the next
    /// row.
    pub fn print(&mut self, text: &str) {
        self.feed(text.replace('\n', "\r\n").as_bytes());
    }

    /// Runs `bytes`, written by the program.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.partial.extend_from_slice(bytes);
        let data = std::mem::take(&mut self.partial);
        let valid = match std::str::from_utf8(&data) {
            Ok(text) => text.len(),
            Err(e) if e.error_len().is_none() => e.valid_up_to(),
            Err(e) => panic!("the program wrote invalid UTF-8: {e}"),
        };
        let text = std::str::from_utf8(&data[..valid]).unwrap().to_string();
        self.partial = data[valid..].to_vec();
        for c in text.chars() {
            self.char(c);
        }
    }

    fn char(&mut self, c: char) {
        match self.state {
            State::Text => match c {
                '\x1b' => self.state = State::Escape,
                '\n' => self.line_feed(),
                '\r' => {
                    self.x = 0;
                    self.pending_wrap = false;
                }
                '\x08' => {
                    self.x = self.x.saturating_sub(1);
                    self.pending_wrap = false;
                }
                '\x07' => {}
                c if c.is_control() => panic!("the program wrote the control character {c:?}"),
                c => self.print_char(c),
            },
            State::Escape => {
                self.state = match c {
                    '[' => {
                        self.params.clear();
                        State::Csi
                    }
                    ']' => State::Osc,
                    _ => State::Text,
                }
            }
            State::Csi => {
                if ('\x40'..='\x7e').contains(&c) {
                    let params = std::mem::take(&mut self.params);
                    self.csi(&params, c);
                    self.state = State::Text;
                } else {
                    self.params.push(c);
                }
            }
            State::Osc => match c {
                '\x07' => self.state = State::Text,
                '\x1b' => self.state = State::OscEscape,
                _ => {}
            },
            State::OscEscape => self.state = State::Text,
        }
    }

    fn csi(&mut self, params: &str, command: char) {
        if params.starts_with(['?', '<', '>', '=']) {
            return;
        }
        let numbers: Vec<u16> = params.split(';').map(|p| p.parse().unwrap_or(0)).collect();
        let n = |i: usize| numbers.get(i).copied().filter(|n| *n > 0).unwrap_or(1);
        self.pending_wrap = false;
        match command {
            'H' | 'f' => {
                self.y = n(0).min(self.rows) - 1;
                self.x = n(1).min(self.cols) - 1;
            }
            'A' => self.y = self.y.saturating_sub(n(0)),
            'B' => self.y = (self.y + n(0)).min(self.rows - 1),
            'C' => self.x = (self.x + n(0)).min(self.cols - 1),
            'D' => self.x = self.x.saturating_sub(n(0)),
            'G' => self.x = n(0).min(self.cols) - 1,
            'J' => match numbers.first().copied().unwrap_or(0) {
                0 => {
                    let (x, y) = (self.x as usize, self.y as usize);
                    self.clear_cells(y, x);
                    for row in &mut self.grid[y + 1..] {
                        *row = blank_row(self.cols);
                    }
                }
                2 => self.grid = vec![blank_row(self.cols); self.rows as usize],
                _ => {}
            },
            'K' => {
                let (x, y) = (self.x as usize, self.y as usize);
                self.clear_cells(y, x);
            }
            _ => {}
        }
    }

    /// Clears row `y` from column `x` on.
    fn clear_cells(&mut self, y: usize, x: usize) {
        if x > 0 && self.grid[y][x].is_empty() {
            self.grid[y][x - 1] = " ".into();
        }
        for cell in &mut self.grid[y][x..] {
            *cell = " ".into();
        }
    }

    fn line_feed(&mut self) {
        self.pending_wrap = false;
        if self.y + 1 == self.rows {
            let top = self.grid.remove(0);
            self.scrollback.push(top);
            self.grid.push(blank_row(self.cols));
        } else {
            self.y += 1;
        }
    }

    fn print_char(&mut self, c: char) {
        let width = c.width().unwrap_or(0) as u16;
        if width == 0 {
            // A combining character joins the one before it.
            let x = if self.pending_wrap {
                self.x
            } else {
                self.x.saturating_sub(1)
            } as usize;
            let y = self.y as usize;
            let x = if x > 0 && self.grid[y][x].is_empty() {
                x - 1
            } else {
                x
            };
            self.grid[y][x].push(c);
            return;
        }
        if self.pending_wrap || (width == 2 && self.x + 1 >= self.cols) {
            self.x = 0;
            self.line_feed();
        }
        let (x, y) = (self.x as usize, self.y as usize);
        // Writing over half of a wide character blanks its other half.
        if self.grid[y][x].is_empty() && x > 0 {
            self.grid[y][x - 1] = " ".into();
        }
        let end = x + width as usize;
        if end < self.cols as usize && self.grid[y][end].is_empty() {
            self.grid[y][end] = " ".into();
        }
        self.grid[y][x] = c.to_string();
        if width == 2 {
            self.grid[y][x + 1] = String::new();
        }
        if end >= self.cols as usize {
            self.x = self.cols - 1;
            self.pending_wrap = true;
        } else {
            self.x = end as u16;
        }
    }

    /// The window changes size as xterm's does: rows are cut or padded on the right; a shorter
    /// window pushes the rows above the cursor into scrollback as far as it must to keep the
    /// cursor on screen, and drops the rows below it; a taller one adds blank rows at the bottom.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        for row in self.grid.iter_mut().chain(self.scrollback.iter_mut()) {
            row.resize(cols as usize, " ".into());
            if row.last().is_some_and(String::is_empty) {
                *row.last_mut().unwrap() = " ".into();
            }
        }
        self.cols = cols;
        self.x = self.x.min(cols - 1);
        if rows < self.rows {
            let over = (self.y + 1).saturating_sub(rows);
            for _ in 0..over {
                let top = self.grid.remove(0);
                self.scrollback.push(top);
            }
            self.y -= over;
            self.grid.truncate(rows as usize);
        } else {
            self.grid.resize(rows as usize, blank_row(cols));
        }
        self.rows = rows;
        self.pending_wrap = false;
    }

    /// The rows on screen, without trailing spaces.
    pub fn screen(&self) -> Vec<String> {
        self.grid.iter().map(|row| text(row)).collect()
    }

    /// The rows that scrolled off the top, oldest first.
    pub fn scrollback(&self) -> Vec<String> {
        self.scrollback.iter().map(|row| text(row)).collect()
    }

    /// The scrollback, then the screen.
    pub fn everything(&self) -> Vec<String> {
        let mut rows = self.scrollback();
        rows.extend(self.screen());
        rows
    }

    /// The columns each row's text takes, as the terminal shows it: the scrollback, then the
    /// screen.
    pub fn widths(&self) -> Vec<usize> {
        self.scrollback
            .iter()
            .chain(self.grid.iter())
            .map(|row| {
                row.iter()
                    .rposition(|cell| cell != " ")
                    .map_or(0, |i| i + 1)
            })
            .collect()
    }
}

fn blank_row(cols: u16) -> Vec<String> {
    vec![" ".to_string(); cols as usize]
}

fn text(row: &[String]) -> String {
    row.concat().trim_end().to_string()
}

/// Writes into a [`Vt`].
#[derive(Clone)]
pub struct VtWriter(pub Arc<Mutex<Vt>>);

impl Write for VtWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().feed(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// ratatui's `CrosstermBackend`, writing into a [`Vt`], which answers for the terminal's size and
/// where its cursor is.
pub struct VtBackend {
    vt: Arc<Mutex<Vt>>,
    inner: CrosstermBackend<VtWriter>,
}

impl VtBackend {
    pub fn new(vt: Vt) -> VtBackend {
        let vt = Arc::new(Mutex::new(vt));
        VtBackend {
            inner: CrosstermBackend::new(VtWriter(vt.clone())),
            vt,
        }
    }

    pub fn vt(&self) -> MutexGuard<'_, Vt> {
        self.vt.lock().unwrap()
    }

    /// The terminal, shared, for a test to look at after the backend has moved into the UI.
    pub fn shared(&self) -> Arc<Mutex<Vt>> {
        self.vt.clone()
    }
}

impl Backend for VtBackend {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        self.inner.draw(content)
    }
    fn append_lines(&mut self, n: u16) -> io::Result<()> {
        self.inner.append_lines(n)
    }
    fn hide_cursor(&mut self) -> io::Result<()> {
        self.inner.hide_cursor()
    }
    fn show_cursor(&mut self) -> io::Result<()> {
        self.inner.show_cursor()
    }
    fn get_cursor_position(&mut self) -> io::Result<Position> {
        Ok(self.vt().cursor())
    }
    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        self.inner.set_cursor_position(position)
    }
    fn clear(&mut self) -> io::Result<()> {
        self.inner.clear()
    }
    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        self.inner.clear_region(clear_type)
    }
    fn size(&self) -> io::Result<Size> {
        Ok(self.vt().size())
    }
    fn window_size(&mut self) -> io::Result<WindowSize> {
        Ok(WindowSize {
            columns_rows: self.vt().size(),
            pixels: Size::default(),
        })
    }
    fn flush(&mut self) -> io::Result<()> {
        Backend::flush(&mut self.inner)
    }
}
