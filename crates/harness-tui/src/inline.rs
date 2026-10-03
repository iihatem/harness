//! The terminal, drawn inline: finished lines are written above a live region at the bottom of
//! what harness has drawn, and scroll off into the terminal's own scrollback, where the terminal's
//! scrolling, search and copy work as usual. Only the live region is redrawn, and only the cells
//! in it that changed. Unlike ratatui's inline viewport, the live region's height changes with
//! what it shows. A full-screen view (a picker) is drawn on the terminal's alternate screen, and
//! closing it gives the inline screen back as it was.

use std::io;

use ratatui::{
    backend::{Backend, ClearType},
    buffer::Buffer,
    layout::{Position, Rect, Size},
    text::Line,
    widgets::Widget,
};

/// How many times in a row a terminal may not say where its cursor is after a resize before it is
/// no longer asked: an answer can come late once in a while (over a slow link), while a terminal
/// that never answers would hold up every resize.
pub const CURSOR_MISSES: u8 = 3;

/// What the terminal said, after a resize, of where its cursor is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorReport {
    /// Where it is.
    At(Position),
    /// It did not say in time: after [`CURSOR_MISSES`] in a row, it is not asked again.
    Missed,
    /// It was not asked: keys it has not handed over yet wait ahead of where its answer would
    /// come. Nothing is held against it.
    Unasked,
}
use crate::terminal::AltScreen;

/// A terminal whose bottom rows, from `top` down, are a live region that is redrawn; everything
/// above it is written once.
pub struct InlineTerminal<B: Backend> {
    backend: B,
    screen: Size,
    /// The live region's first row.
    top: u16,
    height: u16,
    /// What the live region shows, so a redraw writes only what changed.
    shown: Buffer,
    /// The row harness left the terminal's cursor on.
    cursor_row: u16,
    /// The terminal answers when asked where its cursor is.
    reports_cursor: bool,
    /// How many times in a row it did not say by the time it was waited for.
    cursor_misses: u8,
    /// Where full-screen views are drawn.
    alt: Option<Box<dyn AltScreen<B> + Send>>,
    /// What the full-screen view shows, while one is open.
    full: Option<Buffer>,
    /// The screen changed size while a full-screen view was open.
    resized_in_full: bool,
}

fn io_error<E: std::error::Error + Send + Sync + 'static>(error: E) -> io::Error {
    io::Error::other(error)
}

