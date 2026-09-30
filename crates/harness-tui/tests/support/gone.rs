//! A backend whose terminal can go away: from then on, every write fails as writing a closed
//! terminal does.

#![allow(dead_code)]

use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use ratatui::{
    backend::{Backend, ClearType, TestBackend, WindowSize},
    buffer::Cell,
    layout::{Position, Size},
};

pub struct Breakable {
    pub inner: TestBackend,
    gone: Arc<AtomicBool>,
}

impl Breakable {
    /// The backend, and the switch that makes its terminal go away.
    pub fn new(inner: TestBackend) -> (Breakable, Arc<AtomicBool>) {
        let gone = Arc::new(AtomicBool::new(false));
        (
            Breakable {
                inner,
                gone: gone.clone(),
            },
            gone,
        )
    }

    fn write(&self) -> io::Result<()> {
        if self.gone.load(Ordering::SeqCst) {
            Err(io::Error::from_raw_os_error(5))
        } else {
            Ok(())
        }
    }
}

impl Backend for Breakable {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        self.write()?;
        let _ = self.inner.draw(content);
        Ok(())
    }
    fn append_lines(&mut self, n: u16) -> io::Result<()> {
        self.write()?;
        let _ = self.inner.append_lines(n);
        Ok(())
    }
    fn hide_cursor(&mut self) -> io::Result<()> {
        self.write()
    }
    fn show_cursor(&mut self) -> io::Result<()> {
        self.write()
    }
    fn get_cursor_position(&mut self) -> io::Result<Position> {
        self.write()?;
        Ok(self.inner.get_cursor_position().unwrap_or_default())
    }
    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        self.write()?;
        let _ = self.inner.set_cursor_position(position);
        Ok(())
    }
    fn clear(&mut self) -> io::Result<()> {
        self.write()?;
        let _ = self.inner.clear();
        Ok(())
    }
    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        self.write()?;
        let _ = self.inner.clear_region(clear_type);
        Ok(())
    }
    fn size(&self) -> io::Result<Size> {
        Ok(self.inner.size().unwrap_or_default())
    }
    fn window_size(&mut self) -> io::Result<WindowSize> {
        Ok(self.inner.window_size().unwrap())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.write()
    }
}
