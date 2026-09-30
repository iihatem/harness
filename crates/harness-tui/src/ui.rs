//! The interactive session: the agent runs in a task of its own, fed turns through a channel,
//! while this side draws the terminal, reads keys, and takes in the agent's events.

use std::{
    io,
    sync::Arc,
    time::{Duration, Instant},
};

use futures::{FutureExt, Stream, StreamExt};
use harness_core::{
    agent::{Agent, ContextUsage},
    event::AgentEvent,
    redact::{EventRedactor, Redactor},
    turn::TurnInput,
};
use ratatui::{backend::Backend, crossterm::event::Event};
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;

use crate::{
    app::{Action, App, Host, Options},
    approval::{Reply, Requests},
    inline::InlineTerminal,
    notify::Notify,
    plan::TextEditor,
};

/// How often the host's warnings are looked for while nothing else comes, as `harness ask` does:
/// a renewal waiting for another process says so while it waits.
pub const HOST_WARNINGS_EVERY: Duration = Duration::from_millis(250);

/// Whether the session goes on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Continue,
    Quit,
}

/// Work for the task that owns the agent.
enum Job {
    Turn {
        input: Box<TurnInput>,
        cancel: CancellationToken,
    },
    SetMode(harness_core::permission::Mode),
}

/// The interactive session on a terminal.
pub struct Ui<B: Backend> {
    app: App,
    term: InlineTerminal<B>,
    jobs: Option<mpsc::UnboundedSender<Job>>,
    events: mpsc::UnboundedReceiver<AgentEvent>,
    /// Where the context goes, sent by the runner after each job.
    contexts: mpsc::UnboundedReceiver<ContextUsage>,
    /// A job was sent whose context update has not come yet.
    awaiting_context: bool,
    approvals: Requests,
    runner: Option<JoinHandle<()>>,
    /// Cancels the running turn.
    cancel: Option<CancellationToken>,
    text_editor: Option<Box<dyn TextEditor>>,
    notifier: Option<Box<dyn Notify>>,
    /// Keeps the secrets harness knows out of what the agent's events show.
    redactor: Option<EventRedactor>,
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
        let (contexts_tx, contexts) = mpsc::unbounded_channel();
        let width = term.width() as usize;
        let mut app = App::new(options, host, width);
        let agent = agent.with_steering(app.steering());
        app.set_context(agent.context_usage());
        let runner = tokio::spawn(async move {
            let mut agent = agent;
            while let Some(job) = queue.recv().await {
                match job {
                    Job::Turn { input, cancel } => {
                        agent.run_turn(*input, &events_tx, cancel).await;
                    }
                    Job::SetMode(mode) => agent.set_mode(mode),
                }
                let _ = contexts_tx.send(agent.context_usage());
            }
        });
        Ui {
            app,
            term,
            jobs: Some(jobs),
            events,
            contexts,
            awaiting_context: false,
            approvals,
            runner: Some(runner),
            cancel: None,
            text_editor,
            notifier,
            redactor: None,
        }
    }

    /// Shows the agent's events, approvals and the host's warnings with the secrets `redactor`
    /// knows replaced by `[redacted]`, even when the model streams one in pieces.
    pub fn with_redactor(mut self, redactor: Arc<Redactor>) -> Self {
        self.redactor = Some(EventRedactor::new(redactor.clone()));
        self.app.set_redactor(redactor);
        self
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

    /// Writes the finished lines into the scrollback and redraws the live region.
    pub fn draw(&mut self) -> io::Result<()> {
        self.notify();
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
            Action::EditPlan(plan) => {
                let edited = match &mut self.text_editor {
                    Some(editor) => {
                        // The editor gets the terminal; the live region is drawn again after.
                        self.term.clear()?;
                        editor.edit(&plan)
                    }
                    None => Err(io::Error::other("no editor is set up")),
                };
                self.app.plan_edited(edited);
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
            Action::Interrupt => {
                if let Some(cancel) = &self.cancel {
                    cancel.cancel();
                }
                Flow::Continue
            }
            Action::Quit => Flow::Quit,
        })
    }

    /// Takes in one terminal event: a key, a paste, or a resize.
    pub fn handle(&mut self, event: Event) -> io::Result<Flow> {
        self.handle_at(event, Instant::now())
    }

    /// Takes in one terminal event read at `now`.
    pub fn handle_at(&mut self, event: Event, now: Instant) -> io::Result<Flow> {
        let flow = match event {
            Event::Key(key) => match self.app.on_key(key, now) {
                Some(action) => self.dispatch(action)?,
                None => Flow::Continue,
            },
            Event::Paste(text) => {
                self.app.on_paste(&text);
                Flow::Continue
            }
            Event::Resize(..) => {
                self.term.resized()?;
                self.app.set_width(self.term.width() as usize);
                Flow::Continue
            }
            _ => Flow::Continue,
        };
        if flow == Flow::Continue {
            self.draw()?;
        }
        Ok(flow)
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
        if let Some(action) = self.app.take_pending_mode() {
            self.dispatch(action)?;
        }
        if let Some(action) = self.app.next_queued() {
            self.dispatch(action)?;
        }
        self.draw()?;
        Ok(Flow::Continue)
    }

    /// Shows an approval request, after the events that came before it.
    fn approval(
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
        self.app.on_approval(request, reply);
        self.draw()?;
        Ok(Flow::Continue)
    }

    /// Takes in where the context goes after a job, and redraws the status line.
    fn context(&mut self, context: ContextUsage) -> io::Result<Flow> {
        self.awaiting_context = false;
        self.app.set_context(context);
        self.host_warnings();
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

    /// Waits for the agent's next event, the runner's next message, or
    /// [`HOST_WARNINGS_EVERY`], and takes it in.
    pub async fn next(&mut self) -> io::Result<Flow> {
        tokio::select! {
            event = self.events.recv() => match event {
                Some(event) => self.agent_event(event),
                None => Ok(Flow::Quit),
            },
            Some(context) = self.contexts.recv() => self.context(context),
            Some((request, reply)) = self.approvals.recv() => self.approval(request, reply),
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
        Ok(())
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
        Ok(())
    }

    /// Takes in the terminal events already waiting. Done before an agent's event or approval is
    /// shown, so that a key typed before a prompt appeared goes where it was typed for, never to
    /// the prompt.
    fn drain<S>(&mut self, input: &mut S) -> io::Result<Flow>
    where
        S: Stream<Item = io::Result<Event>> + Unpin,
    {
        while let Some(next) = input.next().now_or_never() {
            match next {
                Some(Ok(event)) => {
                    if self.handle(event)? == Flow::Quit {
                        return Ok(Flow::Quit);
                    }
                }
                Some(Err(e)) => return Err(e),
                None => return Ok(Flow::Quit),
            }
        }
        Ok(Flow::Continue)
    }

    /// Runs the session on `input`, the terminal's events, until the user leaves. Keys come
    /// first: what the user typed is taken in before what the agent sent meanwhile.
    pub async fn run<S>(&mut self, mut input: S) -> io::Result<()>
    where
        S: Stream<Item = io::Result<Event>> + Unpin,
    {
        self.draw()?;
        loop {
            let flow = tokio::select! {
                biased;
                event = input.next() => match event {
                    Some(Ok(event)) => self.handle(event)?,
                    Some(Err(e)) => {
                        self.finish().await?;
                        return Err(e);
                    }
                    None => Flow::Quit,
                },
                event = self.events.recv() => match event {
                    Some(event) => match self.drain(&mut input)? {
                        Flow::Continue => self.agent_event(event)?,
                        Flow::Quit => {
                            self.take_in(event);
                            Flow::Quit
                        }
                    },
                    None => Flow::Quit,
                },
                Some(context) = self.contexts.recv() => self.context(context)?,
                // A request left unshown when the user leaves is denied when its reply drops.
                Some((request, reply)) = self.approvals.recv() => match self.drain(&mut input)? {
                    Flow::Continue => self.approval(request, reply)?,
                    Flow::Quit => Flow::Quit,
                },
                _ = tokio::time::sleep(HOST_WARNINGS_EVERY) => self.idle()?,
            };
            if flow == Flow::Quit {
                break;
            }
        }
        self.finish().await
    }

    /// Ends the session: stops a running turn, waits for the agent to be dropped (which
    /// releases the session file), writes what finished meanwhile, and clears the live region.
    pub async fn finish(&mut self) -> io::Result<()> {
        if let Some(cancel) = &self.cancel {
            cancel.cancel();
        }
        self.jobs = None;
        if let Some(runner) = self.runner.take() {
            let _ = runner.await;
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
        self.term.insert(&finished)?;
        self.term.clear()
    }
}
