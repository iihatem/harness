use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::{
    message::ToolSpec,
    permission::{Action, FsAccess, resolve_path},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
    /// The command failed because the OS sandbox blocked it (the agent may offer an unsandboxed re-run).
    pub sandbox_denied: bool,
    /// The sandbox's git-metadata guard undid something this command did. The command counts as
    /// blocked, and it must never be re-run outside the sandbox.
    pub guard_blocked: bool,
    /// The call did not run: a rule denied it, it needed an approval it did not get, or the user
    /// refused it.
    pub blocked: bool,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        ToolOutput {
            content: content.into(),
            is_error: false,
            sandbox_denied: false,
            guard_blocked: false,
            blocked: false,
        }
    }

    pub fn error(content: impl Into<String>) -> Self {
        ToolOutput {
            content: content.into(),
            is_error: true,
            sandbox_denied: false,
            guard_blocked: false,
            blocked: false,
        }
    }

    /// An error for a call that did not run because it was denied, or not approved.
    pub fn refused(content: impl Into<String>) -> Self {
        ToolOutput {
            blocked: true,
            ..ToolOutput::error(content)
        }
    }
}

/// Remembers the content hash of each file the model has read, so writes can detect stale views.
#[derive(Debug, Default)]
pub struct ReadTracker {
    hashes: Mutex<HashMap<PathBuf, String>>,
}

impl ReadTracker {
    fn hash(bytes: &[u8]) -> String {
        hex::encode(Sha256::digest(bytes))
    }

    pub fn record(&self, path: &Path, bytes: &[u8]) {
        self.hashes
            .lock()
            .expect("tracker lock")
            .insert(path.to_path_buf(), Self::hash(bytes));
    }

    /// `Ok` if the file was read in this session and is unchanged on disk since.
    pub fn check_fresh(&self, path: &Path, current: &[u8]) -> Result<(), String> {
        match self.hashes.lock().expect("tracker lock").get(path) {
            None => Err(format!(
                "{} exists but has not been read in this session; read it first",
                path.display()
            )),
            Some(hash) if *hash != Self::hash(current) => Err(format!(
                "{} changed on disk since it was read; read it again",
                path.display()
            )),
            Some(_) => Ok(()),
        }
    }
}

/// What every tool call can see: the workspace, read history, and the turn's cancellation token.
#[derive(Debug, Clone)]
pub struct ToolContext {
    pub workspace: PathBuf,
    pub tracker: Arc<ReadTracker>,
    pub cancel: CancellationToken,
    /// Sandbox for shell commands; `None` runs them directly (full-access, or no sandbox available).
    pub sandbox: Option<Arc<dyn CommandSandbox>>,
    /// What sandboxed commands may write.
    pub access: FsAccess,
    /// Set by the agent for a user-approved re-run outside the sandbox.
    pub unsandboxed: bool,
}

impl ToolContext {
    pub fn new(workspace: &Path) -> Self {
        ToolContext {
            workspace: workspace
                .canonicalize()
                .unwrap_or_else(|_| workspace.to_path_buf()),
            tracker: Arc::default(),
            cancel: CancellationToken::new(),
            sandbox: None,
            access: FsAccess::WorkspaceWrite,
            unsandboxed: false,
        }
    }

    /// Resolves a path argument against the workspace, following symlinks.
    pub fn resolve(&self, path: &str) -> PathBuf {
        resolve_path(&self.workspace, Path::new(path))
    }

    pub fn with_sandbox(
        mut self,
        sandbox: Option<Arc<dyn CommandSandbox>>,
        access: FsAccess,
    ) -> Self {
        self.sandbox = sandbox;
        self.access = access;
        self
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    /// What running this call would do, for the permission check. Called after schema validation.
    fn action(&self, args: &Value, ctx: &ToolContext) -> Action;
    async fn run(&self, args: Value, ctx: &ToolContext) -> ToolOutput;
    /// Everything running this call would do, when it is more than one thing: each is checked
    /// on its own, and the strictest answer decides. The default is the one [`action`](Self::action).
    fn actions(&self, args: &Value, ctx: &ToolContext) -> Vec<Action> {
        vec![self.action(args, ctx)]
    }
    /// The files a successful call of this tool changed, for the checks that follow an edit
    /// (verification gates, language-server diagnostics). Tools that change no file return none.
    fn changed_paths(&self, _args: &Value, _ctx: &ToolContext) -> Vec<PathBuf> {
        Vec::new()
    }
}

/// What a [`CommandGuard`] did around one command, for the tool output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardReport {
    /// Appended to the command's output, so the model and the user both see it.
    pub message: String,
    /// The guard undid something the command did: the command counts as blocked.
    pub blocked: bool,
}

