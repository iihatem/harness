//! The terminal's modes while harness runs: raw mode (keys arrive one by one, Ctrl+C and Ctrl+S
//! included, since raw mode also turns off XON/XOFF flow control), bracketed paste (a paste
//! arrives as one event), and, where the terminal supports it, disambiguated keys (so
//! Shift+Enter differs from Enter). They are undone in reverse order when harness leaves, or
//! hands the terminal to an editor. Full-screen views (the pickers) use the terminal's
//! alternate screen, which gives the inline screen back as it was when they close.

use std::{
    io::{self, Write},
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
};

use ratatui::{
    backend::CrosstermBackend,
    crossterm::{
        event::{
            DisableBracketedPaste, EnableBracketedPaste, KeyboardEnhancementFlags,
            PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
        },
        queue,
        terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
    },
};

/// Turns the terminal's raw mode on and off.
pub trait RawMode {
    fn enable(&mut self) -> io::Result<()>;
    fn disable(&mut self) -> io::Result<()>;
}

/// Raw mode on the process's terminal, through crossterm (`cfmakeraw`, which also clears
/// `IXON`, so Ctrl+S and Ctrl+Q reach harness as keys).
pub struct CrosstermRawMode;

impl RawMode for CrosstermRawMode {
    fn enable(&mut self) -> io::Result<()> {
        terminal::enable_raw_mode()
    }

    fn disable(&mut self) -> io::Result<()> {
        terminal::disable_raw_mode()
    }
}

/// The modes harness sets, undone when dropped.
pub struct Modes<W: Write, R: RawMode> {
    out: W,
    raw: R,
    /// The terminal reports disambiguated keys (the kitty keyboard protocol).
    keyboard: bool,
    /// The modes are set; shared with the panic hook [`leave_on_panic`](Self::leave_on_panic)
    /// sets.
    active: Arc<AtomicBool>,
    /// A full-screen view is open on the alternate screen ([`CrosstermAltScreen`]); leaving the
    /// modes leaves it first, so a panic, a signal or the end of the session while a picker is
    /// open gives back the normal screen.
    alt: Arc<AtomicBool>,
}

impl<W: Write, R: RawMode> Modes<W, R> {
    /// Sets the modes, writing their escape sequences to `out`. `keyboard` asks for
    /// disambiguated keys, for terminals that support them.
    pub fn enter(out: W, raw: R, keyboard: bool) -> io::Result<Self> {
        let mut modes = Modes {
            out,
            raw,
            keyboard,
            active: Arc::default(),
            alt: Arc::default(),
        };
        modes.resume()?;
        Ok(modes)
    }

    /// Sets the modes again after [`suspend`](Self::suspend).
    pub fn resume(&mut self) -> io::Result<()> {
        if self.active.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.raw.enable()?;
        self.active.store(true, Ordering::SeqCst);
        enter(&mut self.out, self.keyboard)
    }

    /// Undoes the modes, in reverse order, so another program (an editor) or the shell gets the
    /// terminal as it was.
    pub fn suspend(&mut self) -> io::Result<()> {
        if !self.active.swap(false, Ordering::SeqCst) {
            return Ok(());
        }
        leave(&mut self.out, &mut self.raw, self.keyboard, &self.alt)
    }

    /// The alternate screen for full-screen views, which these modes leave if one is open.
    pub fn alt_screen(&self) -> CrosstermAltScreen {
        CrosstermAltScreen {
            open: self.alt.clone(),
        }
    }

    pub fn out(&self) -> &W {
        &self.out
    }

    /// Makes a panic, on any thread, leave these modes while its message prints, through `out`
    /// and `raw`, which reach the same terminal: in raw mode, the message would stair-step over
    /// the live region. The hook set before prints it. A panic that is caught (a check on
    /// another thread, a diff made off the session's task) leaves the session running, so the
    /// modes are set again once it has printed; one that ends harness undoes them as it unwinds,
    /// or as the session ends. While they are suspended (an editor has the terminal), a panic
    /// leaves the terminal as it is.
    pub fn leave_on_panic<O, P>(&self, out: impl Fn() -> O + Send + Sync + 'static, raw: P)
    where
        O: Write,
        P: RawMode + Send + 'static,
    {
        let active = self.active.clone();
        let keyboard = self.keyboard;
        let alt = self.alt.clone();
        let raw = Mutex::new(raw);
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let mut raw = raw.lock().unwrap_or_else(PoisonError::into_inner);
            let left = active.load(Ordering::SeqCst)
                && leave(&mut out(), &mut *raw, keyboard, &alt).is_ok();
            previous(info);
            if left && raw.enable().is_ok() {
                let _ = enter(&mut out(), keyboard);
            }
        }));
    }
}

/// Writes the modes' escape sequences to `out`, raw mode on already: bracketed paste, then
/// disambiguated keys when `keyboard`.
fn enter(out: &mut impl Write, keyboard: bool) -> io::Result<()> {
    queue!(out, EnableBracketedPaste)?;
    if keyboard {
        queue!(
            out,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
    }
    out.flush()
}

/// Undoes the modes in reverse order: disambiguated keys when `keyboard`, bracketed paste, then
/// raw mode.
fn leave(
    out: &mut impl Write,
    raw: &mut impl RawMode,
    keyboard: bool,
    alt: &AtomicBool,
) -> io::Result<()> {
    if alt.swap(false, Ordering::SeqCst) {
        queue!(out, LeaveAlternateScreen)?;
    }
    if keyboard {
        queue!(out, PopKeyboardEnhancementFlags)?;
    }
    queue!(out, DisableBracketedPaste)?;
    out.flush()?;
    raw.disable()
}

impl<W: Write, R: RawMode> Drop for Modes<W, R> {
    fn drop(&mut self) {
        let _ = self.suspend();
    }
}

/// A terminal's alternate screen, for full-screen views on `B`: a blank screen of its own,
/// which gives the normal screen back as it was, cursor included, when left. What harness wrote
/// to the normal screen, and the scrollback, are untouched meanwhile.
pub trait AltScreen<B> {
    fn enter(&mut self, backend: &mut B) -> io::Result<()>;
    fn leave(&mut self, backend: &mut B) -> io::Result<()>;
}

/// The real terminal's alternate screen (`ESC [ ? 1049 h`, and `l` to leave), through
/// crossterm.
#[derive(Default)]
pub struct CrosstermAltScreen {
    /// Whether the screen is open, for [`Modes`] to leave it should the session end meanwhile.
    open: Arc<AtomicBool>,
}

impl<W: Write> AltScreen<CrosstermBackend<W>> for CrosstermAltScreen {
    fn enter(&mut self, backend: &mut CrosstermBackend<W>) -> io::Result<()> {
        queue!(backend, EnterAlternateScreen)?;
        self.open.store(true, Ordering::SeqCst);
        backend.flush()
    }

    fn leave(&mut self, backend: &mut CrosstermBackend<W>) -> io::Result<()> {
        queue!(backend, LeaveAlternateScreen)?;
        self.open.store(false, Ordering::SeqCst);
        backend.flush()
    }
}
