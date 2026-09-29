//! What the interactive session shows and how it reacts to keys, apart from the terminal and
//! the running agent: the transcript, the input editor with completion, and the status line.
//! Keys and events go in; lines for the scrollback, the live region, and actions for the
//! session to carry out come out.

use std::time::{Duration, Instant};

use harness_context::commands::{is_builtin, parse_invocation};
use harness_core::{agent::ContextUsage, event::AgentEvent, permission::Mode, turn::TurnInput};
use ratatui::{
    crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    layout::Position,
    text::{Line, Span},
};

use crate::{
    complete::{self, Completer, Offer},
    editor::{Edit, Editor},
    status::{self, Totals},
    style::Theme,
    text::{sanitize, wrap},
    transcript::Transcript,
};

/// Ctrl+C twice within this long exits.
pub const QUIT_WINDOW: Duration = Duration::from_secs(2);

/// Built-in commands that come with the rest of the terminal UI, and where.
const LATER: [(&str, &str); 7] = [
    ("model", "with model profiles (P4) and the model picker"),
    ("login", "with provider sign-in (P4)"),
    (
        "mode",
        "with the full terminal UI; press Shift+Tab to cycle plan, ask and auto",
    ),
    ("new", "with the session picker"),
    (
        "resume",
        "with the session picker; start harness with -c or --resume <id>",
    ),
    ("rewind", "with the rewind picker"),
    ("compact", "with the full terminal UI"),
];

/// A slash command expanded for a turn.
pub struct Prepared {
    pub input: TurnInput,
    /// Shown dim before the turn.
    pub notes: Vec<String>,
    pub warnings: Vec<String>,
}

/// What the session provides that the UI cannot know by itself: the custom commands, and how
/// they and `/init` expand.
pub trait Host: Send {
    /// Whether `/name` is a custom command.
    fn is_command(&self, name: &str) -> bool;
    /// The turn for `typed`, which names a custom command or `/init`.
    fn prepare(&mut self, typed: &str) -> Prepared;
}

/// Settings of the interactive session.
pub struct Options {
    pub theme: Theme,
    /// The session's model, `<provider>/<model>`.
    pub model: String,
    pub mode: Mode,
    /// Commands and their descriptions, built-ins first, for `/help` and completion.
    pub commands: Vec<(String, String)>,
    pub workspace: std::path::PathBuf,
    /// Earlier inputs, oldest first, for Up.
    pub history: Vec<String>,
    /// The instruction files in the system prompt, with their estimated tokens, for `/context`.
    pub instruction_files: Vec<(String, u64)>,
    /// Said next to the context window in `/context`, such as where its size comes from.
    pub window_note: Option<String>,
}

/// What the session should do after a key.
#[derive(Debug)]
pub enum Action {
    /// Start a turn.
    Run(TurnInput),
    /// Stop the running turn.
    Interrupt,
    /// Leave harness.
    Quit,
}

/// The completion list being shown.
struct Completion {
    offer: Offer,
    selected: usize,
}

pub struct App {
    pub transcript: Transcript,
    editor: Editor,
    completer: Completer,
    completion: Option<Completion>,
    host: Box<dyn Host>,
    model: String,
    mode: Mode,
    /// A line under the status line, such as how to exit.
    hint: Option<String>,
    /// When Ctrl+C was last pressed.
    ctrl_c: Option<Instant>,
    /// From the moment a turn is asked for until it has finished.
    running: bool,
    /// Where the next request's tokens go, as of the end of the last turn.
    context: ContextUsage,
    totals: Totals,
    instruction_files: Vec<(String, u64)>,
    window_note: Option<String>,
    width: usize,
}

impl App {
    pub fn new(options: Options, host: Box<dyn Host>, width: usize) -> App {
        App {
            transcript: Transcript::new(options.theme),
            editor: Editor::new(options.history),
            completer: Completer::new(options.commands, &options.workspace),
            completion: None,
            host,
            model: options.model,
            mode: options.mode,
            hint: None,
            ctrl_c: None,
            running: false,
            context: ContextUsage::default(),
            totals: Totals::default(),
            instruction_files: options.instruction_files,
            window_note: options.window_note,
            width,
        }
    }

    fn theme(&self) -> Theme {
        *self.transcript.theme()
    }