/// Checks and repairs protected git metadata around one sandboxed command. Implemented by
/// `harness-sandbox` on Linux.
pub trait CommandGuard: Send {
    /// Called right after the command was spawned, with its process id, so the guard never
    /// reaps the process the caller waits for. The default does nothing.
    fn started(&mut self, pid: u32) {
        let _ = pid;
    }
    /// Called once the command has ended: it exited, timed out, was interrupted, or never started.
    fn finish(self: Box<Self>) -> Option<GuardReport>;
}

/// A sandboxed command, and the guard to finish once it has ended.
pub struct SandboxedCommand {
    pub command: tokio::process::Command,
    pub guard: Option<Box<dyn CommandGuard>>,
}

/// How a sandbox protects git metadata (hooks, config, `commondir`, `.harness/`, a top-level
/// `HEAD`) inside a writable workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitProtection {
    /// Writes to protected git metadata fail: Seatbelt on macOS, the full tier on Linux.
    Full,
    /// Protected git metadata is checked after each command, and changes are moved to quarantine
    /// or restored: the Linux basic tier. `reason` says why the full tier is unavailable.
    Basic { reason: String },
}

/// Wraps shell commands so they run inside an OS sandbox. Implemented by `harness-sandbox`.
pub trait CommandSandbox: Send + Sync + std::fmt::Debug {
    /// Mechanism name for messages, e.g. `seatbelt` or `landlock+seccomp`.
    fn name(&self) -> &'static str;
    /// A command that runs `program args…` in the sandbox with `access` for `workspace`, already set
    /// up to lead its own process group. The caller sets the working directory, stdio, and environment.
    fn command(
        &self,
        access: FsAccess,
        workspace: &Path,
        program: &str,
        args: &[&str],
    ) -> std::io::Result<tokio::process::Command>;
    /// Whether a failed command's output looks like the sandbox blocked it.
    fn is_denial(&self, exit_code: Option<i32>, output: &str) -> bool;
    /// Called once when the session starts, before the agent runs, so the sandbox can read what
    /// it needs from `workspace` before any tool can have changed it. The default does nothing.
    fn start_session(&self, workspace: &Path) {
        let _ = workspace;
    }
    /// Ends the sandbox's session, as harness exits however it exits: on Linux, it ends the
    /// processes sandboxed commands left running and checks git metadata once more, undoing what
    /// they changed (but not the user's changes, where there were none). What it did, for stderr,
    /// if anything. It returns within a few seconds. The default does nothing.
    fn end_session(&self) -> Option<String> {
        None
    }
    /// [`command`](Self::command), plus a guard already started for it. The caller must finish the
    /// guard after the command ends, however it ends. The default starts no guard.
    fn prepare(
        &self,
        access: FsAccess,
        workspace: &Path,
        program: &str,
        args: &[&str],
    ) -> std::io::Result<SandboxedCommand> {
        Ok(SandboxedCommand {
            command: self.command(access, workspace, program, args)?,
            guard: None,
        })
    }
    /// How git metadata is protected in workspace-write mode. The default is
    /// [`GitProtection::Full`].
    fn git_protection(&self) -> GitProtection {
        GitProtection::Full
    }
    /// Why this sandbox would refuse, now, a command with `access`, if it would: on Linux, once a
    /// session that requires full git-metadata protection dropped to the basic tier, a command
    /// that may write. The agent then asks whether to run the command outside the sandbox, as a
    /// session without one asks for every command. The default is `None`.
    fn cannot_run(&self, access: FsAccess) -> Option<String> {
        let _ = access;
        None
    }
}

/// Tools in a fixed order, so tool definitions are byte-identical across requests.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new(tools: Vec<Arc<dyn Tool>>) -> Self {
        ToolRegistry { tools }
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.iter().map(|t| t.spec()).collect()
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.iter().find(|t| t.spec().name == name).cloned()
    }
}