impl<B> InlineTerminal<B>
where
    B: Backend,
    B::Error: Send + Sync + 'static,
{
    /// Starts drawing at row `top`, where the cursor was when harness started: the rows above it
    /// are the user's. A row past the bottom of the screen (the row after a cursor left mid-line
    /// on the bottom row, or `u16::MAX` when the cursor's place is not known) starts below the
    /// bottom row: the first draw scrolls the user's rows up rather than drawing over them.
    pub fn new(backend: B, top: u16) -> io::Result<Self> {
        let screen = backend.size().map_err(io_error)?;
        let top = top.min(screen.height);
        Ok(InlineTerminal {
            backend,
            screen,
            top,
            height: 0,
            shown: Buffer::empty(Rect::new(0, top, screen.width, 0)),
            cursor_row: top.min(screen.height.saturating_sub(1)),
            reports_cursor: true,
            cursor_misses: 0,
            alt: None,
            full: None,
            resized_in_full: false,
        })
    }

    /// For a terminal that did not say where its cursor was when harness started: it is not
    /// asked again after a resize.
    pub fn without_cursor_reports(mut self) -> Self {
        self.reports_cursor = false;
        self
    }

    /// Draws full-screen views on `alt`, the terminal's alternate screen.
    pub fn with_alt_screen(mut self, alt: Box<dyn AltScreen<B> + Send>) -> Self {
        self.alt = Some(alt);
        self
    }

    /// Whether a full-screen view is open.
    pub fn in_full_screen(&self) -> bool {
        self.full.is_some()
    }

    pub fn backend(&self) -> &B {
        &self.backend
    }

    pub fn backend_mut(&mut self) -> &mut B {
        &mut self.backend
    }

    /// The screen's width in columns.
    pub fn width(&self) -> u16 {
        self.screen.width
    }

    /// The screen's height in rows.
    pub fn height(&self) -> u16 {
        self.screen.height
    }

    /// The live region's first row.
    pub fn top(&self) -> u16 {
        self.top
    }

    /// Scrolls the whole screen up by `rows`, the top rows going into scrollback.
    fn scroll_up(&mut self, rows: u16) -> io::Result<()> {
        if rows == 0 {
            return Ok(());
        }
        let bottom = self.screen.height.saturating_sub(1);
        self.move_cursor(Position::new(0, bottom))?;
        self.backend.append_lines(rows).map_err(io_error)
    }

    /// Moves the terminal's cursor to `position`, and remembers its row.
    fn move_cursor(&mut self, position: Position) -> io::Result<()> {
        self.backend
            .set_cursor_position(position)
            .map_err(io_error)?;
        self.cursor_row = position.y;
        Ok(())
    }

    /// Clears from the live region's top to the bottom of the screen, and forgets what it showed.
    /// A live region below the bottom row (just after lines that reached it) has nothing on
    /// screen to clear.
    fn clear_live(&mut self) -> io::Result<()> {
        if self.top < self.screen.height {
            self.move_cursor(Position::new(0, self.top))?;
            self.backend
                .clear_region(ClearType::AfterCursor)
                .map_err(io_error)?;
        }
        self.shown = Buffer::empty(Rect::new(0, self.top, self.screen.width, self.height));
        Ok(())
    }

    /// Writes `lines`, each at most the screen's width, above the live region, which moves down
    /// (and, at the bottom of the screen, pushes the rows above into scrollback). The live region
    /// is cleared and left empty, just below the lines: the next draw makes the room it needs,
    /// so the end of the lines stays on screen even after a live region as tall as the screen.
    pub fn insert(&mut self, lines: &[Line<'_>]) -> io::Result<()> {
        if lines.is_empty() {
            return Ok(());
        }
        self.height = 0;
        self.clear_live()?;
        let width = self.screen.width;
        let mut y = self.top;
        for line in lines {
            if y >= self.screen.height {
                self.scroll_up(1)?;
                y = self.screen.height.saturating_sub(1);
            }
            let area = Rect::new(0, y, width, 1);
            let mut row = Buffer::empty(area);
            line.render(area, &mut row);
            // The row is blank on screen: write only what differs from blank, which also skips
            // the columns hidden under wide characters (writing those would push the rest of the
            // row right, past its end).
            let blank = Buffer::empty(area);
            self.backend
                .draw(blank.diff(&row).into_iter())
                .map_err(io_error)?;
            y += 1;
        }
        // The row after the last line, which is past the bottom of the screen when the lines
        // reached it.
        self.top = y.min(self.screen.height);
        self.shown = Buffer::empty(Rect::new(0, self.top, width, 0));
        self.move_cursor(Position::new(
            0,
            self.top.min(self.screen.height.saturating_sub(1)),
        ))?;
        self.backend.flush().map_err(io_error)
    }

    /// Moves the live region up far enough for `height` rows below its top.
    fn make_room(&mut self, height: u16) -> io::Result<()> {
        let height = height.min(self.screen.height);
        let over = (self.top + height).saturating_sub(self.screen.height);
        self.scroll_up(over)?;
        self.top -= over;
        Ok(())
    }

    /// Draws the live region, `height` rows tall, with `render`, which returns where the cursor
    /// goes (it is hidden for `None`). Only cells that changed since the last draw are written.
    pub fn draw(
        &mut self,
        height: u16,
        render: impl FnOnce(Rect, &mut Buffer) -> Option<Position>,
    ) -> io::Result<()> {
        let height = height.min(self.screen.height);
        if height != self.height {
            self.make_room(height)?;
            self.height = height;
            self.clear_live()?;
        }
        let area = Rect::new(0, self.top, self.screen.width, height);
        let mut next = Buffer::empty(area);
        let cursor = render(area, &mut next);
        let updates = self.shown.diff(&next);
        self.backend.draw(updates.into_iter()).map_err(io_error)?;
        // The cursor is left where it is known to be, for after a resize.
        match cursor {
            Some(position) => {
                self.move_cursor(position)?;
                self.backend.show_cursor().map_err(io_error)?;
            }
            None => {
                self.backend.hide_cursor().map_err(io_error)?;
                let row = self.top.min(self.screen.height.saturating_sub(1));
                self.move_cursor(Position::new(0, row))?;
            }
        }
        self.shown = next;
        self.backend.flush().map_err(io_error)
    }

    /// Clears the live region and leaves the cursor at its top, as harness exits or hands the
    /// terminal to another program. The cursor gets a row of its own below the last line.
    pub fn clear(&mut self) -> io::Result<()> {
        self.height = 0;
        self.make_room(1)?;
        self.clear_live()?;
        self.backend.show_cursor().map_err(io_error)?;
        self.backend.flush().map_err(io_error)
    }

    /// Whether the terminal is asked where its cursor is after a resize: it answered when
    /// harness started, and every time since.
    pub fn reports_cursor(&self) -> bool {
        self.reports_cursor
    }

    /// After the terminal changed size, asking the backend where the cursor is now, as
    /// [`resized_to`](Self::resized_to) says. For a backend that answers itself; the session asks
    /// its terminal through the thread that reads it.
    pub fn resized(&mut self) -> io::Result<()> {
        // Not asked while a full-screen view is open: the view's own screen has no say in where the
        // live region's cursor is.
        let cursor = if self.reports_cursor && self.full.is_none() {
            match self.backend.get_cursor_position() {
                Ok(position) => CursorReport::At(position),
                Err(_) => CursorReport::Missed,
            }
        } else {
            CursorReport::Unasked
        };
        self.resized_to(cursor)
    }

    /// After the terminal changed size, with its cursor where `cursor` says now. Terminals move
    /// rows when they resize: xterm keeps the cursor's row on screen as the window gets shorter,
    /// pushing the rows above it into scrollback, and others pull rows back from scrollback as it
    /// gets taller, or rewrap them. So the live region, cleared, is placed as far above the
    /// cursor as the cursor was below the region's top; the next draw makes the room it needs. A
    /// terminal that did not say, or was not asked, is taken to have done what xterm does; one
    /// that did not say [`CURSOR_MISSES`] times in a row is not asked again.
    pub fn resized_to(&mut self, cursor: CursorReport) -> io::Result<()> {
        self.screen = self.backend.size().map_err(io_error)?;
        let full_screen = self.full.is_some();
        if full_screen {
            // The live region waits, kept on screen, for the view to close.
            let height = self.height.min(self.screen.height);
            self.top = self
                .top
                .min(self.screen.height.saturating_sub(height.max(1)));
            self.height = height;
            self.cursor_row = self.cursor_row.min(self.screen.height.saturating_sub(1));
            self.full = Some(Buffer::empty(self.whole_screen()));
            self.resized_in_full = true;
            return self.backend.clear().map_err(io_error);
        }
        let bottom = self.screen.height.saturating_sub(1);
        let below_top = i32::from(self.cursor_row) - i32::from(self.top);
        let row = match cursor {
            CursorReport::At(position) => {
                self.cursor_misses = 0;
                Some(position.y.min(bottom))
            }
            CursorReport::Missed => {
                self.cursor_misses = self.cursor_misses.saturating_add(1);
                if self.cursor_misses >= CURSOR_MISSES {
                    self.reports_cursor = false;
                }
                None
            }
            CursorReport::Unasked => None,
        };
        self.cursor_row = row.unwrap_or(self.cursor_row.min(bottom));
        let top = (i32::from(self.cursor_row) - below_top).clamp(0, i32::from(self.screen.height));
        self.top = top as u16;
        self.height = 0;
        self.clear_live()
    }

    fn whole_screen(&self) -> Rect {
        Rect::new(0, 0, self.screen.width, self.screen.height)
    }

    /// Draws a full-screen view with `render`, which returns where the cursor goes (hidden for
    /// `None`). The first draw switches to the alternate screen, or, without one, clears the
    /// screen; after that only cells that changed are written. Draw nothing inline meanwhile.
    pub fn draw_full(
        &mut self,
        render: impl FnOnce(Rect, &mut Buffer) -> Option<Position>,
    ) -> io::Result<()> {
        let area = self.whole_screen();
        let shown = match self.full.take() {
            Some(shown) => shown,
            None => {
                match &mut self.alt {
                    Some(alt) => alt.enter(&mut self.backend)?,
                    None => self.backend.clear().map_err(io_error)?,
                }
                Buffer::empty(area)
            }
        };
        let mut next = Buffer::empty(area);
        let cursor = render(area, &mut next);
        let updates = shown.diff(&next);
        self.backend.draw(updates.into_iter()).map_err(io_error)?;
        match cursor {
            Some(position) => {
                self.backend
                    .set_cursor_position(position)
                    .map_err(io_error)?;
                self.backend.show_cursor().map_err(io_error)?;
            }
            None => self.backend.hide_cursor().map_err(io_error)?,
        }
        self.full = Some(next);
        self.backend.flush().map_err(io_error)
    }

    /// Closes the full-screen view: the normal screen comes back as it was, with the live region
    /// where it was, to be drawn again. Without an alternate screen the view was drawn over the
    /// screen, which is cleared, and the live region starts again at the top.
    pub fn leave_full(&mut self) -> io::Result<()> {
        if self.full.take().is_none() {
            return Ok(());
        }
        match &mut self.alt {
            Some(alt) => {
                alt.leave(&mut self.backend)?;
                if std::mem::take(&mut self.resized_in_full) {
                    self.clear_live()?;
                }
            }
            None => {
                self.resized_in_full = false;
                self.top = 0;
                self.clear_live()?;
            }
        }
        self.backend.flush().map_err(io_error)
    }
}
