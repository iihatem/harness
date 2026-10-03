//! Which language server serves a file, and when one is started, kept, and given up on.

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use harness_core::{
    agent::{ApprovalDecision, ApprovalKind, ApprovalRequest, Approver},
    permission::{Action, FsAccess},
    tool::CommandSandbox,
};
use lsp_types::Diagnostic;
use tokio::process::Command;

use crate::{Check, Client, LspError};

/// The languages harness has servers for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Language {
    Rust,
    TypeScript,
    Python,
    Go,
}

/// A file's language, and the id the server is told it by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Found {
    pub language: Language,
    id: &'static str,
}

impl Found {
    /// The name the settings call the language by: `rust`, `typescript` (also for JavaScript),
    /// `python` or `go`.
    pub fn name(&self) -> &'static str {
        self.language.name()
    }

    /// The `languageId` the server is told the file has.
    pub fn language_id(&self) -> &'static str {
        self.id
    }
}

impl Language {
    pub fn name(self) -> &'static str {
        match self {
            Language::Rust => "rust",
            Language::TypeScript => "typescript",
            Language::Python => "python",
            Language::Go => "go",
        }
    }

    /// The commands tried in turn, the first found on `PATH` winning.
    fn candidates(self) -> &'static [&'static str] {
        match self {
            Language::Rust => &["rust-analyzer"],
            Language::TypeScript => &["typescript-language-server --stdio"],
            Language::Python => &[
                "basedpyright-langserver --stdio",
                "pyright-langserver --stdio",
            ],
            Language::Go => &["gopls"],
        }
    }
}

/// The language of the file at `path`, from its extension.
pub fn language_of(path: &Path) -> Option<Found> {
    let (language, id) = match path.extension()?.to_str()? {
        "rs" => (Language::Rust, "rust"),
        "ts" => (Language::TypeScript, "typescript"),
        "tsx" => (Language::TypeScript, "typescriptreact"),
        "js" => (Language::TypeScript, "javascript"),
        "jsx" => (Language::TypeScript, "javascriptreact"),
        "py" => (Language::Python, "python"),
        "go" => (Language::Go, "go"),
        _ => return None,
    };
    Some(Found { language, id })
}

/// One language's setting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerSetting {
    pub command: Option<String>,
    pub enabled: bool,
}

#[derive(Debug, Clone)]
pub struct Settings {
    pub enabled: bool,
    /// How long to wait for diagnostics after a server's first request.
    pub wait: Duration,
    /// How long for the first, while the server indexes.
    pub first_wait: Duration,
    /// By language name (see [`Found::name`]).
    pub servers: BTreeMap<String, ServerSetting>,
    /// Servers run project code, so they start only in a trusted workspace, or where the user
    /// said yes to the question about them (`allowed`).
    pub trusted: bool,
    /// The stored answer to "start language servers here?": `None` until it was asked.
    pub allowed: Option<bool>,
    /// The directories servers are looked for in.
    pub path: Vec<PathBuf>,
    /// How long a server gets to answer `initialize`.
    pub init_timeout: Duration,
}

/// How a server may be started now: the sandbox bash runs in, if there is one.
pub struct Launch {
    pub sandbox: Option<Arc<dyn CommandSandbox>>,
    pub access: FsAccess,
    /// Whether a server may run with no sandbox: the mode is `full-access`.
    pub unsandboxed_ok: bool,
}

/// What checking a file came to.
#[derive(Debug, Clone, PartialEq)]
pub enum Report {
    /// Nothing to say: there is no server for the file, or it is off, or gone.
    NoServer,
    /// Something to say once, about why there will be no diagnostics.
    Note(String),
    /// The server had not published for the file in time.
    Pending,
    /// What the server published for the file, all severities.
    Checked(Vec<Diagnostic>),
    /// The server published nothing new for this edit; this is the set it last published for the
    /// file.
    Unchanged(Vec<Diagnostic>),
}

/// Asks the user whether language servers may start in the workspace: they run the project's
/// build code. Only an interactive session has one to ask.
#[async_trait]
pub trait ServerConsent: Send + Sync {
    /// Asks, and keeps a yes or a no where the workspace's trust record is kept, so that it is not
    /// asked again. `None` when nobody answered: it is asked at the next edit.
    async fn ask(&self) -> Option<bool>;
}

/// What the note says when there is no sandbox to run a server in.
pub const NO_SANDBOX_NOTE: &str = "diagnostics are off: language servers run only inside the sandbox, and none is available in this mode";

/// What the note says when rust-analyzer is left out because the workspace is read-only.
pub const READ_ONLY_NOTE: &str =
    "diagnostics for Rust are off in read-only mode: rust-analyzer's cargo check writes to target/";

/// Crashes in a session after which a server is not started again.
const MAX_CRASHES: u32 = 2;

/// The question asked of the user: the request's `reason`.
pub const SERVERS_QUESTION: &str =
    "Start language servers here? They run this project's build code.";

