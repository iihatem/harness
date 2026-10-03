//! What the interactive session shows and how it reacts to keys, apart from the terminal and
//! the running agent: the transcript, the input editor with completion, and the status line.
//! Keys and events go in; lines for the scrollback, the live region, and actions for the
//! session to carry out come out.

use std::{
    collections::VecDeque,
    sync::Arc,
    time::{Duration, Instant},
};

use futures::future::BoxFuture;
use harness_context::commands::{is_builtin, parse_invocation};
use harness_core::{
    agent::{
        ApprovalDecision, ApprovalRequest, ContextUsage, REWIND_LIMITS, RewindPoint, SessionModel,
    },
    checkpoint::Checkpoints,
    event::{AgentEvent, TurnEndReason},
    message::Message,
    meter::WindowSnapshot,
    permission::Mode,
    redact::Redactor,
    session::{RewindScope, Session, SessionSummary},
    turn::{Steering, TurnInput},
};
use ratatui::{
    crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    layout::Position,
    text::{Line, Span},
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{
    approval::{Answered, Arming, Prompt, Reply},
    complete::{self, Completer, Offer},
    editor::{Edit, Editor},
    notify::{self, Notify},
    picker::{Item, Picked, Picker, model_item},
    plan::{Choice, PlanChoice, TextEditor},
    status::{self, Costs, Totals},
    style::Theme,
    text::{sanitize, wrap},
    transcript::Transcript,
    usage::UsageContext,
};

/// Ctrl+C twice within this long exits.
pub const QUIT_WINDOW: Duration = Duration::from_secs(2);

/// Said under a prompt once keys typed while it waited went to the input instead.
const TYPED_PAST: &str = "your typing went to your message; the prompt takes keys once you pause";

/// What the model picker says when no model is found.
const NO_MODELS: &str = "no models found: start Ollama, LM Studio or llama.cpp, add a provider's API key with `harness auth add <provider>`, or sign in to ChatGPT with `/login` (then /model offers its models), or name one with `/model chatgpt/<model>`";

/// Esc twice on empty input within this long opens the rewind list.
pub const REWIND_WINDOW: Duration = Duration::from_secs(1);

/// What the rewind's second list offers, in order.
const SCOPES: [(RewindScope, &str, &str); 3] = [
    (
        RewindScope::CodeAndConversation,
        "code and conversation",
        "the files and the conversation as they were",
    ),
    (
        RewindScope::Conversation,
        "conversation only",
        "the files stay as they are now",
    ),
    (
        RewindScope::Code,
        "code only",
        "the conversation stays as it is now",
    ),
];

/// The modes `/mode` offers, and what each lets the agent do. `full-access` is chosen only when
/// harness starts.
const MODES: [(Mode, &str); 4] = [
    (
        Mode::Plan,
        "read-only; ends with a plan to build, edit, or keep planning",
    ),
    (
        Mode::ReadOnly,
        "reads, and runs commands that change nothing",
    ),
    (Mode::Ask, "asks before edits, and commands no rule allows"),
    (
        Mode::Auto,
        "edits the workspace and runs sandboxed commands",
    ),
];

/// What a picker is choosing.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Pick {
    Mode,
    /// The message to rewind to, one per item: `None` undoes the last rewind.
    Rewind(Vec<Option<RewindPoint>>),
    /// The session to resume, one id per item.
    Session(Vec<String>),
    /// The model to switch to, one id per item.
    Model(Vec<String>),
    /// What to restore to before this message.
    RewindScope(RewindPoint),
}

/// How many times the countdown to an automatic resume is armed again when the window still shows
/// no capacity at the reset.
const MAX_REARMS: u8 = 2;

/// The message sent when the session resumes by itself.
pub const RESUME_MESSAGE: &str = "Continue where you left off.";

/// Where waiting out a subscription limit stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Resume {
    /// The user is asked, once, whether to resume at the reset.
    Asking { resets_at: u64 },
    /// Counting down to `at`, `rearms` times armed again so far.
    Waiting { at: u64, rearms: u8 },
    /// At the reset: the window is being read.
    Checking { rearms: u8 },
}

/// How work the agent did for a command ended.
#[derive(Debug)]
pub enum Done {
    /// `/compact`: `Err` says why nothing was compacted.
    Compacted(Result<(), String>),
    /// A rewind: `Err` says why it failed.
    Rewound(Result<(), String>),
    /// Undoing the last rewind.
    UndidRewind(Result<(), String>),
    /// A new session (`/new`), or another one (`/resume`), that the agent continues in now.
    Session {
        resumed: bool,
        result: Result<SessionView, String>,
    },
    /// `/model`: the model the session continues on, or why it could not switch.
    Model(Result<ModelView, String>),
    /// `/login`: what to tell the user, or why it did not sign in.
    LoggedIn(Result<String, String>),
}

/// A session the agent is to continue in, as the host opened it.
pub struct OpenedSession {
    pub session: Session,
    /// Its checkpoints; `None` when they cannot work here.
    pub checkpoints: Option<Arc<Checkpoints>>,
    /// What opening it had to warn about.
    pub warnings: Vec<String>,
}

/// A session the agent continues in now, for the app to show.
#[derive(Debug, Clone)]
pub struct SessionView {
    pub id: String,
    /// Its conversation, oldest first.
    pub history: Vec<Message>,
}

/// A model made ready for the session by the host: the agent switches to it.
pub struct ModelSwitch {
    pub model: SessionModel,
    /// Where its context window comes from, for `/context`.
    pub window_note: String,
    /// What to warn about, such as a window too small for agentic work.
    pub warnings: Vec<String>,
}

