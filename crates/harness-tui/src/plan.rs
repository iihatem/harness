//! Plan mode's ending: once a planning turn ends with a plan, the user builds it, edits it in
//! their editor, or keeps planning.

use std::{
    io::{self, Write},
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};

use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};
use ratatui::{
    crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
    text::{Line, Span},
};

use crate::{
    input::InputPause,
    style::Theme,
    terminal::{Modes, RawMode},
    text::wrap,
};

/// Opens text in the user's editor and returns it as saved.
pub trait TextEditor: Send {
    fn edit(&mut self, text: &str) -> io::Result<String>;
}

/// The user's `$EDITOR` (`vi` without one), run with the terminal's modes undone meanwhile.
pub struct ExternalEditor<W: Write + Send, R: RawMode + Send> {
    /// Run as `sh -c '<command> "$1"'`, so it may hold arguments, as `$EDITOR` often does.
    command: String,
    modes: Modes<W, R>,
    /// Stops the session's reading of the terminal while the editor reads it.
    input: Option<InputPause>,
}

impl<W: Write + Send, R: RawMode + Send> ExternalEditor<W, R> {
    /// The editor `$EDITOR` names, or `vi`, owning the terminal's `modes` for the session.
    pub fn from_env(modes: Modes<W, R>) -> Self {
        let command = std::env::var("EDITOR")
            .ok()
            .filter(|e| !e.trim().is_empty())
            .unwrap_or_else(|| "vi".into());
        ExternalEditor::new(command, modes)
    }

    pub fn new(command: String, modes: Modes<W, R>) -> Self {
        ExternalEditor {
            command,
            modes,
            input: None,
        }
    }

    /// Stops the session's reading of the terminal with `input` while the editor runs, so every
    /// key goes to the editor.
    pub fn pausing(mut self, input: InputPause) -> Self {
        self.input = Some(input);
        self
    }
}

/// A new file for the plan, readable only by the user.
fn plan_file() -> io::Result<(PathBuf, std::fs::File)> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    for _ in 0..100 {
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!("harness-plan-{}-{n}.md", std::process::id()));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => return Ok((path, file)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::other("cannot create a file for the plan"))
}

impl<W: Write + Send, R: RawMode + Send> TextEditor for ExternalEditor<W, R> {
    fn edit(&mut self, text: &str) -> io::Result<String> {
        let (path, mut file) = plan_file()?;
        let result = (|| {
            file.write_all(text.as_bytes())?;
            drop(file);
            let _paused = self.input.as_ref().map(InputPause::pause);
            // Ctrl+C and Ctrl+\ at the editor signal its process group, which is harness's: they
            // are the editor's, until raw mode is back.
            let held = HeldInterrupts::hold();
            self.modes.suspend()?;
            let status = Command::new("/bin/sh")
                .arg("-c")
                .arg(format!("{} \"$1\"", self.command))
                .arg("harness")
                .arg(&path)
                .status();
            let resumed = self.modes.resume();
            drop(held);
            resumed?;
            let status = status?;
            if !status.success() {
                return Err(io::Error::other(format!(
                    "the editor `{}` exited with {status}",
                    self.command
                )));
            }
            std::fs::read_to_string(&path)
        })();
        let _ = std::fs::remove_file(&path);
        result
    }
}

/// SIGINT and SIGQUIT caught and dropped while held, and given back their earlier actions
/// after. A caught signal, unlike an ignored one, is not passed on to the programs harness runs:
/// they get the default action.
struct HeldInterrupts(Vec<(Signal, SigAction)>);

extern "C" fn drop_signal(_: libc::c_int) {}

impl HeldInterrupts {
    fn hold() -> HeldInterrupts {
        let caught = SigAction::new(
            SigHandler::Handler(drop_signal),
            SaFlags::SA_RESTART,
            SigSet::empty(),
        );
        let mut earlier = Vec::new();
        for signal in [Signal::SIGINT, Signal::SIGQUIT] {
            // SAFETY: the handler does nothing, so it is async-signal-safe.
            if let Ok(action) = unsafe { sigaction(signal, &caught) } {
                earlier.push((signal, action));
            }
        }
        HeldInterrupts(earlier)
    }
}

impl Drop for HeldInterrupts {
    fn drop(&mut self) {
        for (signal, action) in &self.0 {
            // SAFETY: puts back the action that was there before.
            let _ = unsafe { sigaction(*signal, action) };
        }
    }
}

/// What the user chose for a plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    Build,
    Edit,
    KeepPlanning,
}

/// A plan waiting for the user's choice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanChoice {
    pub plan: String,
    /// The user edited it, so the model has not seen it as it is.
    pub edited: bool,
}

impl PlanChoice {
    /// The choice a key makes, if any: `b`, `e` and `k`, without Ctrl or Alt, and Esc to keep
    /// planning. Enter was typed for the input.
    pub fn key(&self, key: KeyEvent) -> Option<Choice> {
        let plain = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        match key.code {
            KeyCode::Char('b') if plain => Some(Choice::Build),
            KeyCode::Char('e') if plain => Some(Choice::Edit),
            KeyCode::Char('k') if plain => Some(Choice::KeepPlanning),
            KeyCode::Esc => Some(Choice::KeepPlanning),
            _ => None,
        }
    }

    /// The prompt, `width` columns wide.
    pub fn render(&self, width: usize, theme: &Theme) -> Vec<Line<'static>> {
        let title = if self.edited {
            "The edited plan is ready: "
        } else {
            "The plan is ready: "
        };
        let line = Line::from(vec![
            Span::styled(title, theme.bold()),
            Span::styled("[b] ", theme.accent()),
            Span::raw("build it  "),
            Span::styled("[e] ", theme.accent()),
            Span::raw("edit it in your editor  "),
            Span::styled("[k] ", theme.accent()),
            Span::raw("keep planning"),
        ]);
        wrap(&line, width, &[], &[Span::raw("  ")])
    }

    /// What the build turn sends: the plan above, or the edited plan itself.
    pub fn build_message(&self) -> String {
        if self.edited {
            format!("Implement this plan:\n\n{}", self.plan)
        } else {
            "Implement the plan above.".into()
        }
    }
}
