//! The terminal's modes while harness runs: raw mode (keys arrive one by one, Ctrl+C and Ctrl+S
//! included, since raw mode also turns off XON/XOFF flow control), bracketed paste (a paste
//! arrives as one event), and, where the terminal supports it, disambiguated keys (so
//! Shift+Enter differs from Enter). They are undone in reverse order when harness leaves, or
//! hands the terminal to an editor.

use std::io::{self, Write};

use ratatui::crossterm::{
    event::{
        DisableBracketedPaste, EnableBracketedPaste, KeyboardEnhancementFlags,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    queue, terminal,
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
    active: bool,
}

impl<W: Write, R: RawMode> Modes<W, R> {
    /// Sets the modes, writing their escape sequences to `out`. `keyboard` asks for
    /// disambiguated keys, for terminals that support them.
    pub fn enter(out: W, raw: R, keyboard: bool) -> io::Result<Self> {
        let mut modes = Modes {
            out,
            raw,
            keyboard,
            active: false,
        };
        modes.resume()?;
        Ok(modes)
    }

    /// Sets the modes again after [`suspend`](Self::suspend).
    pub fn resume(&mut self) -> io::Result<()> {
        if self.active {
            return Ok(());
        }
        self.raw.enable()?;
        self.active = true;
        queue!(self.out, EnableBracketedPaste)?;
        if self.keyboard {
            queue!(
                self.out,
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
            )?;
        }
        self.out.flush()
    }

    /// Undoes the modes, in reverse order, so another program (an editor) or the shell gets the
    /// terminal as it was.
    pub fn suspend(&mut self) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }
        self.active = false;
        if self.keyboard {
            queue!(self.out, PopKeyboardEnhancementFlags)?;
        }
        queue!(self.out, DisableBracketedPaste)?;
        self.out.flush()?;
        self.raw.disable()
    }

    pub fn out(&self) -> &W {
        &self.out
    }
}

impl<W: Write, R: RawMode> Drop for Modes<W, R> {
    fn drop(&mut self) {
        let _ = self.suspend();
    }
}