/// The model the session continues on now, for the app to show.
#[derive(Debug, Clone)]
pub struct ModelView {
    pub id: String,
    pub window_note: String,
}

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
    /// This project's sessions, the most recently used first.
    fn sessions(&self) -> Vec<SessionSummary> {
        Vec::new()
    }
    /// Opens a new session (`None`), or this project's session `id`, for the agent to continue
    /// in. Errors say why it cannot be.
    ///
    /// Reading a session and opening its checkpoints take time, which does not block the
    /// session's loop: the future runs in the background. `cancel` stops it (Esc).
    fn open_session(
        &self,
        _id: Option<&str>,
        _cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<OpenedSession, String>> {
        Box::pin(async { Err("this session cannot change".to_string()) })
    }
    /// The first run's model answered a request, so it can be kept as the default (what it
    /// saved, or why it could not, is returned as notes to show).
    fn model_answered(&self, _model: &str) -> Vec<String> {
        Vec::new()
    }
    /// The ids of the models harness finds: local servers', and those of providers with a key
    /// or a sign-in.
    fn models(&self) -> BoxFuture<'static, Vec<String>> {
        Box::pin(async { Vec::new() })
    }
    /// Makes model `id` ready for the session: its provider, and what its profile and window
    /// say. Errors say why it cannot be used. `cancel` stops it while a local server is asked
    /// for its window.
    fn switch_model(
        &self,
        _id: &str,
        _cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<ModelSwitch, String>> {
        Box::pin(async { Err("the model cannot change".into()) })
    }
    /// The settings `/usage` shows beside what the session knows.
    fn usage_context(&self) -> UsageContext {
        UsageContext::default()
    }
    /// The ledger's report for `/usage <args>` (a group, `model`, `provider`, `day` or `project`,
    /// and a period), as lines; none when there is no ledger to report.
    fn usage_report(&self, _args: &str) -> BoxFuture<'static, Vec<String>> {
        Box::pin(async { Vec::new() })
    }
    /// The budgets with what is spent against them, in `session`; with `set`, first sets the
    /// session budget to that many USD (for this session only). Errors say why not.
    fn budget(
        &self,
        _session: &str,
        _set: Option<f64>,
    ) -> BoxFuture<'static, Result<Vec<String>, String>> {
        Box::pin(async { Err("there are no budgets here".to_string()) })
    }
    /// Signs in to `provider`, with a device code when `device` is set: what the user must do
    /// (the address to open, the code) goes to `notes` as it comes, and `cancel` stops it. Ok:
    /// what to tell the user; errors say why not.
    fn login(
        &self,
        _provider: &str,
        _device: bool,
        _notes: mpsc::UnboundedSender<String>,
        _cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<String, String>> {
        Box::pin(async { Err("sign-in is not available here".into()) })
    }
}

/// A model that has not answered yet: its id cannot be checked offline, so a wrong one shows up
/// as a refused request.
struct Unproven {
    id: String,
    /// The model before a `/model` switch, to offer the way back.
    previous: Option<String>,
    /// The first run's choice, kept as the default only once it has answered.
    first_run: bool,
}

