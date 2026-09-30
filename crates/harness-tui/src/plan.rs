//! Plan mode's ending: once a planning turn ends with a plan, the user builds it, edits it in
//! their editor, or keeps planning.

use std::{
    io::{self, Write},
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};

use ratatui::{
    crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
    text::{Line, Span},
};

use crate::{
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
        ExternalEditor { command, modes }
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
            self.modes.suspend()?;
            let status = Command::new("/bin/sh")
                .arg("-c")
                .arg(format!("{} \"$1\"", self.command))
                .arg("harness")
                .arg(&path)
                .status();
            self.modes.resume()?;
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
