//! The backend trait: start a run, stream its events, answer approvals, interrupt.

use std::{
    ffi::OsString,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use harness_core::{permission::Mode, redact::Redactor};
use tokio::sync::{mpsc, oneshot};

use crate::{
    env::AuthMode,
    event::{ApprovalAnswer, DelegateEvent},
};

/// Which program a backend drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BackendKind {
    Claude,
    Codex,
}

impl BackendKind {
    /// The program's name on `PATH`.
    pub fn binary(self) -> &'static str {
        match self {
            BackendKind::Claude => "claude",
            BackendKind::Codex => "codex",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DelegateError {
    #[error("{0} was not found on PATH: install it and sign in with it first")]
    NotFound(&'static str),
    #[error("could not read the version of {binary}: `--version` printed {output:?}")]
    VersionUnreadable { binary: String, output: String },
    #[error(
        "{binary} {found} is outside the tested range {range}: upgrade or downgrade it, or allow untested versions"
    )]
    VersionOutOfRange {
        binary: &'static str,
        found: String,
        range: String,
    },
    #[error("full-access delegation needs an explicit confirmation for this run")]
    FullAccessUnconfirmed,
    #[error("could not start {binary}: {source}")]
    Spawn {
        binary: String,
        source: std::io::Error,
    },
    #[error("the run has ended")]
    RunEnded,
    #[error("no approval with id {0} is waiting")]
    UnknownApproval(String),
}

/// What to run.
#[derive(Debug, Clone)]
pub struct StartRequest {
    /// The task, written to the child's input (never to its command line).
    pub task: String,
    pub model: Option<String>,
    /// harness's mode; mapped onto the child's (see the plan).
    pub mode: Mode,
    /// `Mode::FullAccess` is refused unless this is true.
    pub full_access_confirmed: bool,
    pub workspace: PathBuf,
    /// More directories the child may use.
    pub extra_dirs: Vec<PathBuf>,
    /// The child's session id to continue (`--resume`, `thread/resume`).
    pub resume: Option<String>,
    pub auth: AuthMode,
    /// Applied to everything the child relays.
    pub redactor: Arc<Redactor>,
    /// The binary to run; else `PATH` is searched.
    pub binary: Option<PathBuf>,
    /// The `PATH` to search, else the process's.
    pub path_var: Option<OsString>,
    /// Run a version outside the tested range.
    pub allow_untested: bool,
    /// How long an interrupt waits at each step before the next, harsher one.
    pub grace: Duration,
    /// For a resumed Claude session: the session's cost total before this run.
    pub cost_baseline_usd: f64,
}

impl StartRequest {
    pub fn new(task: impl Into<String>, workspace: impl Into<PathBuf>) -> StartRequest {
        StartRequest {
            task: task.into(),
            model: None,
            mode: Mode::Ask,
            full_access_confirmed: false,
            workspace: workspace.into(),
            extra_dirs: Vec::new(),
            resume: None,
            auth: AuthMode::Subscription,
            redactor: Arc::new(Redactor::default()),
            binary: None,
            path_var: None,
            allow_untested: false,
            grace: Duration::from_secs(5),
            cost_baseline_usd: 0.0,
        }
    }
}

/// What a [`RunControl`] asks of the driver.
pub(crate) enum Command {
    Answer {
        id: String,
        answer: ApprovalAnswer,
        reply: oneshot::Sender<Result<(), DelegateError>>,
    },
    Interrupt,
}

/// Answers approvals, interrupts, and knows the child's session id. Cloneable, so one task can
/// read the events while another answers.
#[derive(Clone)]
pub struct RunControl {
    pub(crate) tx: mpsc::UnboundedSender<Command>,
    pub(crate) session: Arc<Mutex<Option<String>>>,
}

impl RunControl {
    pub async fn answer(&self, id: &str, answer: ApprovalAnswer) -> Result<(), DelegateError> {
        let (reply, got) = oneshot::channel();
        self.tx
            .send(Command::Answer {
                id: id.to_string(),
                answer,
                reply,
            })
            .map_err(|_| DelegateError::RunEnded)?;
        got.await.map_err(|_| DelegateError::RunEnded)?
    }

    /// Ask the child to stop its turn. It escalates (a signal, a harsher one, a kill) if the
    /// child does not; the stream ends with `Finished`.
    pub fn interrupt(&self) -> Result<(), DelegateError> {
        self.tx
            .send(Command::Interrupt)
            .map_err(|_| DelegateError::RunEnded)
    }

    /// The child's own id for the session, once it has said (for resume).
    pub fn session_id(&self) -> Option<String> {
        self.session.lock().expect("session lock").clone()
    }
}

/// A started run.
pub struct DelegateRun {
    /// Ends after `Finished`. Unbounded: the driver must keep reading the child's output (Claude
    /// waits only 30 s for a slow reader), so a slow consumer costs memory, not a stall.
    pub events: mpsc::UnboundedReceiver<DelegateEvent>,
    pub control: RunControl,
}

#[async_trait]
pub trait DelegateBackend: Send + Sync {
    fn kind(&self) -> BackendKind;

    /// Check the binary and its version, spawn it, and start the task.
    async fn start(&self, request: StartRequest) -> Result<DelegateRun, DelegateError>;
}
