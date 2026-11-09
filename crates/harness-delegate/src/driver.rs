//! The part every transport shares: run the child in its own process group, read its output as
//! bounded lines, write its input, keep the tail of its stderr, escalate signals when it must
//! stop, and make sure the event stream always ends with exactly one `Finished`.
//!
//! What the lines *mean* belongs to a [`Protocol`]: a synchronous state machine (a line in,
//! [`Out`] out), so each transport's behaviour is a plain unit test over fixture lines.

use std::{
    process::ExitStatus,
    sync::{Arc, Mutex},
    time::Duration,
};

use harness_core::{
    event::{ErrorKind, TurnEndReason},
    redact::Redactor,
};
use nix::{
    sys::signal::{Signal, killpg},
    unistd::Pid,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader},
    process::Command,
    sync::mpsc,
    time::Instant,
};

use crate::{
    DelegateError, DelegateRun, RunControl,
    backend::Command as ControlCommand,
    event::{ApprovalAnswer, DelegateEvent},
    redact::EventFilter,
};

/// How much of the end of stderr is kept for a crash message.
const STDERR_TAIL: usize = 4096;
/// How long stdout is still read after the child exited (something it started may hold the pipe).
const DRAIN_AFTER_EXIT: Duration = Duration::from_millis(300);

/// What a protocol wants done after a line, an answer or an interrupt.
#[derive(Debug, Default)]
pub struct Out {
    /// Events to emit (they pass through the redaction filter).
    pub events: Vec<DelegateEvent>,
    /// Lines to write to the child's stdin (a newline is added).
    pub writes: Vec<String>,
    /// The turn is over (the protocol already emitted `Finished`): the driver stops parsing,
    /// closes stdin if asked, and ends the child if it lingers.
    pub finished: bool,
    pub close_stdin: bool,
    /// Also stop the child: the escalation clock starts (the protocol already wrote whatever
    /// polite request there is). For a cancelled approval, or a run that cannot go on.
    pub stop: bool,
}

/// One transport's wire format, as a state machine.
pub trait Protocol: Send + 'static {
    /// Called once, before any line is read: the lines that open the conversation.
    fn start(&mut self) -> Out;
    /// A non-empty line of the child's stdout.
    fn on_line(&mut self, line: &str) -> Out;
    /// The caller's answer to the approval `id`; an error for one that is not waiting.
    fn on_answer(&mut self, id: &str, answer: ApprovalAnswer) -> Result<Out, DelegateError>;
    /// The caller asked to stop the turn: the polite, in-protocol request.
    fn on_interrupt(&mut self) -> Out;
    /// The child's session id, once known.
    fn session_id(&self) -> Option<String> {
        None
    }
    /// Signals for the process group, one more grace period apart, after the polite request.
    fn escalation(&self) -> &'static [Signal] {
        &[Signal::SIGTERM, Signal::SIGKILL]
    }
}

#[derive(Debug, Clone)]
pub struct DriverConfig {
    /// The program's name, for messages.
    pub label: String,
    /// How long each step of a stop waits before the next.
    pub grace: Duration,
    pub redactor: Arc<Redactor>,
    /// The longest line read; a longer one is dropped with a warning.
    pub max_line: usize,
}

/// A line of the child's output.
#[derive(Debug, PartialEq, Eq)]
enum Line {
    Text(String),
    /// A line over the cap, of this many bytes, dropped.
    TooLong(usize),
}

/// Reads lines no longer than `cap`; a longer line is skipped to its end and reported. Cancel
/// safe: its state is in the struct, so it can sit in a `select!`.
struct LineReader<R> {
    reader: BufReader<R>,
    buf: Vec<u8>,
    skipping: bool,
    dropped: usize,
    cap: usize,
}

impl<R: AsyncRead + Unpin> LineReader<R> {
    fn new(reader: R, cap: usize) -> Self {
        LineReader {
            reader: BufReader::new(reader),
            buf: Vec::new(),
            skipping: false,
            dropped: 0,
            cap,
        }
    }

    /// Takes `n` more bytes of a line into the buffer, or counts them as dropped.
    fn take(&mut self, chunk: &[u8]) {
        if self.skipping {
            self.dropped += chunk.len();
        } else if self.buf.len() + chunk.len() > self.cap {
            self.skipping = true;
            self.dropped = self.buf.len() + chunk.len();
            self.buf.clear();
        } else {
            self.buf.extend_from_slice(chunk);
        }
    }

    fn finish_line(&mut self) -> Line {
        if self.skipping {
            self.skipping = false;
            return Line::TooLong(std::mem::take(&mut self.dropped));
        }
        let bytes = std::mem::take(&mut self.buf);
        let text = String::from_utf8_lossy(&bytes);
        Line::Text(text.trim_end_matches('\r').to_string())
    }

