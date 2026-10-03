//! The interactive session: the agent runs in a task of its own, fed turns through a channel,
//! while this side draws the terminal, reads keys, and takes in the agent's events.

use std::{
    io,
    sync::Arc,
    time::{Duration, Instant},
};

use futures::{FutureExt, Stream, StreamExt};
use harness_core::{
    agent::{Agent, ContextUsage, RewindError, RewindPoint, SessionModel},
    checkpoint::Checkpoints,
    event::AgentEvent,
    meter::WindowSnapshot,
    provider::Provider,
    redact::{EventRedactor, Redactor},
    session::{RewindScope, Session},
    turn::TurnInput,
};
use ratatui::{backend::Backend, crossterm::event::Event};
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;

use crate::{
    app::{Action, App, Done, Host, ModelSwitch, ModelView, OpenedSession, Options, SessionView},
    approval::{Reply, Requests},
    inline::{CursorReport, InlineTerminal},
    input::{CursorQuery, Timed},
    notify::Notify,
    plan::TextEditor,
};

/// How often the host's warnings are looked for while nothing else comes, as `harness ask` does:
/// a renewal waiting for another process says so while it waits.
pub const HOST_WARNINGS_EVERY: Duration = Duration::from_millis(250);

/// While the agent's events stream, the screen is redrawn at most this often; keys redraw at
/// once.
pub const REDRAW_EVERY: Duration = Duration::from_millis(30);

/// How long `/usage` waits for the provider to say where its usage windows stand.
pub const POLL_WAIT: Duration = Duration::from_secs(5);

/// How long the terminal's input is given to end after a write to the terminal failed.
const GONE_WAIT: Duration = Duration::from_secs(1);

/// How long the terminal has to say where its cursor is after a resize; one that does not
/// answer by then is taken to have done what xterm does, and after
/// [`CURSOR_MISSES`](crate::inline::CURSOR_MISSES) such misses in a row is not asked again.
pub const CURSOR_WAIT: Duration = Duration::from_millis(500);

/// After a write to the terminal failed with `error`: the terminal went away, when its input ends
/// within [`GONE_WAIT`] (its reader tells a moment after it goes), or the write failed for some
/// other reason.
async fn gone_or<S, E>(input: &mut S, error: io::Error) -> io::Result<Ending>
where
    S: Stream<Item = io::Result<E>> + Unpin,
{
    match tokio::time::timeout(GONE_WAIT, input.next()).await {
        Ok(None | Some(Err(_))) => Ok(Ending::Hangup),
        _ => Err(error),
    }
}

/// Whether the session goes on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Continue,
    Quit,
}

/// How the session ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ending {
    /// The user left.
    Quit,
    /// The terminal went away: a hangup (SIGHUP), or its input ended or failed.
    Hangup,
    /// harness was asked to stop (SIGTERM).
    Terminated,
}

/// Work for the task that owns the agent.
enum Job {
    Turn {
        input: Box<TurnInput>,
        cancel: CancellationToken,
    },
    SetMode(harness_core::permission::Mode),
    Compact {
        focus: Option<String>,
        cancel: CancellationToken,
    },
    Rewind {
        entry: String,
        scope: RewindScope,
    },
    UndoRewind,
    StartSession {
        session: Box<Session>,
        checkpoints: Option<Arc<Checkpoints>>,
        resumed: bool,
    },
    SwitchModel {
        model: Box<SessionModel>,
        window_note: String,
    },
}

/// What work the session started in the background, apart from the agent, comes back with.
enum Background {
    /// The models the host found.
    Models(Vec<String>),
    /// The model to switch to, made ready, or why it could not be.
    Switch(Result<ModelSwitch, String>),
    /// The session to continue in, opened, or why it could not be.
    Session {
        resumed: bool,
        result: Result<OpenedSession, String>,
    },
    /// Something a sign-in says while it waits.
    Note(String),
    /// How a sign-in ended.
    Login(Result<String, String>),
    /// What the provider said of its usage windows when the session asked as it started, or
    /// after a model switch.
    Windows(Result<WindowSnapshot, String>),
    /// What `/usage` asked for: the provider's windows (when it has any) and the ledger's report.
    Usage {
        windows: Option<Result<WindowSnapshot, String>>,
        ledger: Vec<String>,
    },
}