/// Asks the session's user the question through its approvals (so that it is shown, and takes
/// keys, as an approval does), and keeps a yes or a no with `remember`.
pub struct AskThroughApprover {
    approver: Arc<dyn Approver>,
    workspace: PathBuf,
    remember: Box<dyn Fn(bool) + Send + Sync>,
}

impl AskThroughApprover {
    pub fn new(
        approver: Arc<dyn Approver>,
        workspace: PathBuf,
        remember: impl Fn(bool) + Send + Sync + 'static,
    ) -> AskThroughApprover {
        AskThroughApprover {
            approver,
            workspace,
            remember: Box::new(remember),
        }
    }
}

#[async_trait]
impl ServerConsent for AskThroughApprover {
    async fn ask(&self) -> Option<bool> {
        let request = ApprovalRequest {
            call_id: "language-servers".into(),
            tool: "language servers".into(),
            arguments: serde_json::Value::Null,
            action: Action::Read(self.workspace.clone()),
            reason: SERVERS_QUESTION.into(),
            kind: ApprovalKind::StartServers,
            kept_for_session: false,
        };
        let yes = match self.approver.decide(&request).await {
            ApprovalDecision::Approve | ApprovalDecision::ApproveForSession => true,
            // Only an answer of `n` or Enter carries no reason: a turn that was stopped does.
            ApprovalDecision::Deny { feedback: None } => false,
            ApprovalDecision::Deny { .. } | ApprovalDecision::Unavailable => return None,
        };
        (self.remember)(yes);
        Some(yes)
    }
}

#[derive(Default)]
struct Slot {
    client: Option<Client>,
    crashes: u32,
    /// Whether a request to the running server was made: the first one waits longer.
    asked: bool,
}

pub struct Manager {
    settings: Settings,
    workspace: PathBuf,
    slots: Mutex<HashMap<Language, Arc<tokio::sync::Mutex<Slot>>>>,
    noted_sandbox: AtomicBool,
    noted_read_only: AtomicBool,
    /// The answer to the question, as stored or as given in this session.
    answer: Mutex<Option<bool>>,
    consent: Option<Arc<dyn ServerConsent>>,
    /// Held while the question is asked, so that files edited together ask it once.
    asking: tokio::sync::Mutex<()>,
}

impl Manager {
    pub fn new(settings: Settings, workspace: PathBuf) -> Manager {
        Manager {
            answer: Mutex::new(settings.allowed),
            settings,
            workspace,
            slots: Mutex::default(),
            noted_sandbox: AtomicBool::new(false),
            noted_read_only: AtomicBool::new(false),
            consent: None,
            asking: tokio::sync::Mutex::new(()),
        }
    }

    /// Lets the manager ask the user whether servers may start in an untrusted workspace.
    pub fn with_consent(mut self, consent: Arc<dyn ServerConsent>) -> Manager {
        self.consent = Some(consent);
        self
    }

    fn answer(&self) -> Option<bool> {
        *self.answer.lock().expect("answer lock")
    }

    /// Whether servers may start: the workspace is trusted, or the user said yes, now or before.
    /// In an untrusted, unanswered workspace the user is asked, once, when someone can be.
    async fn may_start(&self, launch: &Launch) -> Option<Report> {
        if self.settings.trusted || self.answer() == Some(true) {
            return None;
        }
        let no = Some(Report::NoServer);
        if self.answer() == Some(false) {
            return no;
        }
        let Some(consent) = &self.consent else {
            return no;
        };
        // A server that could not be started in this mode is not asked about.
        if launch.sandbox.is_none() && !launch.unsandboxed_ok {
            return Some(once(&self.noted_sandbox, NO_SANDBOX_NOTE));
        }
        let _asking = self.asking.lock().await;
        match self.answer() {
            Some(true) => None,
            Some(false) => no,
            None => match consent.ask().await {
                Some(yes) => {
                    *self.answer.lock().expect("answer lock") = Some(yes);
                    if yes { None } else { no }
                }
                None => no,
            },
        }
    }

    fn slot(&self, language: Language) -> Arc<tokio::sync::Mutex<Slot>> {
        self.slots
            .lock()
            .expect("slots lock")
            .entry(language)
            .or_default()
            .clone()
    }

    /// The program and arguments for `language`: the override in the settings, or the first
    /// candidate found on the path.
    fn command(&self, language: Language) -> Option<(PathBuf, Vec<String>)> {
        let split = |text: &str| -> Option<(String, Vec<String>)> {
            let mut words = text.split_whitespace().map(String::from);
            Some((words.next()?, words.collect()))
        };
        let from = |program: String, args| {
            let program = if program.contains('/') {
                let path = PathBuf::from(&program);
                Some(if path.is_absolute() {
                    path
                } else {
                    self.workspace.join(path)
                })
                .filter(|p| is_executable(p))
            } else {
                self.settings
                    .path
                    .iter()
                    .map(|dir| dir.join(&program))
                    .find(|p| is_executable(p))
            };
            program.map(|program| (program, args))
        };
        if let Some(text) = self
            .settings
            .servers
            .get(language.name())
            .and_then(|s| s.command.as_deref())
        {
            let (program, args) = split(text)?;
            return from(program, args);
        }
        language.candidates().iter().find_map(|text| {
            let (program, args) = split(text)?;
            from(program, args)
        })
    }