    async fn next(&mut self) -> Option<Line> {
        loop {
            let available = match self.reader.fill_buf().await {
                Ok(bytes) if !bytes.is_empty() => bytes,
                // The end (or an error, which ends the stream the same way).
                _ => {
                    return (self.skipping || !self.buf.is_empty()).then(|| self.finish_line());
                }
            };
            match available.iter().position(|b| *b == b'\n') {
                Some(end) => {
                    let chunk = available[..end].to_vec();
                    self.reader.consume(end + 1);
                    self.take(&chunk);
                    return Some(self.finish_line());
                }
                None => {
                    let chunk = available.to_vec();
                    self.reader.consume(chunk.len());
                    self.take(&chunk);
                }
            }
        }
    }
}

struct Ladder {
    steps: &'static [Signal],
    next: usize,
    at: Instant,
}

struct Run<P> {
    protocol: P,
    filter: EventFilter,
    events: mpsc::UnboundedSender<DelegateEvent>,
    stdin: mpsc::UnboundedSender<Option<String>>,
    session: Arc<Mutex<Option<String>>>,
    finished: bool,
    consumer_gone: bool,
}

impl<P: Protocol> Run<P> {
    fn apply(&mut self, out: Out) {
        for event in out.events {
            for shown in self.filter.push(event) {
                if self.events.send(shown).is_err() {
                    self.consumer_gone = true;
                }
            }
        }
        for line in out.writes {
            let _ = self.stdin.send(Some(line));
        }
        if out.close_stdin {
            let _ = self.stdin.send(None);
        }
        self.finished |= out.finished;
        if let Some(id) = self.protocol.session_id() {
            *self.session.lock().expect("session lock") = Some(id);
        }
    }

    fn warn(&mut self, message: String) {
        self.apply(Out {
            events: vec![DelegateEvent::Warning { message }],
            ..Out::default()
        });
    }
}

fn ladder_for<P: Protocol>(protocol: &P, grace: Duration) -> Ladder {
    Ladder {
        steps: protocol.escalation(),
        next: 0,
        at: Instant::now() + grace,
    }
}

fn signal_group(pid: i32, signal: Signal) {
    let _ = killpg(Pid::from_raw(pid), signal);
}

/// Spawns `command` in its own process group with all three pipes, and runs `protocol` over it.
/// Must be called inside a tokio runtime.
pub fn spawn_run<P: Protocol>(
    mut command: Command,
    protocol: P,
    config: DriverConfig,
) -> Result<DelegateRun, DelegateError> {
    use std::process::Stdio;
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(|source| DelegateError::Spawn {
        binary: config.label.clone(),
        source,
    })?;
    let pid = child
        .id()
        .and_then(|id| i32::try_from(id).ok())
        .expect("a spawned child has a pid");
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");
    let mut stdin = child.stdin.take().expect("stdin is piped");

    // The writer is its own task so a large write cannot stall the reading of stdout.
    let (in_tx, mut in_rx) = mpsc::unbounded_channel::<Option<String>>();
    tokio::spawn(async move {
        while let Some(Some(line)) = in_rx.recv().await {
            let sent = async {
                stdin.write_all(line.as_bytes()).await?;
                stdin.write_all(b"\n").await?;
                stdin.flush().await
            };
            if sent.await.is_err() {
                break;
            }
        }
        // Dropping stdin closes the child's input.
    });

    let tail = Arc::new(Mutex::new(Vec::<u8>::new()));
    let tail_task = {
        let tail = tail.clone();
        tokio::spawn(async move {
            let mut stderr = stderr;
            let mut chunk = [0u8; 2048];
            loop {
                match tokio::io::AsyncReadExt::read(&mut stderr, &mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut tail = tail.lock().expect("tail lock");
                        tail.extend_from_slice(&chunk[..n]);
                        let over = tail.len().saturating_sub(STDERR_TAIL);
                        tail.drain(..over);
                    }
                }
            }
        })
    };

    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let session = Arc::new(Mutex::new(None));
    let run = Run {
        protocol,
        filter: EventFilter::new(config.redactor.clone()),
        events: events_tx,
        stdin: in_tx,
        session: session.clone(),
        finished: false,
        consumer_gone: false,
    };
    tokio::spawn(drive(
        run, child, pid, stdout, cmd_rx, tail, tail_task, config,
    ));
    Ok(DelegateRun {
        events: events_rx,
        control: RunControl {
            tx: cmd_tx,
            session,
        },
    })
}

