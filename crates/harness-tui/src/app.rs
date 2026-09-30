//! What the interactive session shows and how it reacts to keys, apart from the terminal and
//! the running agent: the transcript, the input editor with completion, and the status line.
//! Keys and events go in; lines for the scrollback, the live region, and actions for the
//! session to carry out come out.

use std::{
    collections::VecDeque,
    sync::Arc,
    time::{Duration, Instant},
};

use harness_context::commands::{is_builtin, parse_invocation};
use harness_core::{
    agent::{ApprovalDecision, ApprovalRequest, ContextUsage},
    event::{AgentEvent, TurnEndReason},
    permission::Mode,
    redact::Redactor,
    turn::{Steering, TurnInput},
};
use ratatui::{
    crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    layout::Position,
    text::{Line, Span},
};

use crate::{
    approval::{Answered, Arming, Prompt, Reply},
    complete::{self, Completer, Offer},
    editor::{Edit, Editor},
    notify::{self, Notify},
    plan::{Choice, PlanChoice, TextEditor},
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
    /// What the host has had to warn about since it was last asked, such as a renewed sign-in
    /// that could not be stored. Each warning is given once.
    fn take_warnings(&self) -> Vec<String> {
        Vec::new()
    }
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
    /// The mode Build switches to when the session started in plan mode.
    pub default_mode: Mode,
    /// Opens a plan in the user's editor.
    pub text_editor: Option<Box<dyn TextEditor>>,
    /// Tells the user when a long turn ends or an approval waits.
    pub notifier: Option<Box<dyn Notify>>,
}