/// What the task that owns the agent says after each job: where the context goes now, and how
/// the job ended when that has something to tell.
struct Update {
    context: ContextUsage,
    /// What the conversation can be rewound to, and whether the last rewind can be undone.
    rewind: (Vec<RewindPoint>, bool),
    /// The session the agent continues in.
    session: String,
    done: Option<Done>,
}

/// The interactive session on a terminal.
pub struct Ui<B: Backend> {
    app: App,
    term: InlineTerminal<B>,
    jobs: Option<mpsc::UnboundedSender<Job>>,
    events: mpsc::UnboundedReceiver<AgentEvent>,
    /// What the runner says after each job.
    updates: mpsc::UnboundedReceiver<Update>,
    /// A job was sent whose context update has not come yet.
    awaiting_context: bool,
    approvals: Requests,
    runner: Option<JoinHandle<()>>,
    /// Cancels the running turn.
    cancel: Option<CancellationToken>,
    text_editor: Option<Box<dyn TextEditor>>,
    /// The plan the user asked to edit, for [`edit_plan`](Self::edit_plan).
    editing: Option<String>,
    notifier: Option<Box<dyn Notify>>,
    /// Keeps the secrets harness knows out of what the agent's events show.
    redactor: Option<EventRedactor>,
    /// When the screen was last drawn.
    drawn_at: Option<tokio::time::Instant>,
    /// Events came in since then that are not drawn yet.
    redraw: bool,
    /// Asks the terminal where its cursor is after a resize, through the thread that reads it;
    /// without it, the backend is asked.
    cursor_query: Option<CursorQuery>,
    /// What work in the background comes back with.
    background_tx: mpsc::UnboundedSender<Background>,
    background: mpsc::UnboundedReceiver<Background>,
    /// The provider the session's model runs on, asked where its usage windows stand.
    provider: Arc<dyn Provider>,
}

