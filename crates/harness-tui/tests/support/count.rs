//! A backend that counts how often the live region is drawn.

#![allow(dead_code)]

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use ratatui::{
    backend::{Backend, ClearType, WindowSize},
    buffer::Cell,
    layout::{Position, Size},
};

/// `inner`, counting the draws of the live region: each ends by showing or hiding the cursor.
pub struct Counted<B> {
    pub inner: B,
    draws: Arc<AtomicUsize>,
}

impl<B> Counted<B> {
    pub fn new(inner: B) -> (Counted<B>, Arc<AtomicUsize>) {
        let draws = Arc::new(AtomicUsize::new(0));
        (
            Counted {
                inner,
                draws: draws.clone(),
            },
            draws,
        )
    }

    fn count(&self) {
        self.draws.fetch_add(1, Ordering::SeqCst);
    }
}

impl<B: Backend> Backend for Counted<B> {
    type Error = B::Error;

    fn draw<'a, I>(&mut self, content: I) -> Result<(), B::Error>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        self.inner.draw(content)
    }
    fn append_lines(&mut self, n: u16) -> Result<(), B::Error> {
        self.inner.append_lines(n)
    }
    fn hide_cursor(&mut self) -> Result<(), B::Error> {
        self.count();
        self.inner.hide_cursor()
    }
    fn show_cursor(&mut self) -> Result<(), B::Error> {
        self.count();
        self.inner.show_cursor()
    }
    fn get_cursor_position(&mut self) -> Result<Position, B::Error> {
        self.inner.get_cursor_position()
    }
    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> Result<(), B::Error> {
        self.inner.set_cursor_position(position)
    }
    fn clear(&mut self) -> Result<(), B::Error> {
        self.inner.clear()
    }
    fn clear_region(&mut self, clear_type: ClearType) -> Result<(), B::Error> {
        self.inner.clear_region(clear_type)
    }
    fn size(&self) -> Result<Size, B::Error> {
        self.inner.size()
    }
    fn window_size(&mut self) -> Result<WindowSize, B::Error> {
        self.inner.window_size()
    }
    fn flush(&mut self) -> Result<(), B::Error> {
        self.inner.flush()
    }
}
