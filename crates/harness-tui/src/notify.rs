//! Telling the user something needs them while they look elsewhere: a desktop notification
//! through the terminal (OSC 9) and the terminal bell, when a turn that ran long finishes, or
//! an approval waits.

use std::{
    io::{self, Write},
    time::Duration,
};

/// Turns that run at least this long notify when they finish.
pub const LONG_TURN: Duration = Duration::from_secs(10);

/// Characters of a notification's text at most.
const MAX_TEXT: usize = 200;

/// Sends notifications.
pub trait Notify: Send {
    fn notify(&mut self, text: &str) -> io::Result<()>;
}

/// Notifications written to the terminal: OSC 9 (`ESC ] 9 ; text BEL`) and a bell, each when
/// enabled. The session enables them only when stdout is a terminal.
pub struct TerminalNotifier<W: Write + Send> {
    out: W,
    desktop: bool,
    bell: bool,
}

impl<W: Write + Send> TerminalNotifier<W> {
    pub fn new(out: W, desktop: bool, bell: bool) -> Self {
        TerminalNotifier { out, desktop, bell }
    }

    pub fn out(&self) -> &W {
        &self.out
    }
}

/// `text` as an OSC 9 payload: control characters (which would end the sequence early, or start
/// another) are dropped, and so are the characters that reorder text or draw nothing, which
/// could disguise a command waiting for approval; it is cut to [`MAX_TEXT`] characters.
fn payload(text: &str) -> String {
    crate::text::strip(text).chars().take(MAX_TEXT).collect()
}

impl<W: Write + Send> Notify for TerminalNotifier<W> {
    fn notify(&mut self, text: &str) -> io::Result<()> {
        if self.desktop {
            // Starting with "harness:" keeps a terminal from reading the text as one of the
            // numbered OSC 9 commands (ConEmu's `9;4;…` progress, for one).
            write!(self.out, "\x1b]9;harness: {}\x07", payload(text))?;
        }
        if self.bell {
            self.out.write_all(b"\x07")?;
        }
        self.out.flush()
    }
}

/// How long `elapsed` is, for people: `12s`, `3m 2s`, `1h 5m`.
pub fn duration(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m {}s", secs / 60, secs % 60),
        _ => format!("{}h {}m", secs / 3600, (secs % 3600) / 60),
    }
}