impl<B> Ui<B>
where
    B: Backend,
    B::Error: Send + Sync + 'static,
{
    /// Starts the session: `agent` moves to a task of its own. `approvals` are the requests of
    /// the agent's [`ChannelApprover`](crate::approval::ChannelApprover).
    pub fn start(
        agent: Agent,
        host: Box<dyn Host>,
        term: InlineTerminal<B>,
        mut options: Options,
        approvals: Requests,
    ) -> Self {
        let text_editor = options.text_editor.take();
        let notifier = options.notifier.take();
        let (jobs, mut queue) = mpsc::unbounded_channel::<Job>();
        let (events_tx, events) = mpsc::unbounded_channel();
        let (updates_tx, updates) = mpsc::unbounded_channel();
        let (background_tx, background) = mpsc::unbounded_channel();
        let width = term.width() as usize;
        let mut app = App::new(options, host, width);
        let provider = agent.provider();
        let agent = agent.with_steering(app.steering());
        app.set_context(agent.context_usage());
        app.set_rewind(agent.rewind_points(), agent.can_undo_rewind());
        app.set_session_id(agent.session().id());
        let runner = tokio::spawn(async move {
            let mut agent = agent;
            while let Some(job) = queue.recv().await {
                let done = match job {
                    Job::Turn { input, cancel } => {
                        agent.run_turn(*input, &events_tx, cancel).await;
                        None
                    }
                    Job::SetMode(mode) => {
                        agent.set_mode(mode);
                        None
                    }
                    Job::Compact { focus, cancel } => {
                        let result = agent.compact(focus.as_deref(), &events_tx, cancel).await;
                        Some(Done::Compacted(result))
                    }
                    Job::Rewind { entry, scope } => {
                        let result = agent.rewind(&entry, scope).await;
                        Some(Done::Rewound(result.map_err(|e| rewind_error(&e))))
                    }
                    Job::UndoRewind => {
                        let result = agent.undo_rewind().await;
                        Some(Done::UndidRewind(result.map_err(|e| rewind_error(&e))))
                    }
                    Job::StartSession {
                        session,
                        checkpoints,
                        resumed,
                    } => {
                        agent.start_session(*session, checkpoints);
                        let view = SessionView {
                            id: agent.session().id().to_string(),
                            history: agent.history().to_vec(),
                        };
                        Some(Done::Session {
                            resumed,
                            result: Ok(view),
                        })
                    }
                    Job::SwitchModel { model, window_note } => {
                        agent.switch_model(*model);
                        let view = ModelView {
                            id: agent.model_id().to_string(),
                            window_note,
                        };
                        Some(Done::Model(Ok(view)))
                    }
                };
                let _ = updates_tx.send(Update {
                    context: agent.context_usage(),
                    rewind: (agent.rewind_points(), agent.can_undo_rewind()),
                    session: agent.session().id().to_string(),
                    done,
                });
            }
        });
        let ui = Ui {
            app,
            term,
            jobs: Some(jobs),
            events,
            updates,
            awaiting_context: false,
            approvals,
            runner: Some(runner),
            cancel: None,
            text_editor,
            editing: None,
            notifier,
            redactor: None,
            drawn_at: None,
            redraw: false,
            cursor_query: None,
            background_tx,
            background,
            provider,
        };
        // A subscription's windows are asked for once as the session starts, never on a timer.
        ui.poll_windows();
        ui
    }

    /// Asks the provider where its usage windows stand, when it has any, in the background.
    fn poll_windows(&self) {
        let Some(poll) = self.provider.windows() else {
            return;
        };
        let tx = self.background_tx.clone();
        tokio::spawn(async move {
            let result = match tokio::time::timeout(POLL_WAIT, poll).await {
                Ok(result) => result,
                Err(_) => Err("it did not answer in time".to_string()),
            };
            let _ = tx.send(Background::Windows(result));
        });
    }

    /// After a resize, asks the terminal where its cursor is with `query`, through the thread
    /// that reads the terminal, rather than asking the backend, which would read the terminal
    /// itself.
    pub fn with_cursor_query(mut self, query: CursorQuery) -> Self {
        self.cursor_query = Some(query);
        self
    }

    /// Shows the agent's events, approvals and the host's warnings with the secrets `redactor`
    /// knows replaced by `[redacted]`, even when the model streams one in pieces.
    pub fn with_redactor(mut self, redactor: Arc<Redactor>) -> Self {
        self.redactor = Some(EventRedactor::new(redactor.clone()));
        self.app.set_redactor(redactor);
        self
    }

    /// Opens the session picker, as `harness --resume` does when the session starts.
    pub fn open_session_picker(&mut self) -> io::Result<()> {
        self.app.open_session_picker();
        self.draw()
    }

    pub fn app(&self) -> &App {
        &self.app
    }

    pub fn app_mut(&mut self) -> &mut App {
        &mut self.app
    }

    pub fn terminal(&self) -> &InlineTerminal<B> {
        &self.term
    }

    pub fn terminal_mut(&mut self) -> &mut InlineTerminal<B> {
        &mut self.term
    }

    /// Sends the notifications waiting. One that fails is dropped: it is no reason to stop.
    fn notify(&mut self) {
        for text in self.app.take_notifications() {
            if let Some(notifier) = &mut self.notifier {
                let _ = notifier.notify(&text);
            }
        }
    }

    /// Writes the finished lines into the scrollback and redraws the live region, or draws the
    /// picker the user is choosing in, in a full-screen view.
    pub fn draw(&mut self) -> io::Result<()> {
        self.drawn_at = Some(tokio::time::Instant::now());
        self.redraw = false;
        self.notify();
        if let Some(picker) = self.app.picker() {
            // Finished lines wait for the inline screen.
            let theme = self.app.theme();
            self.term
                .draw_full(|area, buf| picker.render(area, buf, &theme))?;
            // The picker takes keys once the user has paused, as an approval does.
            self.app.drawn(Instant::now());
            return Ok(());
        }
        if self.term.in_full_screen() {
            self.term.leave_full()?;
        }
        let finished = self.app.transcript.take_finished();
        self.term.insert(&finished)?;
        let rows = self.term.height() as usize;
        let (lines, cursor) = self.app.live(rows);
        let height = lines.len() as u16;
        self.term.draw(height, |area, buf| {
            for (i, line) in lines.iter().enumerate() {
                buf.set_line(area.x, area.y + i as u16, line, area.width);
            }
            cursor.map(|c| ratatui::layout::Position::new(area.x + c.x, area.y + c.y))
        })?;
        self.app.drawn(Instant::now());
        Ok(())
    }

    fn dispatch(&mut self, action: Action) -> io::Result<Flow> {
        Ok(match action {
            Action::RunIn(mode, input) => {
                self.dispatch(Action::SetMode(mode))?;
                self.dispatch(Action::Run(input))?
            }
            // Opened by `edit_plan`, off the session's task.
            Action::EditPlan(plan) => {
                self.editing = Some(plan);
                Flow::Continue
            }
            Action::Run(input) => {
                let cancel = CancellationToken::new();
                self.cancel = Some(cancel.clone());
                if let Some(jobs) = &self.jobs {
                    let _ = jobs.send(Job::Turn {
                        input: Box::new(input),
                        cancel,
                    });
                    self.awaiting_context = true;
                }
                Flow::Continue
            }
            Action::SetMode(mode) => {
                if let Some(jobs) = &self.jobs {
                    let _ = jobs.send(Job::SetMode(mode));
                    self.awaiting_context = true;
                }
                Flow::Continue
            }
            Action::Rewind { entry, scope } => {
                self.send_job(Job::Rewind { entry, scope });
                Flow::Continue
            }
            Action::UndoRewind => {
                self.send_job(Job::UndoRewind);
                Flow::Continue
            }
            Action::Usage(args) => {
                let poll = self.provider.windows();
                let ledger = self.app.host().usage_report(&args);
                let tx = self.background_tx.clone();
                tokio::spawn(async move {
                    let windows = match poll {
                        Some(poll) => Some(match tokio::time::timeout(POLL_WAIT, poll).await {
                            Ok(result) => result,
                            Err(_) => Err("it did not answer in time".to_string()),
                        }),
                        None => None,
                    };
                    let ledger = ledger.await;
                    if windows.is_some() || !ledger.is_empty() {
                        let _ = tx.send(Background::Usage { windows, ledger });
                    }
                });
                Flow::Continue
            }
            Action::ListModels => {
                let models = self.app.host().models();
                let tx = self.background_tx.clone();
                tokio::spawn(async move {
                    let _ = tx.send(Background::Models(models.await));
                });
                Flow::Continue
            }
            Action::SwitchModel(id) => {
                let cancel = CancellationToken::new();
                self.cancel = Some(cancel.clone());
                let switch = self.app.host().switch_model(&id, cancel);
                let tx = self.background_tx.clone();
                tokio::spawn(async move {
                    let _ = tx.send(Background::Switch(switch.await));
                });
                Flow::Continue
            }
            Action::Login { provider, device } => {
                let cancel = CancellationToken::new();
                self.cancel = Some(cancel.clone());
                let (notes_tx, mut notes) = mpsc::unbounded_channel();
                let login = self.app.host().login(&provider, device, notes_tx, cancel);
                let tx = self.background_tx.clone();
                tokio::spawn(async move {
                    let mut login = login;
                    let result = loop {
                        tokio::select! {
                            result = &mut login => break result,
                            Some(note) = notes.recv() => {
                                let _ = tx.send(Background::Note(note));
                            }
                        }
                    };
                    // What it said before it ended comes first.
                    while let Ok(note) = notes.try_recv() {
                        let _ = tx.send(Background::Note(note));
                    }
                    let _ = tx.send(Background::Login(result));
                });
                Flow::Continue
            }
            Action::OpenSession(id) => {
                // Reading the session and opening its checkpoints are the host's to do in the
                // background, so the screen stays alive and Esc can stop them.
                let resumed = id.is_some();
                let cancel = CancellationToken::new();
                self.cancel = Some(cancel.clone());
                let opening = self.app.host().open_session(id.as_deref(), cancel.clone());
                let tx = self.background_tx.clone();
                tokio::spawn(async move {
                    let result = tokio::select! {
                        biased;
                        () = cancel.cancelled() => Err("stopped".to_string()),
                        opened = opening => opened,
                    };
                    let _ = tx.send(Background::Session { resumed, result });
                });
                Flow::Continue
            }
            Action::Compact(focus) => {
                let cancel = CancellationToken::new();
                self.cancel = Some(cancel.clone());
                if let Some(jobs) = &self.jobs {
                    let _ = jobs.send(Job::Compact { focus, cancel });
                    self.awaiting_context = true;
                }
                Flow::Continue
            }
            Action::Interrupt => {
                if let Some(cancel) = &self.cancel {
                    cancel.cancel();
                }
                Flow::Continue
            }
            Action::Quit => Flow::Quit,
        })
    }

    /// Gives `job` to the task that owns the agent.
    fn send_job(&mut self, job: Job) {
        if let Some(jobs) = &self.jobs {
            let _ = jobs.send(job);
            self.awaiting_context = true;
        }
    }

    /// Opens the plan in the user's editor, when they asked to edit it, and takes in the edited
    /// plan. The editor runs on a thread of its own, off the session's task, with the terminal
    /// given to it meanwhile.
    pub async fn edit_plan(&mut self) -> io::Result<()> {
        let Some(plan) = self.editing.take() else {
            return Ok(());
        };
        let edited = match self.text_editor.take() {
            Some(mut editor) => {
                // The editor gets the terminal; the live region is drawn again after.
                if let Err(e) = self.term.clear() {
                    self.text_editor = Some(editor);
                    return Err(e);
                }
                let ran = tokio::task::spawn_blocking(move || {
                    let edited = editor.edit(&plan);
                    (editor, edited)
                })
                .await;
                match ran {
                    Ok((editor, edited)) => {
                        self.text_editor = Some(editor);
                        edited
                    }
                    Err(e) => Err(io::Error::other(format!("the editor failed: {e}"))),
                }
            }
            None => Err(io::Error::other("no editor is set up")),
        };
        self.app.plan_edited(edited);
        self.draw()
    }

    /// When the screen may be drawn again after the agent's events.
    fn redraw_at(&self) -> tokio::time::Instant {
        self.drawn_at
            .map_or_else(tokio::time::Instant::now, |at| at + REDRAW_EVERY)
    }

    /// Draws after the agent's events, or, within [`REDRAW_EVERY`] of the last draw, once that
    /// has passed.
    fn draw_soon(&mut self) -> io::Result<()> {
        if tokio::time::Instant::now() >= self.redraw_at() {
            self.draw()
        } else {
            self.redraw = true;
            Ok(())
        }
    }

    /// Takes in one terminal event: a key, a paste, or a resize (the backend is asked where the
    /// cursor is), and draws.
    pub fn handle(&mut self, event: Event) -> io::Result<Flow> {
        self.handle_at(event, Instant::now())
    }

    /// Takes in one terminal event read at `now`, and draws.
    pub fn handle_at(&mut self, event: Event, now: Instant) -> io::Result<Flow> {
        let flow = self.take_in_at(event, now)?;
        if flow == Flow::Continue {
            self.draw()?;
        }
        Ok(flow)
    }

    /// Takes in one terminal event read at `now`, without drawing.
    fn take_in_at(&mut self, event: Event, now: Instant) -> io::Result<Flow> {
        Ok(match event {
            Event::Key(key) => match self.app.on_key(key, now) {
                Some(action) => self.dispatch(action)?,
                None => Flow::Continue,
            },
            Event::Paste(text) => {
                self.app.on_paste_at(&text, now);
                Flow::Continue
            }
            Event::Resize(..) => {
                self.term.resized()?;
                self.app.set_width(self.term.width() as usize);
                Flow::Continue
            }
            _ => Flow::Continue,
        })
    }

    /// Takes in one terminal event, as of when it was read, without drawing. After a resize,
    /// the terminal is asked where its cursor is through the thread that reads it, when there is
    /// one, waiting at most [`CURSOR_WAIT`] for the answer.
    async fn take_in_key(&mut self, Timed { event, at }: Timed) -> io::Result<Flow> {
        if let (Event::Resize(..), Some(query)) = (&event, self.cursor_query.clone()) {
            let cursor = if self.term.reports_cursor() {
                query.position(CURSOR_WAIT).await
            } else {
                CursorReport::Unasked
            };
            self.term.resized_to(cursor)?;
            self.app.set_width(self.term.width() as usize);
            return Ok(Flow::Continue);
        }
        self.take_in_at(event, at)
    }

    /// Takes in `event`, redacted: none, one or more events, since the redactor holds back the
    /// end of a reply that could still become a secret.
    fn show(&mut self, event: AgentEvent) {
        let shown = match &mut self.redactor {
            Some(redactor) => redactor.push(event),
            None => vec![event],
        };
        for event in &shown {
            self.app.on_event(event);
        }
    }

    /// Shows a message the host kept: a note when it starts `note: `, else a warning.
    fn show_notice(&mut self, message: String) {
        match message.strip_prefix("note: ") {
            Some(note) => self.app.push_note(note),
            None => self.show(AgentEvent::Warning { message }),
        }
    }

    /// Shows what the host has had to warn about; whether there was anything.
    fn host_warnings(&mut self) -> bool {
        let warnings = self.app.host().take_warnings();
        let any = !warnings.is_empty();
        for message in warnings {
            self.show(AgentEvent::Warning { message });
        }
        any
    }

    /// Takes in an event from the agent, after what the host has had to warn about by then: a
    /// sign-in renewed during the turn that could not be stored, say, is told before what the
    /// provider sent after the renewal.
    fn take_in(&mut self, event: AgentEvent) {
        self.host_warnings();
        self.show(event);
    }

    /// Takes in an event from the agent, and the others already waiting; once a turn has
    /// ended, switches to the mode chosen during it.
    fn agent_event(&mut self, event: AgentEvent) -> io::Result<Flow> {
        self.take_in(event);
        while let Ok(event) = self.events.try_recv() {
            self.take_in(event);
        }
        self.host_warnings();
        self.next_actions()?;
        self.draw_soon()?;
        Ok(Flow::Continue)
    }

    /// Once nothing runs: switches to the mode chosen during the turn, then sends queued input.
    fn next_actions(&mut self) -> io::Result<()> {
        if let Some(action) = self.app.take_pending_mode() {
            self.dispatch(action)?;
        }
        if let Some(action) = self.app.next_queued() {
            self.dispatch(action)?;
        }
        Ok(())
    }

    /// Shows an approval request, after the events that came before it.
    async fn approval(
        &mut self,
        request: harness_core::agent::ApprovalRequest,
        reply: Reply,
    ) -> io::Result<Flow> {
        while let Ok(event) = self.events.try_recv() {
            self.take_in(event);
        }
        self.host_warnings();
        // What the redactor holds back comes before the prompt.
        if let Some(redactor) = &mut self.redactor {
            for event in redactor.finish() {
                self.app.on_event(&event);
            }
        }
        let (workspace, theme) = self.app.approval_context();
        let body = crate::approval::prepare_body(&request, workspace, theme).await;
        self.app.on_approval(request, reply, body);
        self.draw()?;
        Ok(Flow::Continue)
    }

    /// Takes in what the runner says after a job, and redraws; queued input goes next.
    fn update(&mut self, update: Update) -> io::Result<Flow> {
        self.awaiting_context = false;
        // Events the job sent come first.
        while let Ok(event) = self.events.try_recv() {
            self.show(event);
        }
        self.app.set_context(update.context);
        let (points, can_undo) = update.rewind;
        self.app.set_rewind(points, can_undo);
        self.app.set_session_id(&update.session);
        if let Some(done) = update.done {
            self.app.on_done(done);
        }
        self.host_warnings();
        self.next_actions()?;
        self.draw()?;
        Ok(Flow::Continue)
    }

    /// Shows what the host has had to warn about while nothing else came.
    fn idle(&mut self) -> io::Result<Flow> {
        if self.host_warnings() {
            self.draw()?;
        }
        Ok(Flow::Continue)
    }

    /// Takes in what work in the background came back with.
    fn background(&mut self, done: Background) -> io::Result<Flow> {
        match done {
            Background::Models(ids) => self.app.on_models(ids),
            Background::Switch(Ok(switch)) => {
                for message in switch.warnings {
                    self.show(AgentEvent::Warning { message });
                }
                self.provider = switch.model.provider.clone();
                self.poll_windows();
                self.send_job(Job::SwitchModel {
                    model: Box::new(switch.model),
                    window_note: switch.window_note,
                });
            }
            Background::Switch(Err(why)) => {
                self.app.on_done(Done::Model(Err(why)));
                self.next_actions()?;
            }
            Background::Session {
                resumed,
                result: Ok(opened),
            } => {
                for message in opened.warnings {
                    self.show_notice(message);
                }
                self.send_job(Job::StartSession {
                    session: Box::new(opened.session),
                    checkpoints: opened.checkpoints,
                    resumed,
                });
            }
            Background::Session {
                resumed,
                result: Err(why),
            } => {
                self.app.on_done(Done::Session {
                    resumed,
                    result: Err(why),
                });
                self.next_actions()?;
            }
            Background::Windows(result) => self.app.on_windows(result),
            Background::Usage { windows, ledger } => self.app.on_usage(windows, ledger),
            Background::Note(note) => self.app.push_note(&note),
            Background::Login(result) => {
                self.app.on_done(Done::LoggedIn(result));
                self.next_actions()?;
            }
        }
        self.host_warnings();
        self.draw()?;
        Ok(Flow::Continue)
    }

    /// Waits for the agent's next event, the runner's next message, or
    /// [`HOST_WARNINGS_EVERY`], and takes it in.
    pub async fn next(&mut self) -> io::Result<Flow> {
        let (redraw, redraw_at) = (self.redraw, self.redraw_at());
        tokio::select! {
            event = self.events.recv() => match event {
                Some(event) => self.agent_event(event),
                None => Ok(Flow::Quit),
            },
            Some(update) = self.updates.recv() => self.update(update),
            Some(done) = self.background.recv() => self.background(done),
            Some((request, reply)) = self.approvals.recv() => self.approval(request, reply).await,
            _ = tokio::time::sleep_until(redraw_at), if redraw => {
                self.draw().map(|()| Flow::Continue)
            }
            _ = tokio::time::sleep(HOST_WARNINGS_EVERY) => self.idle(),
        }
    }

    /// Takes in the agent's events until `done` holds.
    pub async fn until(&mut self, done: impl Fn(&App) -> bool) -> io::Result<()> {
        while !done(&self.app) {
            if self.next().await? == Flow::Quit {
                break;
            }
        }
        self.drawn()
    }

    /// Draws what is not drawn yet.
    fn drawn(&mut self) -> io::Result<()> {
        if self.redraw { self.draw() } else { Ok(()) }
    }

    /// Takes in the agent's events until no turn is running and the runner has said where the
    /// context goes after it, or until an approval waits for the user.
    pub async fn settle(&mut self) -> io::Result<()> {
        while self.app.prompt().is_none()
            && (self.app.busy() || self.awaiting_context || !self.events.is_empty())
        {
            if self.next().await? == Flow::Quit {
                break;
            }
        }
        self.drawn()
    }

    /// Takes in `first`, if any, and the terminal events already waiting after it, and draws
    /// once after them all: a burst of keys costs one draw. Done before an agent's event or
    /// approval is shown, so that a key typed before a prompt appeared goes where it was typed
    /// for, never to the prompt. `Some` when the session ends.
    async fn keys<S, E>(
        &mut self,
        first: Option<Timed>,
        input: &mut S,
    ) -> io::Result<Option<Ending>>
    where
        S: Stream<Item = io::Result<E>> + Unpin,
        E: Into<Timed>,
    {
        let mut next = first;
        let mut taken = false;
        loop {
            let timed = match next.take() {
                Some(timed) => timed,
                None => match input.next().now_or_never() {
                    None => break,
                    Some(Some(Ok(event))) => event.into(),
                    Some(Some(Err(_)) | None) => return Ok(Some(Ending::Hangup)),
                },
            };
            taken = true;
            if self.take_in_key(timed).await? == Flow::Quit {
                return Ok(Some(Ending::Quit));
            }
        }
        if taken {
            self.draw()?;
        }
        Ok(None)
    }

    /// Runs the session on `input`, the terminal's events (each timed by when it was read, or
    /// else taken as read when the session takes it in), until the user leaves, the terminal goes
    /// away (its input ends or fails), or `shutdown` says to stop (a signal). Keys come first:
    /// what the user typed is taken in before what the agent sent meanwhile.
    ///
    /// However it ends, the session [finishes](Self::finish): the turn stops, whatever waits
    /// for an approval is denied, and the agent is dropped. Only after the user left does a
    /// failure to write the terminal fail the run: once it went away, or harness was asked to
    /// stop, there may be no terminal to write to.
    pub async fn run<S, E, D>(&mut self, mut input: S, shutdown: D) -> io::Result<Ending>
    where
        S: Stream<Item = io::Result<E>> + Unpin,
        E: Into<Timed>,
        D: std::future::Future<Output = Ending>,
    {
        let ending = self.serve(&mut input, shutdown).await;
        let finished = self.finish().await;
        match ending? {
            Ending::Quit => finished.map(|()| Ending::Quit),
            other => Ok(other),
        }
    }

    /// The session's loop, for [`run`](Self::run): how it ended. Its only failures are writes to
    /// the terminal, and a terminal that closed fails a write a moment before its reader can
    /// tell: however the draw came about (a key, the agent's events, an approval), a failed one
    /// ends the session as a hangup when the terminal's input ends soon after.
    async fn serve<S, E, D>(&mut self, input: &mut S, shutdown: D) -> io::Result<Ending>
    where
        S: Stream<Item = io::Result<E>> + Unpin,
        E: Into<Timed>,
        D: std::future::Future<Output = Ending>,
    {
        match self.serve_until_done(input, shutdown).await {
            Ok(ending) => Ok(ending),
            Err(error) => gone_or(input, error).await,
        }
    }

    /// The loop [`serve`](Self::serve) runs.
    async fn serve_until_done<S, E, D>(&mut self, input: &mut S, shutdown: D) -> io::Result<Ending>
    where
        S: Stream<Item = io::Result<E>> + Unpin,
        E: Into<Timed>,
        D: std::future::Future<Output = Ending>,
    {
        tokio::pin!(shutdown);
        self.draw()?;
        loop {
            self.edit_plan().await?;
            let (redraw, redraw_at) = (self.redraw, self.redraw_at());
            let flow = tokio::select! {
                biased;
                ending = &mut shutdown => return Ok(ending),
                event = input.next() => match event {
                    Some(Ok(event)) => match self.keys(Some(event.into()), input).await? {
                        None => Flow::Continue,
                        Some(ending) => return Ok(ending),
                    },
                    Some(Err(_)) | None => return Ok(Ending::Hangup),
                },
                event = self.events.recv() => match event {
                    Some(event) => match self.keys(None, input).await? {
                        None => self.agent_event(event)?,
                        Some(ending) => {
                            self.take_in(event);
                            return Ok(ending);
                        }
                    },
                    None => Flow::Quit,
                },
                Some(update) = self.updates.recv() => self.update(update)?,
                Some(done) = self.background.recv() => self.background(done)?,
                // A request left unshown when the session ends is denied when its reply drops.
                Some((request, reply)) = self.approvals.recv() => match self.keys(None, input).await? {
                    None => self.approval(request, reply).await?,
                    Some(ending) => return Ok(ending),
                },
                // Drawn after the keys waiting.
                _ = tokio::time::sleep_until(redraw_at), if redraw => match self.keys(None, input).await? {
                    None => self.draw().map(|()| Flow::Continue)?,
                    Some(ending) => return Ok(ending),
                },
                _ = tokio::time::sleep(HOST_WARNINGS_EVERY) => self.idle()?,
            };
            if flow == Flow::Quit {
                return Ok(Ending::Quit);
            }
        }
    }

    /// Ends the session: denies what waits for an approval, shown or not, and whatever the
    /// agent would ask from now on; stops a running turn, which stops the command it runs;
    /// waits for the agent to be dropped (which releases the session file); writes what finished
    /// meanwhile, and clears the live region. An agent whose task panicked (its events then end,
    /// as if the user had left) fails it, saying so.
    pub async fn finish(&mut self) -> io::Result<()> {
        self.app.deny_waiting();
        self.approvals.close();
        // Dropping a reply denies it.
        while self.approvals.try_recv().is_ok() {}
        if let Some(cancel) = &self.cancel {
            cancel.cancel();
        }
        self.jobs = None;
        let mut stopped = Ok(());
        if let Some(runner) = self.runner.take()
            && let Err(error) = runner.await
        {
            stopped = Err(io::Error::other(format!(
                "the agent stopped unexpectedly ({})",
                why_stopped(error)
            )));
        }
        while let Ok(event) = self.events.try_recv() {
            self.take_in(event);
        }
        self.host_warnings();
        if let Some(redactor) = &mut self.redactor {
            for event in redactor.finish() {
                self.app.on_event(&event);
            }
        }
        let finished = self.app.transcript.take_finished();
        let written = self.term.insert(&finished).and_then(|()| self.term.clear());
        stopped.and(written)
    }
}