/// What the session should do after a key.
#[derive(Debug)]
pub enum Action {
    /// Start a turn.
    Run(TurnInput),
    /// Switch the approval mode.
    SetMode(Mode),
    /// Switch the approval mode, then start a turn.
    RunIn(Mode, TurnInput),
    /// Open the plan in the user's editor.
    EditPlan(String),
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
    /// An approval waiting for the user's answer.
    prompt: Option<Prompt>,
    /// When the approval or the plan choice shown starts to take keys.
    arming: Arming,
    /// The mode chosen while a turn runs, to switch to when it ends.
    pending_mode: Option<Mode>,
    /// Input to send when the running turn ends: as shown, and in full.
    queued: VecDeque<(String, String)>,
    /// Where send-now input goes; the agent takes it at the next tool result.
    steering: Steering,
    /// Send-now input the agent has not taken yet.
    sent_now: Vec<String>,
    /// The turn's last reply, which in plan mode is the plan.
    last_reply: String,
    /// The mode to go back to when a plan is built.
    mode_before_plan: Option<Mode>,
    default_mode: Mode,
    /// The session started in plan mode, and the agent has not been told to plan yet.
    plan_note_pending: bool,
    /// A plan waiting for the user's choice.
    plan_choice: Option<PlanChoice>,
    /// When the running turn started.
    turn_started: Option<Instant>,
    /// Notifications to send.
    notifications: Vec<String>,
    workspace: std::path::PathBuf,
    width: usize,
    /// Keeps the secrets harness knows out of what approvals show.
    redactor: Option<Arc<Redactor>>,
    /// Said when the session first switches to a mode that writes.
    write_mode_warning: Option<String>,
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
            prompt: None,
            arming: Arming::default(),
            pending_mode: None,
            queued: VecDeque::new(),
            steering: Steering::new(),
            sent_now: Vec::new(),
            last_reply: String::new(),
            mode_before_plan: None,
            default_mode: options.default_mode,
            plan_note_pending: options.mode == Mode::Plan,
            plan_choice: None,
            turn_started: None,
            notifications: Vec::new(),
            workspace: options.workspace,
            width,
            redactor: None,
            write_mode_warning: None,
        }
    }

    /// What the modes that write (ask, auto) lack, said the first time the session switches to
    /// one: a session that started in another mode was not told as it started.
    pub fn set_write_mode_warning(&mut self, warning: Option<String>) {
        self.write_mode_warning = warning;
    }

    /// Shows approvals with the secrets `redactor` knows replaced.
    pub fn set_redactor(&mut self, redactor: Arc<Redactor>) {
        self.redactor = Some(redactor);
    }

    /// What the CLI provides.
    pub fn host(&self) -> &dyn Host {
        &*self.host
    }

    fn theme(&self) -> Theme {
        *self.transcript.theme()
    }

    /// The screen's width changed.
    pub fn set_width(&mut self, width: usize) {
        self.width = width;
    }

    /// Notifications to send now, which are then forgotten.
    pub fn take_notifications(&mut self) -> Vec<String> {
        std::mem::take(&mut self.notifications)
    }

    /// What the prompt for `request` needs to show what it asks about: the tool call's
    /// arguments (when known), the workspace, and the theme.
    pub fn approval_context(
        &self,
        request: &ApprovalRequest,
    ) -> (Option<serde_json::Value>, std::path::PathBuf, Theme) {
        let arguments = self.transcript.arguments(&request.call_id).cloned();
        (arguments, self.workspace.clone(), self.theme())
    }

    /// The agent asks the user to approve `request`, showing `body`
    /// ([`approval::body`](crate::approval::body)) under the reason.
    pub fn on_approval(
        &mut self,
        request: ApprovalRequest,
        reply: Reply,
        body: Vec<Line<'static>>,
    ) {
        let mut prompt = Prompt::new(request, reply, body);
        if let Some(redactor) = &self.redactor {
            prompt.redact(redactor);
        }
        self.notifications
            .push(format!("approval needed: {}", prompt.request().reason));
        self.prompt = Some(prompt);
        self.arming = Arming::default();
    }

    /// The live region was drawn at `now`: an approval or a plan choice it showed for the first
    /// time takes keys from [`ARMING_DELAY`](crate::approval::ARMING_DELAY) later.
    pub fn drawn(&mut self, now: Instant) {
        if self.prompt.is_some() || self.plan_choice.is_some() {
            self.arming.drawn(now);
        }
    }

    /// When the approval or plan choice shown takes keys: `None` while none has been drawn.
    pub fn armed_at(&self) -> Option<Instant> {
        self.arming
            .armed_at()
            .filter(|_| self.prompt.is_some() || self.plan_choice.is_some())
    }

    /// Denies the approval waiting, if any, as the session ends: the turn stops.
    pub fn deny_waiting(&mut self) {
        if self.prompt.is_some() {
            self.answer(Answered::Interrupt);
        }
    }

    /// Leaves harness: nothing waits for an answer after.
    fn quit(&mut self) -> Option<Action> {
        self.deny_waiting();
        Some(Action::Quit)
    }

    /// The approval waiting for an answer.
    pub fn prompt(&self) -> Option<&Prompt> {
        self.prompt.as_ref()
    }

    /// Answers the waiting approval, and notes the answer in the transcript.
    fn answer(&mut self, answered: Answered) {
        let Some(mut prompt) = self.prompt.take() else {
            return;
        };
        let line = prompt.outcome(&answered, &self.theme());
        let decision = match answered {
            Answered::Decided(decision) => decision,
            Answered::Interrupt => ApprovalDecision::Deny {
                feedback: Some("the user stopped the turn".into()),
            },
        };
        prompt.answer(decision);
        self.transcript.push_lines(vec![line], self.width);
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

    /// Where send-now input goes: give it to the agent (`Agent::with_steering`).
    pub fn steering(&self) -> Steering {
        self.steering.clone()
    }

    /// Takes in an event from the agent.
    pub fn on_event(&mut self, event: &AgentEvent) {
        self.on_event_at(event, Instant::now());
    }

    /// Takes in an event from the agent that came at `now`.
    pub fn on_event_at(&mut self, event: &AgentEvent, now: Instant) {
        match event {
            AgentEvent::TurnStarted => {
                self.last_reply.clear();
                self.turn_started = Some(now);
            }
            AgentEvent::AssistantMessage { content, .. } if !content.trim().is_empty() => {
                self.last_reply = content.clone();
            }
            AgentEvent::TurnFinished { reason } => self.turn_ended(*reason, now),
            AgentEvent::Usage { model, usage } => self.totals.add(model, usage),
            AgentEvent::Steered { text } => {
                if let Some(i) = self.sent_now.iter().position(|t| t == text) {
                    self.sent_now.remove(i);
                }
            }
            _ => {}
        }
        self.transcript.on_event(event, self.width);
    }

    /// A turn ended. Send-now input it did not take is sent next, before queued input. After an
    /// interruption, both go back into the editor instead, for the user to look at again.
    fn turn_ended(&mut self, reason: TurnEndReason, now: Instant) {
        self.running = false;
        if let Some(started) = self.turn_started.take() {
            let took = now.saturating_duration_since(started);
            let how = match reason {
                TurnEndReason::Completed => Some("the turn finished"),
                TurnEndReason::Error => Some("the turn stopped with an error"),
                TurnEndReason::StepLimit => Some("the turn stopped at the step limit"),
                TurnEndReason::Interrupted => None,
            };
            if let Some(how) = how.filter(|_| took >= notify::LONG_TURN) {
                self.notifications
                    .push(format!("{how} after {}", notify::duration(took)));
            }
        }
        if reason == TurnEndReason::Completed
            && self.mode == Mode::Plan
            && !self.last_reply.trim().is_empty()
        {
            self.plan_choice = Some(PlanChoice {
                plan: self.last_reply.clone(),
                edited: false,
            });
            self.arming = Arming::default();
        }
        let left = self.steering.take();
        self.sent_now.clear();
        if reason == TurnEndReason::Interrupted {
            let mut parts = left;
            parts.extend(self.queued.drain(..).map(|(_, full)| full));
            if !parts.is_empty() {
                if !self.editor.is_empty() {
                    parts.push(self.editor.expanded());
                }
                self.editor.set_text(&parts.join("\n\n"));
            }
        } else {
            for text in left.into_iter().rev() {
                self.queued.push_front((text.clone(), text));
            }
        }
    }

    /// The next queued input, once no turn runs.
    pub fn next_queued(&mut self) -> Option<Action> {
        if self.busy() || self.prompt.is_some() || self.plan_choice.is_some() {
            return None;
        }
        let (shown, full) = self.queued.pop_front()?;
        self.send(shown, full)
    }

    /// Where the next request's tokens go, now.
    pub fn set_context(&mut self, context: ContextUsage) {
        self.context = context;
    }

    /// Asks for a turn. In a session that started in plan mode, the agent is told to plan first.
    fn run(&mut self, input: TurnInput) -> Option<Action> {
        self.running = true;
        if std::mem::take(&mut self.plan_note_pending) && self.mode == Mode::Plan {
            return Some(Action::RunIn(Mode::Plan, input));
        }
        Some(Action::Run(input))
    }

    /// The plan waiting for the user's choice.
    pub fn plan_choice(&self) -> Option<&PlanChoice> {
        self.plan_choice.as_ref()
    }

    /// A plan choice was made.
    fn choose(&mut self, choice: Choice) -> Option<Action> {
        let width = self.width;
        match choice {
            Choice::KeepPlanning => {
                self.plan_choice = None;
                self.transcript
                    .push_note("still planning: say what to change", width);
                self.next_queued()
            }
            Choice::Edit => {
                let plan = self.plan_choice.as_ref()?.plan.clone();
                Some(Action::EditPlan(plan))
            }
            Choice::Build => {
                let choice = self.plan_choice.take()?;
                let mode = self.mode_before_plan.take().unwrap_or(self.default_mode);
                self.mode = mode;
                self.transcript
                    .push_note(&format!("switched to {mode} mode"), width);
                self.transcript.push_user("Build the plan", width);
                self.running = true;
                let input = TurnInput {
                    parts: vec![harness_core::turn::InputPart::Text(choice.build_message())],
                    display: Some("Build the plan".into()),
                    plan: Some(choice.plan),
                    ..TurnInput::default()
                };
                Some(Action::RunIn(mode, input))
            }
        }
    }

    /// The plan came back from the user's editor.
    pub fn plan_edited(&mut self, edited: std::io::Result<String>) {
        let width = self.width;
        match edited {
            Ok(plan) => {
                self.transcript.push_note("the edited plan:", width);
                let theme = self.theme();
                self.transcript
                    .push_lines(crate::markdown::render(&plan, width, &theme), width);
                self.plan_choice = Some(PlanChoice { plan, edited: true });
                self.arming = Arming::default();
            }
            Err(e) => self
                .transcript
                .push_error(&format!("could not edit the plan: {e}"), width),
        }
    }

    /// Takes in a paste: into the reason for a denial being typed, else into the input.
    pub fn on_paste(&mut self, text: &str) {
        if let Some(prompt) = &mut self.prompt
            && prompt.paste(text)
        {
            return;
        }
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
        if self.prompt.is_some() || self.plan_choice.is_some() {
            if self.arming.armed(now) {
                return self.prompt_key(key);
            }
            // Esc stops the turn, as it does without the prompt: the approval is denied.
            if key.code == KeyCode::Esc && self.prompt.is_some() {
                self.answer(Answered::Interrupt);
                return Some(Action::Interrupt);
            }
            // Until the prompt takes keys, they were typed for the input.
        }
        if ctrl && key.code == KeyCode::Char('d') && self.editor.is_empty() {
            return self.quit();
        }
        if ctrl && key.code == KeyCode::Char('s') {
            return self.send_now();
        }
        if let Some(action) = self.completion_key(key) {
            return action;
        }
        match key.code {
            KeyCode::Esc if self.busy() => return Some(Action::Interrupt),
            KeyCode::Esc => return None,
            KeyCode::BackTab => return self.cycle_mode(),
            _ => {}
        }
        match self.editor.key(key) {
            Edit::Submit => return self.submit(),
            Edit::Handled => self.update_completion(),
            Edit::Ignored => {}
        }
        None
    }

    /// A key for the approval or the plan choice, once it takes keys.
    fn prompt_key(&mut self, key: KeyEvent) -> Option<Action> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
            self.hint = Some(if self.prompt.is_some() {
                "answer the approval first: nothing is sent while it waits".into()
            } else {
                "choose b, e or k first: nothing is sent while the plan waits".into()
            });
            return None;
        }
        if let Some(prompt) = &mut self.prompt {
            let answered = prompt.key(key)?;
            let interrupt = answered == Answered::Interrupt;
            self.answer(answered);
            return interrupt.then_some(Action::Interrupt);
        }
        let choice = self.plan_choice.as_ref()?.key(key)?;
        self.choose(choice)
    }

    /// Shift+Tab: the next of plan, ask and auto, now, or when the running turn ends.
    fn cycle_mode(&mut self) -> Option<Action> {
        let next = next_mode(self.pending_mode.unwrap_or(self.mode));
        if self.busy() {
            self.pending_mode = (next != self.mode).then_some(next);
            return None;
        }
        self.switch_mode(next)
    }

    fn switch_mode(&mut self, mode: Mode) -> Option<Action> {
        if mode == Mode::Plan && self.mode != Mode::Plan {
            self.mode_before_plan = Some(self.mode);
        } else if mode != Mode::Plan {
            self.mode_before_plan = None;
        }
        self.plan_note_pending = false;
        self.mode = mode;
        self.transcript
            .push_note(&format!("switched to {mode} mode"), self.width);
        if matches!(mode, Mode::Ask | Mode::Auto)
            && let Some(warning) = self.write_mode_warning.take()
        {
            self.transcript.push_warning(&warning, self.width);
        }
        // Leaving plan mode, as the user chose during the planning turn or before the plan took
        // keys, leaves its plan: Build would switch to a mode other than the one they chose.
        if mode != Mode::Plan && self.plan_choice.take().is_some() {
            self.transcript.push_note(
                "the plan is left unbuilt: you left plan mode; say what to do next",
                self.width,
            );
        }
        Some(Action::SetMode(mode))
    }

    /// The mode chosen during the turn that just ended, to switch to now.
    pub fn take_pending_mode(&mut self) -> Option<Action> {
        if self.busy() {
            return None;
        }
        let mode = self.pending_mode.take()?;
        self.switch_mode(mode)
    }

    /// Ctrl+C: interrupts a running turn, or clears the input; pressed again within
    /// [`QUIT_WINDOW`], it exits.
    fn ctrl_c(&mut self, now: Instant) -> Option<Action> {
        if self
            .ctrl_c
            .is_some_and(|at| now.duration_since(at) <= QUIT_WINDOW)
        {
            return self.quit();
        }
        self.ctrl_c = Some(now);
        self.hint = Some("press Ctrl+C again to exit".into());
        if self.prompt.is_some() {
            self.answer(Answered::Interrupt);
        }
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

    /// Enter: runs a built-in command now, and sends the input, or queues it while a turn runs.
    fn submit(&mut self) -> Option<Action> {
        let full = self.editor.expanded();
        if full.trim().is_empty() {
            return None;
        }
        self.completion = None;
        if let Some(invocation) = parse_invocation(&full)
            && !self.starts_a_turn(invocation.name)
        {
            let name = invocation.name.to_string();
            return self.builtin(&name, &full);
        }
        let (shown, full) = self.editor.submit();
        // Input typed ahead of a plan choice waits for it, as input typed during a turn does.
        if self.busy() || self.plan_choice.is_some() {
            self.queued.push_back((shown, full));
            return None;
        }
        self.send(shown, full)
    }

    /// Ctrl+S: while a turn runs, gives the input to the model with the next tool results.
    fn send_now(&mut self) -> Option<Action> {
        if !self.busy() {
            return self.submit();
        }
        let full = self.editor.expanded();
        if full.trim().is_empty() {
            return None;
        }
        if parse_invocation(&full).is_some() {
            self.hint =
                Some("a command cannot be sent during a turn: press Enter to queue it".into());
            return None;
        }
        let (_, full) = self.editor.submit();
        self.steering.send(full.clone());
        self.sent_now.push(full);
        None
    }

    /// Whether `/name` starts a turn: `/init` and custom commands.
    fn starts_a_turn(&self, name: &str) -> bool {
        name == "init" || (!is_builtin(name) && self.host.is_command(name))
    }

    /// A built-in command that runs here, without a turn, or an unknown one.
    fn builtin(&mut self, name: &str, full: &str) -> Option<Action> {
        let width = self.width;
        match name {
            "quit" => return self.quit(),
            "help" => {
                self.editor.submit();
                self.transcript.push_user(full, width);
                self.help();
            }
            "context" | "usage" => {
                self.editor.submit();
                self.transcript.push_user(full, width);
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
            }
            _ => {
                if let Some((_, when)) = LATER.iter().find(|(n, _)| *n == name) {
                    self.editor.submit();
                    self.transcript.push_user(full, width);
                    self.transcript.push_note(
                        &format!("/{name} is not available yet: it comes {when}."),
                        width,
                    );
                } else {
                    self.transcript.push_error(
                        &format!(
                            "unknown command /{name}; custom commands are Markdown files in .harness/commands, .claude/commands or .opencode/commands; to send text that starts with /, put a word before it"
                        ),
                        width,
                    );
                }
            }
        }
        None
    }

    /// Sends input typed as `shown`, `full` with pastes expanded: a custom command or `/init`
    /// expanded, anything else as it is.
    fn send(&mut self, shown: String, full: String) -> Option<Action> {
        let width = self.width;
        if let Some(invocation) = parse_invocation(&full)
            && self.starts_a_turn(invocation.name)
        {
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
            ("Enter during a turn", "send the input when the turn ends"),
            ("Ctrl+S during a turn", "send it with the next tool results"),
            ("Shift+Tab", "switch between plan, ask and auto mode"),
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
        let mut line = status::status_line(
            &self.model,
            self.mode,
            &self.context,
            &self.totals,
            &self.theme(),
        );
        if let Some(next) = self.pending_mode {
            line.spans.push(Span::styled(
                format!(" · {next} mode after this turn"),
                self.theme().accent(),
            ));
        }
        line
    }

    /// The live region's lines, at most `rows`, and where the cursor is in them (none while an
    /// approval waits).
    pub fn live(&self, rows: usize) -> (Vec<Line<'static>>, Option<Position>) {
        let theme = self.theme();
        let width = self.width;
        let mut below: Vec<Line<'static>> = Vec::new();
        below.extend(wrap(&self.status(), width, &[], &[]));
        if let Some(hint) = self
            .hint
            .as_ref()
            .filter(|_| self.prompt.is_some() || self.plan_choice.is_some())
        {
            below.push(Line::from(Span::styled(sanitize(hint), theme.dim())));
        }
        if let Some(prompt) = &self.prompt {
            let mut lines = prompt.render(width, rows.saturating_sub(below.len()), &theme);
            lines.extend(below);
            let skip = lines.len().saturating_sub(rows);
            return (lines.split_off(skip), None);
        }
        if let Some(choice) = &self.plan_choice {
            let mut lines = choice.render(width, &theme);
            lines.extend(below);
            let skip = lines.len().saturating_sub(rows);
            return (lines.split_off(skip), None);
        }
        below.clear();
        let (mut editor, mut cursor) = self.editor.render("› ", width, &theme);
        // What waits to be sent, above the input.
        let mut waiting: Vec<Line<'static>> = Vec::new();
        for text in &self.sent_now {
            waiting.push(pending_line(
                "sending with the next tool results: ",
                text,
                &theme,
            ));
        }
        for (shown, _) in &self.queued {
            waiting.push(pending_line("queued: ", shown, &theme));
        }
        cursor.y += waiting.len() as u16;
        waiting.append(&mut editor);
        let editor = waiting;
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
        (lines.split_off(skip), Some(cursor))
    }
}

/// The mode Shift+Tab switches to from `mode`: plan, ask and auto in turn. From read-only it
/// goes to ask, and from full-access to plan; it never goes to either.
pub fn next_mode(mode: Mode) -> Mode {
    match mode {
        Mode::Plan | Mode::ReadOnly => Mode::Ask,
        Mode::Ask => Mode::Auto,
        Mode::Auto | Mode::FullAccess => Mode::Plan,
    }
}

/// One line for input waiting to be sent: `label` and the input's first line.
fn pending_line(label: &str, text: &str, theme: &Theme) -> Line<'static> {
    let first = sanitize(text.lines().next().unwrap_or_default());
    let more = if text.lines().count() > 1 { " …" } else { "" };
    Line::from(vec![
        Span::styled(label.to_string(), theme.dim()),
        Span::raw(format!("{first}{more}")),
    ])
}
