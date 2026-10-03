//! For tests of the terminal UI: what a real terminal does that ratatui's `TestBackend` does not.

use std::{
    io,
    sync::{Arc, Mutex},
};

use ratatui::{
    backend::{Backend, TestBackend},
    buffer::Buffer,
    layout::Position,
};

use crate::terminal::AltScreen;

/// The alternate screen as a terminal keeps it, on ratatui's `TestBackend`: entering saves the
/// screen and the cursor and clears the screen; leaving puts them back, as much of them as fits
/// when the screen shrank meanwhile. Each call is logged.
pub struct TestAltScreen {
    saved: Option<(Buffer, Position)>,
    log: Arc<Mutex<Vec<&'static str>>>,
}

impl TestAltScreen {
    /// The alternate screen, and its log of `"enter"` and `"leave"`.
    pub fn new() -> (TestAltScreen, Arc<Mutex<Vec<&'static str>>>) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let screen = TestAltScreen {
            saved: None,
            log: log.clone(),
        };
        (screen, log)
    }
}

impl AltScreen<TestBackend> for TestAltScreen {
    fn enter(&mut self, backend: &mut TestBackend) -> io::Result<()> {
        self.log.lock().unwrap().push("enter");
        let cursor = backend.get_cursor_position().map_err(io::Error::other)?;
        self.saved = Some((backend.buffer().clone(), cursor));
        backend.clear().map_err(io::Error::other)
    }

    fn leave(&mut self, backend: &mut TestBackend) -> io::Result<()> {
        self.log.lock().unwrap().push("leave");
        let Some((screen, cursor)) = self.saved.take() else {
            return Ok(());
        };
        let width = screen.area.width.max(1) as usize;
        let now = backend.size().map_err(io::Error::other)?;
        backend
            .draw(
                screen
                    .content
                    .iter()
                    .enumerate()
                    .map(|(i, cell)| ((i % width) as u16, (i / width) as u16, cell))
                    .filter(|(x, y, _)| *x < now.width && *y < now.height),
            )
            .map_err(io::Error::other)?;
        let cursor = Position::new(
            cursor.x.min(now.width.saturating_sub(1)),
            cursor.y.min(now.height.saturating_sub(1)),
        );
        backend
            .set_cursor_position(cursor)
            .map_err(io::Error::other)
    }
}