/// Why the agent's task ended early: it panicked, with its message when it has one.
fn why_stopped(error: tokio::task::JoinError) -> String {
    if !error.is_panic() {
        return "its task was cancelled".into();
    }
    let payload = error.into_panic();
    match payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
    {
        Some(message) => format!("it panicked: {message}"),
        None => "it panicked".into(),
    }
}

/// What a failed rewind says. One that failed partway through restoring files has changed some,
/// and offers to undo, which puts them back as they were.
fn rewind_error(error: &RewindError) -> String {
    match error {
        RewindError::Restore(_) => format!(
            "{error}; some files may be restored and others not: \"undo the last rewind\" is now offered in /rewind, to put them back"
        ),
        _ => error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use harness_core::checkpoint::CheckpointError;

    use super::*;

    // Review A minor 5.
    #[test]
    fn a_rewind_that_failed_partway_says_undo_is_offered() {
        let half = RewindError::Restore(CheckpointError::Restore {
            before: "abc123".into(),
            source: Box::new(CheckpointError::TooSlow),
        });
        let said = rewind_error(&half);
        assert!(said.contains("snapshot abc123"), "{said}");
        assert!(said.contains("undo the last rewind"), "{said}");
        let refused = rewind_error(&RewindError::NoCheckpoints);
        assert_eq!(refused, RewindError::NoCheckpoints.to_string());
    }
}