    /// Tells the server for the language of `file` that it changed, and waits for its
    /// diagnostics. Starts the server first when it is not running, if it may be.
    pub async fn check(&self, file: &Path, launch: &Launch) -> Report {
        let Some(found) = language_of(file) else {
            return Report::NoServer;
        };
        if !self.settings.enabled
            || self
                .settings
                .servers
                .get(found.name())
                .is_some_and(|s| !s.enabled)
        {
            return Report::NoServer;
        }
        let Some((program, args)) = self.command(found.language) else {
            return Report::NoServer;
        };
        // rust-analyzer's `cargo check` writes `target/`, which read-only access forbids.
        if found.language == Language::Rust && launch.access == FsAccess::ReadOnly {
            return once(&self.noted_read_only, READ_ONLY_NOTE);
        }
        if let Some(report) = self.may_start(launch).await {
            return report;
        }
        let Ok(text) = std::fs::read_to_string(file) else {
            return Report::NoServer;
        };
        let slot = self.slot(found.language);
        let mut slot = slot.lock().await;
        if slot.crashes >= MAX_CRASHES {
            return Report::NoServer;
        }
        let name = found.name();
        if slot.client.is_none() {
            let command = match self.launch(&program, &args, launch) {
                Ok(Some(command)) => command,
                Ok(None) => return once(&self.noted_sandbox, NO_SANDBOX_NOTE),
                Err(_) => return failed(&mut slot, name),
            };
            match Client::start(command, &self.workspace, self.settings.init_timeout).await {
                Ok(client) => {
                    slot.client = Some(client);
                    slot.asked = false;
                }
                Err(_) => return failed(&mut slot, name),
            }
        }
        let wait = if slot.asked {
            self.settings.wait
        } else {
            self.settings.first_wait.max(self.settings.wait)
        };
        slot.asked = true;
        let client = slot.client.as_ref().expect("a client was started");
        match client.check(file, found.language_id(), &text, wait).await {
            Ok(Check::Published(diagnostics)) => Report::Checked(diagnostics),
            Ok(Check::Unchanged(diagnostics)) => Report::Unchanged(diagnostics),
            Ok(Check::Pending) => Report::Pending,
            Ok(Check::Exited) | Err(LspError::Exited) => {
                slot.client = None;
                failed(&mut slot, name)
            }
            Err(_) => Report::Pending,
        }
    }

    /// The command that runs the server, in the sandbox when there is one. `None` when there is
    /// none and the mode does not allow running without.
    fn launch(
        &self,
        program: &Path,
        args: &[String],
        launch: &Launch,
    ) -> std::io::Result<Option<Command>> {
        let program = program.to_string_lossy();
        match &launch.sandbox {
            Some(sandbox) => {
                let args: Vec<&str> = args.iter().map(String::as_str).collect();
                sandbox
                    .command(launch.access, &self.workspace, &program, &args)
                    .map(|mut command| {
                        command.current_dir(&self.workspace);
                        Some(command)
                    })
            }
            None if launch.unsandboxed_ok => {
                let mut command = Command::new(&*program);
                command
                    .args(args)
                    .current_dir(&self.workspace)
                    .process_group(0);
                Ok(Some(command))
            }
            None => Ok(None),
        }
    }

    /// Stops every server, without waiting for them, and forgets what went wrong: a new session
    /// starts afresh.
    pub fn reset(&self) {
        self.noted_sandbox.store(false, Ordering::SeqCst);
        let slots: Vec<_> = self.slots.lock().expect("slots lock").drain().collect();
        for (_, slot) in slots {
            if let Ok(mut slot) = slot.try_lock()
                && let Some(client) = slot.client.take()
            {
                tokio::spawn(client.shutdown());
            }
        }
    }

    /// Stops every server and waits for them, as a session that ends does.
    pub async fn shutdown(&self) {
        let slots: Vec<_> = self.slots.lock().expect("slots lock").drain().collect();
        for (_, slot) in slots {
            let client = slot.lock().await.client.take();
            if let Some(client) = client {
                client.shutdown().await;
            }
        }
    }
}

/// A server that crashed, or could not be started. The second time is the last: it is said once
/// that there will be no diagnostics for the language until a new session, so that the silence is
/// not taken for a clean file.
fn failed(slot: &mut Slot, language: &str) -> Report {
    slot.crashes += 1;
    if slot.crashes == MAX_CRASHES {
        Report::Note(format!(
            "the {language} language server stopped; no diagnostics for {language} files until /new"
        ))
    } else {
        Report::NoServer
    }
}

fn once(flag: &AtomicBool, note: &str) -> Report {
    if flag.swap(true, Ordering::SeqCst) {
        Report::NoServer
    } else {
        Report::Note(note.into())
    }
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}