#[allow(clippy::too_many_arguments)]
async fn drive<P: Protocol>(
    mut run: Run<P>,
    mut child: tokio::process::Child,
    pid: i32,
    stdout: tokio::process::ChildStdout,
    mut commands: mpsc::UnboundedReceiver<ControlCommand>,
    tail: Arc<Mutex<Vec<u8>>>,
    tail_task: tokio::task::JoinHandle<()>,
    config: DriverConfig,
) {
    let mut lines = LineReader::new(stdout, config.max_line);
    let mut exit: Option<ExitStatus> = None;
    let mut ladder: Option<Ladder> = None;
    let mut interrupted = false;
    let mut eof = false;
    let mut commands_open = true;
    let mut drain_until: Option<Instant> = None;
    let mut lingering = false;

    let out = run.protocol.start();
    run.apply(out);

    while !eof && !run.consumer_gone {
        // A finished turn gets one grace period to exit by itself, then the ladder.
        if run.finished && !lingering {
            lingering = true;
            if ladder.is_none() {
                ladder = Some(Ladder {
                    steps: &[Signal::SIGTERM, Signal::SIGKILL],
                    next: 0,
                    at: Instant::now() + config.grace,
                });
            }
        }
        let timer = [ladder.as_ref().map(|l| l.at), drain_until]
            .into_iter()
            .flatten()
            .min();
        tokio::select! {
            command = commands.recv(), if commands_open => match command {
                None => commands_open = false,
                Some(ControlCommand::Answer { id, answer, reply }) => {
                    match run.protocol.on_answer(&id, answer) {
                        Ok(out) => {
                            let stop = out.stop;
                            run.apply(out);
                            if stop {
                                interrupted = true;
                                ladder = Some(ladder_for(&run.protocol, config.grace));
                            }
                            let _ = reply.send(Ok(()));
                        }
                        Err(e) => {
                            let _ = reply.send(Err(e));
                        }
                    }
                }
                Some(ControlCommand::Interrupt) => {
                    if !run.finished && !interrupted {
                        interrupted = true;
                        let out = run.protocol.on_interrupt();
                        run.apply(out);
                        ladder = Some(ladder_for(&run.protocol, config.grace));
                    }
                }
            },
            line = lines.next() => match line {
                None => eof = true,
                Some(_) if run.finished => {}
                Some(Line::TooLong(bytes)) => run.warn(format!(
                    "a line of {bytes} bytes from {} was too long and was dropped",
                    config.label
                )),
                Some(Line::Text(text)) if text.trim().is_empty() => {}
                Some(Line::Text(text)) => {
                    let out = run.protocol.on_line(&text);
                    let stop = out.stop;
                    run.apply(out);
                    if stop {
                        ladder = Some(ladder_for(&run.protocol, config.grace));
                    }
                }
            },
            status = child.wait(), if exit.is_none() => {
                exit = status.ok();
                drain_until = Some(Instant::now() + DRAIN_AFTER_EXIT);
            }
            _ = run.events.closed() => run.consumer_gone = true,
            _ = async { tokio::time::sleep_until(timer.expect("guarded")).await }, if timer.is_some() => {
                let now = Instant::now();
                if drain_until.is_some_and(|d| d <= now) {
                    break;
                }
                if let Some(l) = ladder.as_mut() {
                    if let Some(signal) = l.steps.get(l.next) {
                        signal_group(pid, *signal);
                    }
                    l.next += 1;
                    if l.next < l.steps.len() {
                        l.at = now + config.grace;
                    } else {
                        ladder = None;
                    }
                }
            }
        }
    }

    // Whatever is left of the child's group goes with it.
    signal_group(pid, Signal::SIGKILL);
    let status = match exit {
        Some(status) => Some(status),
        None => child.wait().await.ok(),
    };
    let _ = tokio::time::timeout(Duration::from_millis(500), tail_task).await;

    if !run.finished && !run.consumer_gone {
        let stderr = String::from_utf8_lossy(&tail.lock().expect("tail lock")).to_string();
        let stderr = config.redactor.redact(stderr.trim());
        let how = status.map_or_else(|| "unknown status".to_string(), |s| s.to_string());
        let mut events = Vec::new();
        if interrupted {
            events.push(DelegateEvent::Finished {
                reason: TurnEndReason::Interrupted,
                detail: Some(format!(
                    "{} ended after the interrupt ({how})",
                    config.label
                )),
            });
        } else {
            let tail = if stderr.is_empty() {
                String::new()
            } else {
                format!(": {stderr}")
            };
            events.push(DelegateEvent::Error {
                kind: ErrorKind::Internal,
                message: format!(
                    "{} exited before the turn finished ({how}){tail}",
                    config.label
                ),
            });
            events.push(DelegateEvent::Finished {
                reason: TurnEndReason::Error,
                detail: None,
            });
        }
        run.apply(Out {
            events,
            ..Out::default()
        });
    }
    // Held-back text (a stream that ended mid-reply) still comes out.
    for event in run.filter.finish() {
        let _ = run.events.send(event);
    }
}