/// Whether `message`, of a failed request, says the model is the problem: it is not found, or the
/// request is refused with a 4xx other than a key, a timeout or a rate limit.
fn refuses_the_model(message: &str) -> bool {
    let lower = message.to_lowercase();
    if lower.contains("not found")
        || lower.contains("model_not_found")
        || lower.contains("does not exist")
    {
        return true;
    }
    let Some(at) = message
        .find("HTTP ")
        .map(|i| i + 5)
        .or_else(|| message.find("reported ").map(|i| i + 9))
    else {
        return false;
    };
    let status: String = message[at..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    matches!(status.parse::<u16>(), Ok(400..=499))
        && !matches!(status.as_str(), "401" | "403" | "408" | "429")
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
    /// Compact the conversation, with what to keep in particular.
    Compact(Option<String>),
    /// Rewind to just before the user message `entry`.
    Rewind { entry: String, scope: RewindScope },
    /// Undo the last rewind.
    UndoRewind,
    /// Continue in a new session (`None`), or in this project's session with this id.
    OpenSession(Option<String>),
    /// Ask the provider where its usage windows stand, and the host for the ledger's report, for
    /// `/usage` with these arguments.
    Usage(String),
    /// Show the budgets, or raise the session's (`/budget <usd>`), in this session.
    Budget { session: String, set: Option<f64> },
    /// Look for the models, for the model picker.
    ListModels,
    /// Continue on the model with this id.
    SwitchModel(String),
    /// Sign in to a provider, with a device code when `device` is set.
    Login { provider: String, device: bool },
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
    /// The model the session uses, until it has answered once.
    unproven: Option<Unproven>,
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
    /// Whether the running turn has requested any tool call: files it wrote or created should
    /// be offered by `@` completion once it ends.
    ran_tools: bool,
    /// A picker the user is choosing in, drawn in a full-screen view, and what for.
    picker: Option<(Pick, Picker)>,
    /// What the agent is doing for a command, shown until it is done.
    working: Option<String>,
    /// Whether Esc stops what `working` says (a rewind runs to its end).
    working_stoppable: bool,
    /// The user messages the conversation can be rewound to, oldest first, and whether the last
    /// rewind can be undone, as the agent last said.
    rewind_points: Vec<RewindPoint>,
    can_undo_rewind: bool,
    /// The rewind asked for: to before which message, restoring what.
    rewinding: Option<(String, RewindScope)>,
    /// When Esc was last pressed on empty input.
    last_esc: Option<Instant>,
    /// The id of the session the agent continues in.
    session_id: String,
    /// The mode the session started in, which the system prompt describes.
    start_mode: Mode,
    /// The agent continues in another session, whose conversation may not say the mode: the
    /// next turn tells the model.
    mode_note_pending: bool,
    /// The model being switched to.
    switching: Option<String>,
    /// What the session's requests cost, from the runtime's metered events.
    costs: Costs,
    /// Where the subscription's usage windows stand, as last said.
    windows: Option<WindowSnapshot>,
    /// How far each window's warnings went: 0, 80 or 95 (percent), by window length and reset.
    window_warnings: std::collections::HashMap<(Option<u64>, Option<u64>), u8>,
    usage_context: UsageContext,
    /// The usage limit that ended the running turn, and when it resets.
    limit_reset: Option<u64>,
    /// Waiting out a usage limit, if the session is.
    resume: Option<Resume>,
    /// The user's answer to the offer to resume automatically, for the session.
    resume_answer: Option<bool>,
    /// The time, in seconds since the Unix epoch.
    clock: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl App {
    pub fn new(options: Options, host: Box<dyn Host>, width: usize) -> App {
        App {
            transcript: Transcript::new(options.theme),
            editor: Editor::new(options.history),
            completer: Completer::new(options.commands, &options.workspace),
            completion: None,
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
            unproven: None,
            turn_started: None,
            notifications: Vec::new(),
            workspace: options.workspace,
            width,
            redactor: None,
            write_mode_warning: None,
            ran_tools: false,
            picker: None,
            working: None,
            working_stoppable: true,
            rewind_points: Vec::new(),
            can_undo_rewind: false,
            rewinding: None,
            last_esc: None,
            session_id: String::new(),
            start_mode: options.mode,
            mode_note_pending: false,
            switching: None,
            usage_context: host.usage_context(),
            limit_reset: None,
            resume: None,
            resume_answer: None,
            costs: Costs::default(),
            windows: None,
            window_warnings: std::collections::HashMap::new(),
            clock: Arc::new(harness_core::time::now_unix),
            host,
        }
    }

    /// Reads the time from `clock`, for the windows' staleness and reset times.
    pub fn set_clock(&mut self, clock: Arc<dyn Fn() -> u64 + Send + Sync>) {
        self.clock = clock;
    }

    /// Sets what `/usage` shows beside the session's own figures.
    pub fn set_usage_context(&mut self, context: UsageContext) {
        self.usage_context = context;
    }

    /// Whether the usage windows are known.
    pub fn has_windows(&self) -> bool {
        self.windows.is_some()
    }

    /// Where the usage windows stand, as the provider said: shown in the status line, and a
    /// warning once as each window crosses 80% and once as it crosses 95%. Nothing here
    /// delays or refuses a request.
    fn observe_windows(&mut self, snapshot: WindowSnapshot) {
        let now = (self.clock)();
        for window in &snapshot.windows {
            let Some(used) = window.used_percent else {
                continue;
            };
            let key = (window.window_minutes, window.resets_at);
            let reached = self.window_warnings.entry(key).or_insert(0);
            let level = if used >= 95.0 {
                95
            } else if used >= 80.0 {
                80
            } else {
                0
            };
            if level > *reached {
                *reached = level;
                let resets = window.resets_at.map_or(String::new(), |r| {
                    format!(", resets {}", status::reset_text(r, now))
                });
                self.transcript.push_warning(
                    &format!("{} window at {used:.0}% used{resets}", window.label()),
                    self.width,
                );
            }
        }
        self.windows = Some(snapshot);
    }

    /// The usage windows the provider just gave when asked, and the ledger's report, for
    /// `/usage`.
    pub fn on_usage(
        &mut self,
        windows: Option<Result<WindowSnapshot, String>>,
        ledger: Vec<String>,
    ) {
        let width = self.width;
        let theme = self.theme();
        match windows {
            Some(Ok(snapshot)) => {
                self.observe_windows(snapshot);
                let now = (self.clock)();
                let lines = status::window_lines(self.windows.as_ref(), now, &theme);
                self.transcript.push_lines(lines, width);
            }
            Some(Err(why)) => {
                let why = self.redacted(&why);
                self.transcript
                    .push_note(&format!("could not read the usage windows: {why}"), width);
            }
            None => {}
        }
        if !ledger.is_empty() {
            let lines = ledger
                .iter()
                .map(|l| Line::from(sanitize(l)))
                .collect::<Vec<_>>();
            self.transcript.push_lines(lines, width);
        }
    }

    /// A usage limit ended the turn, and resets at `resets_at`: offers to wait for it (once, then
    /// as the user answered), unless `usage.auto_resume` is `never` or it has reset already.
    fn limit_hit(&mut self, resets_at: u64) {
        let now = (self.clock)();
        if self.usage_context.auto_resume == crate::usage::AutoResume::Never || resets_at <= now {
            return;
        }
        match self.resume_answer {
            Some(false) => {}
            Some(true) => {
                self.resume = Some(Resume::Waiting {
                    at: resets_at,
                    rearms: 0,
                })
            }
            None => {
                self.resume = Some(Resume::Asking { resets_at });
                let at = status::reset_text(resets_at, now);
                self.transcript
                    .push_note(&format!("Resume automatically at {at}? (y/n)"), self.width);
            }
        }
    }

    /// The offer's answer: `y` waits for the reset, anything else declines, for the session.
    fn answer_resume(&mut self, accept: bool) {
        let Some(Resume::Asking { resets_at }) = self.resume else {
            return;
        };
        self.resume_answer = Some(accept);
        if accept {
            self.resume = Some(Resume::Waiting {
                at: resets_at,
                rearms: 0,
            });
        } else {
            self.resume = None;
            self.transcript
                .push_note("not resuming automatically", self.width);
        }
    }

    /// Whether the session counts down to an automatic resume, which the screen shows second by
    /// second.
    pub fn resume_waiting(&self) -> bool {
        matches!(
            self.resume,
            Some(Resume::Waiting { .. } | Resume::Asking { .. })
        )
    }

    /// Whether the reset has come: the window is to be read now (once; the answer comes to
    /// [`on_resume_check`](Self::on_resume_check)).
    pub fn resume_due(&mut self) -> bool {
        match self.resume {
            Some(Resume::Waiting { at, rearms }) if !self.busy() && (self.clock)() >= at => {
                self.resume = Some(Resume::Checking { rearms });
                true
            }
            _ => false,
        }
    }

    /// The window as the provider just said it at the reset (or why it could not): with capacity,
    /// the session continues with a fixed message; without, the countdown is armed again, at
    /// most twice, and then stops.
    pub fn on_resume_check(&mut self, windows: Result<WindowSnapshot, String>) -> Option<Action> {
        let Some(Resume::Checking { rearms }) = self.resume else {
            return None;
        };
        let now = (self.clock)();
        let width = self.width;
        let snapshot = windows.as_ref().ok().cloned();
        let capacity = snapshot.as_ref().is_some_and(|s| {
            s.windows.iter().any(|w| w.used_percent.is_some())
                && s.windows
                    .iter()
                    .all(|w| w.used_percent.is_none_or(|u| u < 100.0))
        });
        if let Some(snapshot) = snapshot.clone() {
            self.observe_windows(snapshot);
        }
        if capacity {
            self.resume = None;
            self.transcript
                .push_note("the usage window has capacity again: continuing", width);
            return self.send(RESUME_MESSAGE.to_string(), RESUME_MESSAGE.to_string());
        }
        if rearms >= MAX_REARMS {
            self.resume = None;
            self.transcript.push_note(
                "the usage window is still full after waiting twice more: not resuming automatically",
                width,
            );
            return None;
        }
        // When the full window says it resets, else a few minutes from now.
        let next = snapshot
            .iter()
            .flat_map(|s| s.windows.iter())
            .filter(|w| w.used_percent.is_some_and(|u| u >= 100.0))
            .filter_map(|w| w.resets_at)
            .filter(|at| *at > now)
            .max()
            .unwrap_or(now + 300);
        let why = match &windows {
            Err(why) => format!("could not read the usage window ({}); ", self.redacted(why)),
            Ok(_) => "the usage window is still full; ".to_string(),
        };
        self.transcript.push_note(
            &format!("{why}trying again at {}", status::reset_text(next, now)),
            width,
        );
        self.resume = Some(Resume::Waiting {
            at: next,
            rearms: rearms + 1,
        });
        None
    }

    /// The live region's line for an offer or a countdown.
    fn resume_line(&self, theme: &Theme) -> Option<Line<'static>> {
        let now = (self.clock)();
        let text = match self.resume? {
            Resume::Asking { .. } => "answer y or n to resume automatically".to_string(),
            Resume::Waiting { at, .. } => {
                let left = at.saturating_sub(now);
                format!(
                    "Resuming automatically at {} (in {}m {:02}s) · Esc to cancel",
                    status::reset_text(at, now),
                    left / 60,
                    left % 60
                )
            }
            Resume::Checking { .. } => "Reading the usage window… · Esc to cancel".to_string(),
        };
        Some(Line::from(Span::styled(text, theme.accent())))
    }

    /// What `/budget` found out: the budgets and their spend, or why not.
    pub fn on_budget(&mut self, result: Result<Vec<String>, String>) {
        let width = self.width;
        match result {
            Ok(lines) => {
                let lines = lines
                    .iter()
                    .map(|l| Line::from(sanitize(l)))
                    .collect::<Vec<_>>();
                self.transcript.push_lines(lines, width);
            }
            Err(why) => {
                let why = self.redacted(&why);
                self.transcript.push_error(&why, width);
            }
        }
    }

    /// The windows the provider gave when the session asked at its start (or after a model
    /// switch). One that could not be read says nothing: `/usage` says why.
    pub fn on_windows(&mut self, windows: Result<WindowSnapshot, String>) {
        if let Ok(snapshot) = windows {
            self.observe_windows(snapshot);
        }
    }

    /// The models the host found, for the model picker if it is open.
    pub fn on_models(&mut self, ids: Vec<String>) {
        let Some((Pick::Model(listed), picker)) = &mut self.picker else {
            return;
        };
        let items = ids
            .iter()
            .map(|id| model_item(id, *id == self.model))
            .collect();
        picker.set_items(items);
        if let Some(current) = ids.iter().position(|id| *id == self.model) {
            picker.select(current);
        }
        *listed = ids;
        // The list that arrived takes keys once the user has paused, as the picker did when it
        // opened: a key typed as it appears never chooses in it.
        self.arming = Arming::default();
    }

    /// The id of the session the agent continues in.
    pub fn set_session_id(&mut self, id: &str) {
        self.session_id = id.to_string();
    }

    /// What the conversation can be rewound to: `points`, oldest first, and whether the last
    /// rewind can be undone.
    pub fn set_rewind(&mut self, points: Vec<RewindPoint>, can_undo: bool) {
        self.rewind_points = points;
        self.can_undo_rewind = can_undo;
    }

    /// The picker the user is choosing in: the session draws it in a full-screen view.
    pub fn picker(&self) -> Option<&Picker> {
        self.picker.as_ref().map(|(_, picker)| picker)
    }

    /// Says what the session is busy with, until [`on_done`](Self::on_done); `stoppable` says
    /// whether Esc stops it.
    fn work(&mut self, what: &str, stoppable: bool) {
        self.working = Some(what.to_string());
        self.working_stoppable = stoppable;
    }

    /// Work the agent did for a command ended.
    pub fn on_done(&mut self, done: Done) {
        self.working = None;
        let width = self.width;
        match done {
            Done::Compacted(Ok(())) => {}
            Done::Compacted(Err(why)) => self
                .transcript
                .push_note(&format!("the conversation was not compacted: {why}"), width),
            Done::Rewound(result) => {
                let Some((text, scope)) = self.rewinding.take() else {
                    return;
                };
                match result {
                    Ok(()) => {
                        let what = match scope {
                            RewindScope::CodeAndConversation => "the code and the conversation",
                            RewindScope::Conversation => "the conversation",
                            RewindScope::Code => "the code",
                        };
                        self.transcript.push_note(
                            &format!("rewound {what} to before: {}", first_line(&text)),
                            width,
                        );
                        // The message comes back, to send again as it is or changed.
                        // What the user typed meanwhile is not replaced: the message is kept in
                        // the history instead, for Up.
                        if scope != RewindScope::Code {
                            self.drop_pending_input();
                            if self.editor.is_empty() {
                                self.editor.set_text(&text);
                            } else {
                                self.editor.remember(&text);
                                self.transcript.push_note(
                                    "the message is in your history (Up), as you had typed another",
                                    width,
                                );
                            }
                        }
                    }
                    Err(why) => self
                        .transcript
                        .push_error(&format!("the rewind failed: {why}"), width),
                }
            }
            Done::Session { resumed, result } => self.session_started(resumed, result),
            Done::LoggedIn(Ok(done)) => {
                self.transcript.push_note(&done, width);
                self.transcript.push_note(
                    "/model offers ChatGPT's models; /model chatgpt/<model> uses any other",
                    width,
                );
            }
            Done::LoggedIn(Err(why)) => self
                .transcript
                .push_error(&format!("could not sign in: {why}"), width),
            Done::Model(result) => {
                let wanted = self.switching.take().unwrap_or_default();
                match result {
                    Ok(view) => {
                        self.transcript
                            .push_note(&format!("switched to {}", view.id), width);
                        let previous = std::mem::replace(&mut self.model, view.id.clone());
                        self.unproven = Some(Unproven {
                            id: view.id,
                            previous: Some(previous),
                            first_run: false,
                        });
                        self.window_note = Some(view.window_note);
                    }
                    Err(why) => {
                        let text = self.redacted(&format!("could not switch to {wanted}: {why}"));
                        self.transcript.push_error(&text, width);
                    }
                }
            }
            Done::UndidRewind(Ok(())) => self.transcript.push_note("undid the last rewind", width),
            Done::UndidRewind(Err(why)) => self
                .transcript
                .push_error(&format!("could not undo the last rewind: {why}"), width),
        }
    }

    /// What the modes that write (ask, auto) lack, said the first time the session switches to
    /// one: a session that started in another mode was not told as it started.
    pub fn set_write_mode_warning(&mut self, warning: Option<String>) {
        self.write_mode_warning = warning;
    }

    /// The session's model is the first run's choice, which the host keeps as the default once
    /// the model has answered.
    pub fn set_first_run_model(&mut self) {
        self.unproven = Some(Unproven {
            id: self.model.clone(),
            previous: None,
            first_run: true,
        });
    }

    /// Shows approvals with the secrets `redactor` knows replaced.
    pub fn set_redactor(&mut self, redactor: Arc<Redactor>) {
        self.redactor = Some(redactor);
    }

    /// Shows `text`, something the host says, as a note, with the secrets harness knows
    /// replaced.
    pub fn push_note(&mut self, text: &str) {
        let text = self.redacted(text);
        self.transcript.push_note(&text, self.width);
    }

    /// `text` with the secrets harness knows replaced.
    fn redacted(&self, text: &str) -> String {
        match &self.redactor {
            Some(redactor) => redactor.redact(text),
            None => text.to_string(),
        }
    }

    /// What the CLI provides.
    pub fn host(&self) -> &dyn Host {
        &*self.host
    }

    pub fn theme(&self) -> Theme {
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

    /// What a prompt needs, besides its request, to show what it asks about: the workspace, and
    /// the theme.
    pub fn approval_context(&self) -> (std::path::PathBuf, Theme) {
        (self.workspace.clone(), self.theme())
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
    /// time takes keys once the user has paused for
    /// [`ARMING_DELAY`](crate::approval::ARMING_DELAY) since then.
    pub fn drawn(&mut self, now: Instant) {
        if self.takes_keys_after_a_pause() {
            self.arming.drawn(now);
        }
    }

    /// When the approval or plan choice shown takes keys: `None` while none has been drawn.
    pub fn armed_at(&self) -> Option<Instant> {
        self.arming
            .armed_at()
            .filter(|_| self.takes_keys_after_a_pause())
    }

    /// Opens `picker`, drawn in a full-screen view. Every picker, one that follows another
    /// included, takes keys only once the user has paused ([`Arming`]): keys typed ahead never
    /// choose an item or confirm what the picker is for.
    fn open_picker(&mut self, pick: Pick, picker: Picker) {
        self.picker = Some((pick, picker));
        self.arming = Arming::default();
    }

    /// Whether an approval, a plan choice or a picker waits for an answer: it takes keys only
    /// once the user has paused ([`Arming`]), so keys typed ahead never answer it.
    fn takes_keys_after_a_pause(&self) -> bool {
        self.prompt.is_some() || self.plan_choice.is_some() || self.picker.is_some()
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

    /// Whether a turn is running, or about to, or the agent works for a command.
    pub fn busy(&self) -> bool {
        self.running || self.working.is_some()
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
                self.ran_tools = false;
            }
            AgentEvent::AssistantMessage { content, .. } => {
                if !content.trim().is_empty() {
                    self.last_reply = content.clone();
                }
                self.model_proven();
            }
            AgentEvent::ToolCallRequested { .. } => self.ran_tools = true,
            AgentEvent::TurnFinished { reason } => self.turn_ended(*reason, now),
            AgentEvent::Usage { model, usage } => self.totals.add(model, usage),
            AgentEvent::Metered { model, cost } => self.costs.add(model, cost),
            AgentEvent::LimitReached { resets_at } => self.limit_reset = Some(*resets_at),
            AgentEvent::RateLimits { snapshot } => self.observe_windows(snapshot.clone()),
            AgentEvent::Steered { text } => {
                if let Some(i) = self.sent_now.iter().position(|t| t == text) {
                    self.sent_now.remove(i);
                }
            }
            _ => {}
        }
        self.transcript.on_event(event, self.width);
        if let AgentEvent::Error { message, .. } = event {
            self.model_refused(message);
        }
    }

    /// The model answered: the first run's choice is kept as the default.
    fn model_proven(&mut self) {
        let Some(proven) = self.unproven.take() else {
            return;
        };
        if proven.first_run {
            for note in self.host.model_answered(&proven.id) {
                self.push_note(&note);
            }
        }
    }

    /// A request failed before the model had answered once: says so when the model is the
    /// problem.
    fn model_refused(&mut self, message: &str) {
        if !refuses_the_model(message) {
            return;
        }
        let Some(failed) = self.unproven.take() else {
            return;
        };
        let note = match (&failed.previous, failed.first_run) {
            (_, true) => format!(
                "{} did not answer, so it is not saved as your default; /model picks another",
                failed.id
            ),
            (Some(previous), false) => format!(
                "{} did not accept the request; /model {previous} switches back",
                failed.id
            ),
            (None, false) => format!(
                "{} did not accept the request; /model picks another",
                failed.id
            ),
        };
        self.push_note(&note);
    }

    /// A turn ended. Send-now input it did not take is sent next, before queued input. After an
    /// interruption, both go back into the editor instead, for the user to look at again.
    fn turn_ended(&mut self, reason: TurnEndReason, now: Instant) {
        self.running = false;
        // Files a tool wrote or created should be offered by `@` completion from here on: the
        // index built before, or during, this turn may already be stale.
        if std::mem::take(&mut self.ran_tools) {
            self.completer.invalidate_files();
        }
        if let Some(started) = self.turn_started.take() {
            let took = now.saturating_duration_since(started);
            let how = match reason {
                TurnEndReason::Completed => Some("the turn finished"),
                TurnEndReason::Error => Some("the turn stopped with an error"),
                TurnEndReason::StepLimit => Some("the turn stopped at the step limit"),
                TurnEndReason::Budget => Some("the turn stopped at its budget"),
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
        if let Some(resets_at) = self.limit_reset.take()
            && reason == TurnEndReason::Error
        {
            self.limit_hit(resets_at);
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

    /// Asks for a turn. In a session that started in plan mode, the agent is told to plan first;
    /// in a session continued after `/new` or `/resume`, it is told the mode when that may not be
    /// what the conversation says.
    fn run(&mut self, input: TurnInput) -> Option<Action> {
        self.running = true;
        // What the user sends, or the resume itself, ends the wait.
        self.resume = None;
        let plan_note = std::mem::take(&mut self.plan_note_pending) && self.mode == Mode::Plan;
        if std::mem::take(&mut self.mode_note_pending) || plan_note {
            return Some(Action::RunIn(self.mode, input));
        }
        Some(Action::Run(input))
    }

    /// The plan choice, input queued behind it or a turn, and a mode change waiting for the turn
    /// to end belong to the conversation they came from: a new or resumed session, or a rewind
    /// of the conversation, drops them. The draft in the editor stays.
    fn drop_pending_input(&mut self) {
        self.plan_choice = None;
        self.queued.clear();
        self.pending_mode = None;
    }

    /// The agent continues in another session now, or could not.
    fn session_started(&mut self, resumed: bool, result: Result<SessionView, String>) {
        let width = self.width;
        let view = match result {
            Ok(view) => view,
            Err(why) => {
                let what = if resumed {
                    "resume the session"
                } else {
                    "start a new session"
                };
                self.transcript
                    .push_error(&format!("could not {what}: {why}"), width);
                return;
            }
        };
        self.session_id = view.id.clone();
        self.last_reply.clear();
        self.drop_pending_input();
        // Up recalls this session's messages.
        let inputs = self.rewind_points.iter().map(|p| p.text.clone()).collect();
        self.editor.set_history(inputs);
        if !resumed {
            self.transcript
                .push_note("started a new session; /resume goes back to another", width);
            self.mode_note_pending = self.mode != self.start_mode;
            self.plan_note_pending = self.mode == Mode::Plan;
            return;
        }
        self.transcript
            .push_note(&format!("resumed session {}", view.id), width);
        self.recap(&view.history);
        self.mode_note_pending = true;
    }

    /// What a resumed conversation ended with: the last message the user typed and the replies
    /// to it.
    fn recap(&mut self, history: &[Message]) {
        let width = self.width;
        let typed = |m: &Message| matches!(m, Message::User { content } if !content.starts_with("[harness]"));
        let Some(last) = history.iter().rposition(typed) else {
            return;
        };
        let earlier = history[..last].iter().filter(|m| typed(m)).count();
        if earlier > 0 {
            self.transcript.push_note(
                &format!(
                    "… {earlier} earlier message{} in this session",
                    if earlier == 1 { "" } else { "s" }
                ),
                width,
            );
        }
        let theme = self.theme();
        for message in &history[last..] {
            match message {
                Message::User { content } if typed(message) => {
                    self.transcript.push_user(content, width)
                }
                Message::Assistant { content, .. } if !content.trim().is_empty() => self
                    .transcript
                    .push_lines(crate::markdown::render(content, width, &theme), width),
                _ => {}
            }
        }
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
        self.on_paste_at(text, Instant::now());
    }

    /// Takes in a paste read at `now`. One that goes to the input while a prompt waits counts as
    /// typing: the prompt waits for a pause after it.
    pub fn on_paste_at(&mut self, text: &str, now: Instant) {
        if let Some(prompt) = &mut self.prompt
            && prompt.paste(text)
        {
            return;
        }
        if self.takes_keys_after_a_pause() && !self.arming.armed(now) {
            self.arming.typed(now);
        }
        if !self.editor.paste(text) {
            let mib = |bytes: usize| bytes as f64 / (1024.0 * 1024.0);
            self.hint = Some(format!(
                "the paste is too large ({:.1} MiB; at most {:.0} MiB): save it to a file in the workspace and mention it with @",
                mib(text.len()),
                mib(crate::editor::PASTE_MAX_BYTES),
            ));
        }
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
        if key.code != KeyCode::Esc {
            self.last_esc = None;
        }
        // The offer to resume automatically is answered with a key, while nothing is typed.
        if !ctrl && self.editor.is_empty() && !self.busy() {
            match (self.resume, key.code) {
                (Some(Resume::Asking { .. }), KeyCode::Char('y' | 'Y')) => {
                    self.answer_resume(true);
                    return None;
                }
                (Some(Resume::Asking { .. }), KeyCode::Char('n' | 'N') | KeyCode::Esc) => {
                    self.answer_resume(false);
                    return None;
                }
                (Some(Resume::Waiting { .. } | Resume::Checking { .. }), KeyCode::Esc) => {
                    self.resume = None;
                    self.transcript
                        .push_note("automatic resume cancelled", self.width);
                    return None;
                }
                _ => {}
            }
        }
        if self.takes_keys_after_a_pause() {
            if self.arming.armed(now) {
                return self.prompt_key(key);
            }
            // Esc stops the turn, as it does without the prompt: the approval is denied.
            if key.code == KeyCode::Esc && self.prompt.is_some() {
                self.answer(Answered::Interrupt);
                return Some(Action::Interrupt);
            }
            // Until the prompt or the picker takes keys, they were typed for the input, and it
            // waits for the user to pause.
            self.arming.typed(now);
        }
        // While a picker is open (not yet taking keys, or it would have had this one), what is
        // typed goes to the draft and nothing else happens: the draft is not sent, no command
        // runs, the mode does not change, and neither Esc nor Ctrl+D leaves or opens anything. It
        // waits for the picker to close.
        let under_picker = self.picker.is_some();
        if under_picker && (key.code == KeyCode::BackTab || key.code == KeyCode::Esc) {
            return None;
        }
        if ctrl && key.code == KeyCode::Char('d') && self.editor.is_empty() {
            return if under_picker { None } else { self.quit() };
        }
        if ctrl && key.code == KeyCode::Char('s') {
            return if under_picker { None } else { self.send_now() };
        }
        if let Some(action) = self.completion_key(key) {
            return action;
        }
        match key.code {
            KeyCode::Esc if self.busy() => return Some(Action::Interrupt),
            KeyCode::Esc if self.editor.is_empty() => return self.esc_on_empty_input(now),
            KeyCode::Esc => return None,
            KeyCode::BackTab => return self.cycle_mode(),
            _ => {}
        }
        match self.editor.key(key) {
            Edit::Submit if under_picker => {}
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
        if let Some((_, picker)) = &mut self.picker {
            let picked = picker.key(key)?;
            let (pick, _) = self.picker.take()?;
            return match picked {
                Picked::Chosen(index) => self.picked(pick, index),
                Picked::Cancelled => None,
            };
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
            // Input sent while the choice waited was queued behind it: it goes now, in this mode.
            if let Some(Action::Run(input)) = self.next_queued() {
                return Some(Action::RunIn(mode, input));
            }
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
        if self.picker.take().is_some() {
            return None;
        }
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
            let args = invocation.args.trim().to_string();
            return self.builtin(&name, &args, &full);
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

    /// Whether a command that changes the session can run now: not during a turn, when a hint
    /// says so and the input stays for later.
    fn between_turns(&mut self, name: &str) -> bool {
        if self.busy() {
            self.hint = Some(format!(
                "/{name} works between turns: press Esc to stop this one first"
            ));
            return false;
        }
        true
    }

    /// A built-in command that runs here, without a turn, or an unknown one. `args` follow its
    /// name.
    fn builtin(&mut self, name: &str, args: &str, full: &str) -> Option<Action> {
        let width = self.width;
        match name {
            "quit" => return self.quit(),
            "mode" | "compact" | "rewind" | "new" | "resume" | "model" | "login" | "budget"
                if !self.between_turns(name) => {}
            "budget" => {
                let amount = args.trim().trim_start_matches('$');
                let set = if amount.is_empty() {
                    None
                } else {
                    match amount.parse::<f64>() {
                        Ok(usd) if usd.is_finite() && usd > 0.0 => Some(usd),
                        _ => {
                            self.editor.submit();
                            self.transcript.push_user(full, width);
                            self.transcript.push_error(
                                &format!(
                                    "/budget takes an amount in USD above 0, such as /budget 2.00, not `{args}`"
                                ),
                                width,
                            );
                            return None;
                        }
                    }
                };
                self.editor.submit();
                self.transcript.push_user(full, width);
                return Some(Action::Budget {
                    session: self.session_id.clone(),
                    set,
                });
            }
            "login" => {
                self.editor.submit();
                self.transcript.push_user(full, width);
                let mut words = args.split_whitespace();
                let mut provider = None;
                let mut device = false;
                for word in words.by_ref() {
                    match word {
                        "--device" => device = true,
                        _ if provider.is_none() => provider = Some(word.to_string()),
                        other => {
                            self.transcript.push_error(
                                &format!("/login takes a provider and --device, not `{other}`"),
                                width,
                            );
                            return None;
                        }
                    }
                }
                self.work("signing in", true);
                return Some(Action::Login {
                    provider: provider.unwrap_or_else(|| "chatgpt".into()),
                    device,
                });
            }
            "model" => {
                self.editor.submit();
                self.transcript.push_user(full, width);
                if !args.is_empty() {
                    return self.switch_to(args);
                }
                let picker =
                    Picker::loading("Choose a model", "looking for models…").with_empty(NO_MODELS);
                self.open_picker(Pick::Model(Vec::new()), picker);
                return Some(Action::ListModels);
            }
            "new" => {
                self.editor.submit();
                self.transcript.push_user(full, width);
                self.work("starting a new session", true);
                return Some(Action::OpenSession(None));
            }
            "resume" => {
                self.editor.submit();
                self.transcript.push_user(full, width);
                return self.resume(args);
            }
            "rewind" => {
                self.editor.submit();
                self.transcript.push_user(full, width);
                self.open_rewind();
            }
            "mode" => {
                self.editor.submit();
                self.transcript.push_user(full, width);
                return self.mode_command(args);
            }
            "compact" => {
                self.editor.submit();
                self.transcript.push_user(full, width);
                self.work("compacting the conversation", true);
                let focus = (!args.is_empty()).then(|| args.to_string());
                return Some(Action::Compact(focus));
            }
            "help" => {
                self.editor.submit();
                self.transcript.push_user(full, width);
                self.help();
            }
            "context" => {
                self.editor.submit();
                self.transcript.push_user(full, width);
                let theme = self.theme();
                let lines = status::context_report(
                    &self.context,
                    &self.instruction_files,
                    self.window_note.as_deref(),
                    &theme,
                );
                self.transcript.push_lines(lines, width);
            }
            "usage" => {
                self.editor.submit();
                self.transcript.push_user(full, width);
                let theme = self.theme();
                let now = (self.clock)();
                let mut lines = self.totals.report(&theme);
                lines.extend(status::cost_lines(&self.costs, &self.usage_context, &theme));
                if self.model.starts_with("chatgpt/") || self.windows.is_some() {
                    lines.extend(status::window_lines(self.windows.as_ref(), now, &theme));
                }
                if !self.usage_context.prices.is_empty() {
                    lines.push(Line::from(Span::styled(
                        format!("price snapshot: {}", sanitize(&self.usage_context.prices)),
                        theme.dim(),
                    )));
                }
                self.transcript.push_lines(lines, width);
                return Some(Action::Usage(args.to_string()));
            }
            _ => {
                self.transcript.push_error(
                    &format!(
                        "unknown command /{name}; custom commands are Markdown files in .harness/commands, .claude/commands or .opencode/commands; to send text that starts with /, put a word before it"
                    ),
                    width,
                );
            }
        }
        None
    }

    /// `/mode`: with a mode's name, switches to it; alone, opens the mode picker.
    fn mode_command(&mut self, args: &str) -> Option<Action> {
        let width = self.width;
        if args.is_empty() {
            let items = MODES
                .iter()
                .map(|(mode, what)| {
                    let current = if *mode == self.mode { "(current) " } else { "" };
                    Item::new(&mode.to_string(), &format!("{current}{what}"))
                })
                .collect();
            let current = MODES.iter().position(|(m, _)| *m == self.mode).unwrap_or(0);
            let picker = Picker::new("Choose the approval mode", items)
                .with_selected(current)
                .with_footer(vec![
                    "full-access is chosen only when harness starts (--mode full-access).".into(),
                ]);
            self.open_picker(Pick::Mode, picker);
            return None;
        }
        match args.parse::<Mode>() {
            Ok(Mode::FullAccess) => self.transcript.push_error(
                "full-access is chosen only when harness starts (--mode full-access)",
                width,
            ),
            Ok(mode) if mode == self.mode => self
                .transcript
                .push_note(&format!("already in {mode} mode"), width),
            Ok(mode) => return self.switch_mode(mode),
            Err(why) => self.transcript.push_error(&why, width),
        }
        None
    }

    /// The user chose item `index` of the picker for `pick`.
    fn picked(&mut self, pick: Pick, index: usize) -> Option<Action> {
        match pick {
            Pick::Mode => {
                let mode = MODES.get(index)?.0;
                if mode == self.mode {
                    return None;
                }
                self.switch_mode(mode)
            }
            Pick::Rewind(points) => match points.into_iter().nth(index)? {
                None => {
                    self.work("undoing the last rewind", false);
                    Some(Action::UndoRewind)
                }
                Some(point) => {
                    let items = SCOPES
                        .iter()
                        .map(|(_, name, what)| Item::new(name, what))
                        .collect();
                    let picker = Picker::new("Restore what, to before this message?", items)
                        .with_footer(vec![
                            format!("› {}", first_line(&point.text)),
                            REWIND_LIMITS.into(),
                        ]);
                    self.open_picker(Pick::RewindScope(point), picker);
                    None
                }
            },
            Pick::Model(ids) => {
                let id = ids.into_iter().nth(index)?;
                self.switch_to(&id)
            }
            Pick::Session(ids) => {
                let id = ids.into_iter().nth(index)?;
                self.work("loading the session", true);
                Some(Action::OpenSession(Some(id)))
            }
            Pick::RewindScope(point) => {
                let scope = SCOPES.get(index)?.0;
                self.work("rewinding", false);
                self.rewinding = Some((point.text, scope));
                Some(Action::Rewind {
                    entry: point.entry,
                    scope,
                })
            }
        }
    }

    /// Switches the session to model `id`, unless it is on it already.
    fn switch_to(&mut self, id: &str) -> Option<Action> {
        if id == self.model {
            self.transcript
                .push_note(&format!("already on {id}"), self.width);
            return None;
        }
        self.work(&format!("switching to {id}"), true);
        self.switching = Some(id.to_string());
        Some(Action::SwitchModel(id.to_string()))
    }

    /// Opens the session picker, as `harness --resume` does when the session starts.
    pub fn open_session_picker(&mut self) {
        self.resume("");
    }

    /// `/resume`: with an id, continues that session; alone, opens the session picker.
    fn resume(&mut self, id: &str) -> Option<Action> {
        let width = self.width;
        if id == self.session_id {
            self.transcript
                .push_note(&format!("already in session {id}"), width);
            return None;
        }
        if !id.is_empty() {
            self.work("loading the session", true);
            return Some(Action::OpenSession(Some(id.to_string())));
        }
        let sessions: Vec<SessionSummary> = self
            .host
            .sessions()
            .into_iter()
            .filter(|s| s.id != self.session_id)
            .collect();
        if sessions.is_empty() {
            self.transcript
                .push_note("there is no other session in this project yet", width);
            return None;
        }
        let items = sessions
            .iter()
            .map(|s| {
                let first = s.first_message.as_deref().map(first_line);
                Item::new(
                    first.as_deref().unwrap_or("(no message)"),
                    &format!("{} · {}", s.started_at, s.id),
                )
            })
            .collect();
        let ids = sessions.into_iter().map(|s| s.id).collect();
        self.open_picker(
            Pick::Session(ids),
            Picker::new("Resume which session?", items),
        );
        None
    }

    /// Esc on empty input: pressed twice within [`REWIND_WINDOW`], opens the rewind list.
    fn esc_on_empty_input(&mut self, now: Instant) -> Option<Action> {
        match self.last_esc.take() {
            Some(at) if now.duration_since(at) <= REWIND_WINDOW => self.open_rewind(),
            _ => self.last_esc = Some(now),
        }
        None
    }

    /// The rewind list: "undo the last rewind" when it can be, then the user's messages, the
    /// latest first, with what a rewind cannot undo under them.
    fn open_rewind(&mut self) {
        let mut points: Vec<Option<RewindPoint>> = Vec::new();
        let mut items = Vec::new();
        if self.can_undo_rewind {
            points.push(None);
            items.push(Item::new(
                "undo the last rewind",
                "the files and the conversation as they were before it",
            ));
        }
        for point in self.rewind_points.iter().rev() {
            items.push(Item::new(&first_line(&point.text), ""));
            points.push(Some(point.clone()));
        }
        if items.is_empty() {
            self.transcript
                .push_note("there is nothing to rewind yet", self.width);
            return;
        }
        let picker = Picker::new("Rewind to before which message?", items)
            .with_footer(vec![REWIND_LIMITS.into()]);
        self.open_picker(Pick::Rewind(points), picker);
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
            ("Esc twice", "rewind to before an earlier message"),
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
        let extras = status::Extras {
            cost: self.costs.status(),
            // Only a subscription provider has windows: others show none.
            window: self
                .model
                .starts_with("chatgpt/")
                .then(|| status::window_segment(self.windows.as_ref(), (self.clock)())),
        };
        let mut line = status::status_line_with(
            &self.model,
            self.mode,
            &self.context,
            &self.totals,
            &extras,
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
        if (self.prompt.is_some() || self.plan_choice.is_some()) && self.arming.typed_past() {
            below.extend(wrap(
                &Line::from(Span::styled(TYPED_PAST, theme.dim())),
                width,
                &[],
                &[],
            ));
        }
        if let Some(line) = self.resume_line(&theme) {
            below.extend(wrap(&line, width, &[], &[]));
        }
        below.extend(wrap(&self.status(), width, &[], &[]));
        if let Some(hint) = self
            .hint
            .as_ref()
            .filter(|_| self.prompt.is_some() || self.plan_choice.is_some())
        {
            below.extend(hint_rows(hint, width, &theme));
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
        if let Some(what) = &self.working {
            waiting.insert(
                0,
                Line::from(vec![
                    Span::styled("● ", theme.accent()),
                    Span::styled(
                        if self.working_stoppable {
                            format!("{what}… (Esc to stop)")
                        } else {
                            format!("{what}…")
                        },
                        theme.dim(),
                    ),
                ]),
            );
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
        if let Some(line) = self.resume_line(&theme) {
            below.extend(wrap(&line, width, &[], &[]));
        }
        below.extend(wrap(&self.status(), width, &[], &[]));
        if let Some(hint) = &self.hint {
            below.extend(hint_rows(hint, width, &theme));
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

/// The first line of `text`, with `…` when there is more.
fn first_line(text: &str) -> String {
    let mut lines = text.trim().lines();
    let first = lines.next().unwrap_or_default();
    if lines.next().is_some() {
        format!("{first} …")
    } else {
        first.to_string()
    }
}

/// A hint under the status line, dim, wrapped to `width`.
fn hint_rows(hint: &str, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    wrap(
        &Line::from(Span::styled(sanitize(hint), theme.dim())),
        width,
        &[],
        &[],
    )
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
