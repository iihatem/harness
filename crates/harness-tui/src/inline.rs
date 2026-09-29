//! The terminal, drawn inline: finished lines are written above a live region at the bottom of
//! what harness has drawn, and scroll off into the terminal's own scrollback, where the terminal's
//! scrolling, search and copy work as usual. Only the live region is redrawn, and only the cells
//! in it that changed. Unlike ratatui's inline viewport, the live region's height changes with
//! what it shows.

use std::io;

use ratatui::{
    backend::{Backend, ClearType},
    buffer::Buffer,
    layout::{Position, Rect, Size},
    text::Line,
    widgets::Widget,
};

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
    /// are the user's.
    pub fn new(backend: B, top: u16) -> io::Result<Self> {
        let screen = backend.size().map_err(io_error)?;
        let top = top.min(screen.height.saturating_sub(1));
        Ok(InlineTerminal {
            backend,
            screen,
            top,
            height: 0,
            shown: Buffer::empty(Rect::new(0, top, screen.width, 0)),
        })
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
        self.backend
            .set_cursor_position(Position::new(0, bottom))
            .map_err(io_error)?;
        self.backend.append_lines(rows).map_err(io_error)
    }

    /// Clears from the live region's top to the bottom of the screen, and forgets what it showed.
    fn clear_live(&mut self) -> io::Result<()> {
        self.backend
            .set_cursor_position(Position::new(0, self.top))
            .map_err(io_error)?;
        self.backend
            .clear_region(ClearType::AfterCursor)
            .map_err(io_error)?;
        self.shown = Buffer::empty(Rect::new(0, self.top, self.screen.width, self.height));
        Ok(())
    }

    /// Writes `lines`, each at most the screen's width, above the live region, which moves down
    /// (and, at the bottom of the screen, pushes the rows above into scrollback). The live region
    /// is cleared: draw it again afterwards.
    pub fn insert(&mut self, lines: &[Line<'_>]) -> io::Result<()> {
        if lines.is_empty() {
            return Ok(());
        }
        let height = self.height;
        self.height = 0;
        self.clear_live()?;
        let width = self.screen.width;
        let mut y = self.top;
        for line in lines {
            if y >= self.screen.height {
                self.scroll_up(1)?;
                y = self.screen.height - 1;
            }
            let area = Rect::new(0, y, width, 1);
            let mut row = Buffer::empty(area);
            line.render(area, &mut row);
            self.backend
                .draw(
                    row.content
                        .iter()
                        .enumerate()
                        .map(|(x, cell)| (x as u16, y, cell)),
                )
                .map_err(io_error)?;
            y += 1;
        }
        self.top = y.min(self.screen.height);
        self.make_room(height)?;
        self.height = height.min(self.screen.height);
        self.shown = Buffer::empty(Rect::new(0, self.top, width, self.height));
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
        match cursor {
            Some(position) => {
                self.backend
                    .set_cursor_position(position)
                    .map_err(io_error)?;
                self.backend.show_cursor().map_err(io_error)?;
            }
            None => self.backend.hide_cursor().map_err(io_error)?,
        }
        self.shown = next;
        self.backend.flush().map_err(io_error)
    }

    /// Clears the live region and leaves the cursor at its top, as harness exits or hands the
    /// terminal to another program.
    pub fn clear(&mut self) -> io::Result<()> {
        self.height = 0;
        self.clear_live()?;
        self.backend.show_cursor().map_err(io_error)?;
        self.backend.flush().map_err(io_error)
    }

    /// After the terminal changed size: the live region stays on screen and is drawn anew.
    pub fn resized(&mut self) -> io::Result<()> {
        self.screen = self.backend.size().map_err(io_error)?;
        let height = self.height.min(self.screen.height);
        self.top = self
            .top
            .min(self.screen.height.saturating_sub(height.max(1)));
        self.height = height;
        self.clear_live()
    }
}
