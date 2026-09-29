//! The interactive session: the agent runs in a task of its own, fed turns through a channel,
//! while this side draws the terminal, reads keys, and takes in the agent's events.

use std::{io, time::Instant};

use futures::{Stream, StreamExt};
use harness_core::{agent::Agent, event::AgentEvent, turn::TurnInput};
use ratatui::{backend::Backend, crossterm::event::Event};
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;

use crate::{
    app::{Action, App, Host, Options},
    inline::InlineTerminal,
};

/// Whether the session goes on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Continue,
    Quit,
}

/// Work for the task that owns the agent.
enum Job {
    Turn {
        input: TurnInput,
        cancel: CancellationToken,
    },
}

/// The interactive session on a terminal.
pub struct Ui<B: Backend> {
    app: App,
    term: InlineTerminal<B>,
    jobs: Option<mpsc::UnboundedSender<Job>>,
    events: mpsc::UnboundedReceiver<AgentEvent>,
    runner: Option<JoinHandle<()>>,
    /// Cancels the running turn.
    cancel: Option<CancellationToken>,
}

impl<B> Ui<B>
where
    B: Backend,
    B::Error: Send + Sync + 'static,
{
    /// Starts the session: `agent` moves to a task of its own.
    pub fn start(
        agent: Agent,
        host: Box<dyn Host>,
        term: InlineTerminal<B>,
        options: Options,
    ) -> Self {
        let (jobs, mut queue) = mpsc::unbounded_channel::<Job>();
        let (events_tx, events) = mpsc::unbounded_channel();
        let runner = tokio::spawn(async move {
            let mut agent = agent;
            while let Some(job) = queue.recv().await {
                match job {
                    Job::Turn { input, cancel } => {
                        agent.run_turn(input, &events_tx, cancel).await;
                    }
                }
            }
        });
        let width = term.width() as usize;
        Ui {
            app: App::new(options, host, width),
            term,
            jobs: Some(jobs),
            events,
            runner: Some(runner),
            cancel: None,
        }
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

    /// Writes the finished lines into the scrollback and redraws the live region.
    pub fn draw(&mut self) -> io::Result<()> {
        let finished = self.app.transcript.take_finished();
        self.term.insert(&finished)?;
        let rows = self.term.height() as usize;
        let (lines, cursor) = self.app.live(rows);
        let height = lines.len() as u16;
        self.term.draw(height, |area, buf| {
            for (i, line) in lines.iter().enumerate() {
                buf.set_line(area.x, area.y + i as u16, line, area.width);
            }
            Some(ratatui::layout::Position::new(
                area.x + cursor.x,
                area.y + cursor.y,
            ))
        })
    }

    fn dispatch(&mut self, action: Action) -> Flow {
        match action {
            Action::Run(input) => {
                let cancel = CancellationToken::new();
                self.cancel = Some(cancel.clone());
                if let Some(jobs) = &self.jobs {
                    let _ = jobs.send(Job::Turn { input, cancel });
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
        }
    }

    /// Takes in one terminal event: a key, a paste, or a resize.
    pub fn handle(&mut self, event: Event) -> io::Result<Flow> {
        let flow = match event {
            Event::Key(key) => match self.app.on_key(key, Instant::now()) {
                Some(action) => self.dispatch(action),
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

    /// Takes in an event from the agent, and the others already waiting.
    fn agent_event(&mut self, event: AgentEvent) -> io::Result<Flow> {
        self.app.on_event(&event);
        while let Ok(event) = self.events.try_recv() {
            self.app.on_event(&event);
        }
        self.draw()?;
        Ok(Flow::Continue)
    }

    /// Waits for the agent's next event and takes it in.
    pub async fn next(&mut self) -> io::Result<Flow> {
        match self.events.recv().await {
            Some(event) => self.agent_event(event),
            None => Ok(Flow::Quit),
        }
    }

    /// Takes in the agent's events until no turn is running.
    pub async fn settle(&mut self) -> io::Result<()> {
        while self.app.busy() || !self.events.is_empty() {
            if self.next().await? == Flow::Quit {
                break;
            }
        }
        Ok(())
    }

    /// Runs the session on `input`, the terminal's events, until the user leaves.
    pub async fn run<S>(mut self, mut input: S) -> io::Result<()>
    where
        S: Stream<Item = io::Result<Event>> + Unpin,
    {
        self.draw()?;
        loop {
            let flow = tokio::select! {
                event = input.next() => match event {
                    Some(Ok(event)) => self.handle(event)?,
                    Some(Err(e)) => {
                        self.finish().await?;
                        return Err(e);
                    }
                    None => Flow::Quit,
                },
                event = self.events.recv() => match event {
                    Some(event) => self.agent_event(event)?,
                    None => Flow::Quit,
                },
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
            self.app.on_event(&event);
        }
        let finished = self.app.transcript.take_finished();
        self.term.insert(&finished)?;
        self.term.clear()
    }
}