    /// The screen's width changed.
    pub fn set_width(&mut self, width: usize) {
        self.width = width;
    }

    pub fn editor(&self) -> &Editor {
        &self.editor
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Whether a turn is running, or about to.
    pub fn busy(&self) -> bool {
        self.running
    }

    /// Takes in an event from the agent.
    pub fn on_event(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::TurnFinished { .. } => self.running = false,
            AgentEvent::Usage { model, usage } => self.totals.add(model, usage),
            _ => {}
        }
        self.transcript.on_event(event, self.width);
    }

    /// Where the next request's tokens go, now.
    pub fn set_context(&mut self, context: ContextUsage) {
        self.context = context;
    }

    /// Asks for a turn.
    fn run(&mut self, input: TurnInput) -> Option<Action> {
        self.running = true;
        Some(Action::Run(input))
    }

    /// Takes in a paste.
    pub fn on_paste(&mut self, text: &str) {
        self.editor.paste(text);
        self.update_completion();
    }

    fn update_completion(&mut self) {
        let offer = self
            .completer
            .offer(self.editor.text(), self.editor.cursor());
        self.completion = offer.map(|offer| Completion { offer, selected: 0 });
    }

    /// Takes in a key; `now` is when it was pressed.
    pub fn on_key(&mut self, key: KeyEvent, now: Instant) -> Option<Action> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            return self.ctrl_c(now);
        }
        self.ctrl_c = None;
        self.hint = None;
        if ctrl && key.code == KeyCode::Char('d') && self.editor.is_empty() {
            return Some(Action::Quit);
        }
        if let Some(action) = self.completion_key(key) {
            return action;
        }
        match key.code {
            KeyCode::Esc if self.busy() => return Some(Action::Interrupt),
            KeyCode::Esc => return None,
            _ => {}
        }
        match self.editor.key(key) {
            Edit::Submit => return self.submit(),
            Edit::Handled => self.update_completion(),
            Edit::Ignored => {}
        }
        None
    }

    /// Ctrl+C: interrupts a running turn, or clears the input; pressed again within
    /// [`QUIT_WINDOW`], it exits.
    fn ctrl_c(&mut self, now: Instant) -> Option<Action> {
        if self
            .ctrl_c
            .is_some_and(|at| now.duration_since(at) <= QUIT_WINDOW)
        {
            return Some(Action::Quit);
        }
        self.ctrl_c = Some(now);
        self.hint = Some("press Ctrl+C again to exit".into());
        if self.busy() {
            return Some(Action::Interrupt);
        }
        self.editor.clear();
        self.completion = None;
        None
    }

    /// Keys for the completion list, when it is shown: `Some` when the key was used.
    fn completion_key(&mut self, key: KeyEvent) -> Option<Option<Action>> {
        let completion = self.completion.as_mut()?;
        let count = completion.offer.items.len();
        match key.code {
            KeyCode::Up => {
                completion.selected = (completion.selected + count - 1) % count;
            }
            KeyCode::Down => completion.selected = (completion.selected + 1) % count,
            KeyCode::Esc => self.completion = None,
            KeyCode::Tab => self.accept(),
            KeyCode::Enter
                if key.modifiers.is_empty()
                    && completion.offer.items[completion.selected].insert
                        != self.editor.text()[completion.offer.replace.clone()] =>
            {
                self.accept()
            }
            _ => return None,
        }
        Some(None)
    }

    /// Puts the selected completion into the input.
    fn accept(&mut self) {
        let Some(completion) = self.completion.take() else {
            return;
        };
        let item = &completion.offer.items[completion.selected];
        self.editor
            .replace_word(completion.offer.replace.clone(), &item.insert);
        self.update_completion();
    }

    /// Enter: sends the input, runs a built-in command, or says why it cannot.
    fn submit(&mut self) -> Option<Action> {
        if self.editor.expanded().trim().is_empty() {
            return None;
        }
        if self.busy() {
            self.hint = Some("a turn is running: press Esc to interrupt it".into());
            return None;
        }
        self.completion = None;
        let full = self.editor.expanded();
        let width = self.width;
        if let Some(invocation) = parse_invocation(&full) {
            let name = invocation.name.to_string();
            if name == "help" {
                self.editor.submit();
                self.transcript.push_user(&full, width);
                self.help();
                return None;
            }
            if name == "quit" {
                return Some(Action::Quit);
            }
            if name == "context" || name == "usage" {
                self.editor.submit();
                self.transcript.push_user(&full, width);
                let theme = self.theme();
                let lines = if name == "context" {
                    status::context_report(
                        &self.context,
                        &self.instruction_files,
                        self.window_note.as_deref(),
                        &theme,
                    )
                } else {
                    self.totals.report(&theme)
                };
                self.transcript.push_lines(lines, width);
                return None;
            }
            if let Some((_, when)) = LATER.iter().find(|(n, _)| *n == name) {
                self.editor.submit();
                self.transcript.push_user(&full, width);
                self.transcript.push_note(
                    &format!("/{name} is not available yet: it comes {when}."),
                    width,
                );
                return None;
            }
            if name == "init" || (!is_builtin(&name) && self.host.is_command(&name)) {
                self.editor.submit();
                let prepared = self.host.prepare(&full);
                self.transcript.push_user(&full, width);
                for warning in &prepared.warnings {
                    self.transcript.push_warning(warning, width);
                }
                for note in &prepared.notes {
                    self.transcript.push_note(note, width);
                }
                return self.run(prepared.input);
            }
            if !is_builtin(&name) {
                self.transcript.push_error(
                    &format!(
                        "unknown command /{name}; custom commands are Markdown files in .harness/commands, .claude/commands or .opencode/commands; to send text that starts with /, put a word before it"
                    ),
                    width,
                );
                return None;
            }
        }
        let (shown, full) = self.editor.submit();
        self.transcript.push_user(&shown, width);
        self.run(TurnInput::from(full))
    }

    /// `/help`: the commands and the keys.
    fn help(&mut self) {
        let theme = self.theme();
        let mut lines = vec![Line::from(Span::styled("Commands", theme.bold()))];
        for (name, description) in self.completer.commands() {
            lines.push(Line::from(vec![
                Span::styled(format!("  /{}", sanitize(name)), theme.accent()),
                Span::styled(format!("  {}", sanitize(description)), theme.dim()),
            ]));
        }
        lines.push(Line::from(Span::styled("Keys", theme.bold())));
        for (keys, what) in [
            ("Enter", "send"),
            ("Alt+Enter, Shift+Enter, Ctrl+J", "new line"),
            ("Up, Down", "earlier inputs"),
            ("Tab", "complete a /command or @file"),
            ("Ctrl+O", "expand a collapsed paste"),
            ("Esc", "interrupt the running turn"),
            ("Ctrl+C twice", "exit"),
        ] {
            lines.push(Line::from(vec![
                Span::styled(format!("  {keys}"), theme.accent()),
                Span::styled(format!("  {what}"), theme.dim()),
            ]));
        }
        self.transcript.push_lines(lines, self.width);
    }

    /// The status line.
    fn status(&self) -> Line<'static> {
        status::status_line(
            &self.model,
            self.mode,
            &self.context,
            &self.totals,
            &self.theme(),
        )
    }

    /// The live region's lines, at most `rows`, and where the cursor is in them.
    pub fn live(&self, rows: usize) -> (Vec<Line<'static>>, Position) {
        let theme = self.theme();
        let width = self.width;
        let (editor, cursor) = self.editor.render("› ", width, &theme);
        let mut below: Vec<Line<'static>> = Vec::new();
        if let Some(completion) = &self.completion {
            below.extend(complete::render(
                &completion.offer,
                completion.selected,
                width,
                &theme,
            ));
        }
        below.extend(wrap(&self.status(), width, &[], &[]));
        if let Some(hint) = &self.hint {
            below.push(Line::from(Span::styled(sanitize(hint), theme.dim())));
        }
        let fixed = editor.len() + below.len();
        let mut lines = self.transcript.live(width, rows.saturating_sub(fixed + 1));
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        let top = lines.len();
        lines.extend(editor);
        lines.extend(below);
        let skip = lines.len().saturating_sub(rows);
        let cursor = Position::new(
            cursor.x,
            (cursor.y as usize + top).saturating_sub(skip) as u16,
        );
        (lines.split_off(skip), cursor)
    }
}
