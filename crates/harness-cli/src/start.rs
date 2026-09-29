//! Starting a session's agent, as `harness ask` and the interactive terminal both do: the
//! sandbox and its warnings, the permission engine, the tools' context, the model's profile and
//! window, the system prompt, the checkpoints, and the agent itself.

use std::{io::Write, path::Path, sync::Arc};

use harness_config::config::LinuxGitProtection;
use harness_core::{
    agent::{Agent, AgentConfig, Approver},
    engine::{EngineConfig, PermissionEngine, RuleSet},
    permission::{FsAccess, Mode},
    session::Session,
    tool::{CommandSandbox, ToolContext},
};
use harness_providers::{
    profiles,
    registry::Resolved,
    window::{self, LOAD_TIMEOUT, PROBE_TIMEOUT},
};
use tokio_util::sync::CancellationToken;

use crate::{notices::Notices, prompt, sandbox, setup::Setup, term::terminal_safe_text};

/// What a frontend asks for.
pub struct Request<'a> {
    pub setup: &'a Setup,
    pub mode: Mode,
    pub model: Resolved,
    pub session: Session,
    pub approver: Arc<dyn Approver>,
    /// Whether a user at a terminal answers approvals.
    pub interactive: bool,
    /// Names the run's tool-output directory (and `--debug`'s log).
    pub run_id: String,
    /// Stops the start while it waits for a local server's answer.
    pub cancel: CancellationToken,
}

/// A started agent, and what the frontend needs next to it.
pub struct Started {
    pub agent: Agent,
    /// Ends the sandbox's session; drop the agent first.
    pub sandbox_session: SessionEnd,
    pub policy: Arc<PermissionEngine>,
}

/// A new run's id: its start time and the process id.
pub fn run_id() -> String {
    format!(
        "run-{}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        std::process::id()
    )
}

/// Starts the agent for `request`, printing startup warnings to stderr through `notices`. `None`
/// when cancelled while a local server was asked for its context window.
pub async fn start(request: Request<'_>, notices: &mut Notices) -> Option<Started> {
    let Request {
        setup,
        mode,
        model: resolved,
        session,
        approver,
        interactive,
        run_id,
        cancel,
    } = request;
    if mode == Mode::FullAccess {
        notices.warn("full-access mode: commands run without approval or sandbox");
    }
    let output_dir = setup.paths.state_dir.join("tool-output").join(run_id);
    let sandbox_disabled_by_env =
        std::env::var("HARNESS_SANDBOX").as_deref() == Ok("none") && mode != Mode::FullAccess;
    // Write access to `/`, `$HOME` or an ancestor of it would cover the user's dotfiles. Plan and
    // read-only modes keep their read-only sandbox.
    let workspace_too_broad = mode.fs_access() == FsAccess::WorkspaceWrite
        && harness_sandbox::workspace_is_too_broad(&setup.workspace);
    let required = setup.config.linux_git_protection == LinuxGitProtection::Required;
    let settings = harness_sandbox::SandboxSettings {
        extra_writable: setup.config.writable_roots.clone(),
        allow_localhost: setup.config.allow_localhost,
        quarantine_dir: Some(setup.paths.data_dir.join("quarantine")),
        require_full_git_protection: required,
    };
    let detected = if mode == Mode::FullAccess || sandbox_disabled_by_env || workspace_too_broad {
        None
    } else {
        harness_sandbox::detect(settings.clone())
    };
    let choice = sandbox::choose(detected, mode.fs_access(), required);
    if let Some(warning) = &choice.warning {
        notices.warn(warning);
    }
    let sandbox = choice.sandbox;
    // From here on, however the run is left, the sandbox's session ends: on Linux that ends what
    // sandboxed commands left running, and checks git metadata once more.
    let sandbox_session = SessionEnd::new(sandbox.clone());
    let sandboxed = sandbox.is_some();
    if mode != Mode::FullAccess && !sandboxed && choice.warning.is_none() {
        if sandbox_disabled_by_env {
            notices.warn(
                "the sandbox is disabled by HARNESS_SANDBOX=none; every shell command will need approval",
            );
        } else if workspace_too_broad {
            notices.warn(&format!(
                "the workspace {} is your home directory or above, where the sandbox would make your dotfiles writable, so it is off; every shell command will need approval",
                setup.workspace.display()
            ));
        } else {
            notices.warn("no OS sandbox is available; every shell command will need approval");
        }
    }
    let mut read_dirs = setup.config.read_dirs.clone();
    read_dirs.push(output_dir.clone());
    let policy = Arc::new(PermissionEngine::new(EngineConfig {
        mode,
        workspace: setup.workspace.clone(),
        read_dirs,
        rules: RuleSet {
            allow: setup.config.allow.clone(),
            deny: setup.config.deny.clone(),
            confirm: setup.config.confirm.clone(),
        },
        sandbox_available: sandboxed,
        writes_need_approval: workspace_too_broad,
    }));
    for rule in policy.unknown_rules() {
        notices.warn(&format!(
            "rule `{rule}` names an unknown tool (use bash:, read:, or write:)"
        ));
    }
    let ctx = tool_context(&setup.workspace, sandbox, mode.fs_access()).await;
    let local = profiles::is_local(&resolved.id, &resolved.base_url);
    let profile = profiles::resolve(&resolved.id, local, &setup.config.profiles);
    // The window the server really runs the model with, when it is a local server that says.
    let provider = resolved.id.split('/').next().unwrap_or_default();
    let server = window::Server::of(provider, &setup.config.providers);
    let running = match server {
        Some(server) => tokio::select! {
            tokens = window::running_context(server, &resolved.base_url, &resolved.model, PROBE_TIMEOUT, LOAD_TIMEOUT) => tokens,
            _ = cancel.cancelled() => return None,
        },
        None => window::Running::Unknown,
    };
    let window = window::effective_window(&resolved.id, &profile, running, server);
    for warning in &window.warnings {
        notices.warn(warning);
    }
    let context_window = window.tokens;
    let mut config = AgentConfig::new(
        resolved.id.clone(),
        resolved.model.clone(),
        crate::context::system_prompt(
            setup,
            &prompt::base_prompt(mode, sandboxed, interactive),
            context_window,
            notices,
        ),
        output_dir,
    );
    config.context_window = context_window;
    config.request = profile.request_options();
    config.text_tool_calls = profile.text_tool_calls;
    if let Some(steps) = setup.config.max_steps {
        config.max_steps = steps;
    }
    config.compaction = harness_core::compaction::CompactionConfig {
        threshold: setup.config.compaction.threshold(),
        keep_recent: setup.config.compaction.keep_recent(),
    };
    // Sandboxed commands run without approval: what they can write to must not hold the
    // checkpoint repository, which harness's own git reads outside the sandbox.
    let writable = if sandboxed {
        harness_sandbox::writable_roots(&settings, &setup.workspace)
    } else {
        Vec::new()
    };
    let checkpoints = crate::sessions::checkpoints(setup, &session, &writable, notices);
    let agent = Agent::new(
        resolved.provider,
        harness_tools::builtin(),
        policy.clone(),
        approver,
        config,
        ctx,
    )
    .with_redactor(setup.redactor.clone())
    .with_session(session)
    .with_checkpoints(checkpoints);
    Some(Started {
        agent,
        sandbox_session,
        policy,
    })
}

/// Ends the run: first the agent, which releases the session file, then the sandbox's session,
/// which can take a few seconds, so a `-c` started once the answer prints finds the file free.
pub fn end_run(agent: Agent, sandbox_session: SessionEnd) {
    drop(agent);
    sandbox_session.end();
}

/// Ends the sandbox's session ([`CommandSandbox::end_session`]) once: when [`end`](Self::end) is
/// called, or when dropped, so that an early return or a panic ends it too. What that says goes
/// to stderr, escaped; the exit code does not depend on it. It takes a few seconds at most.
pub struct SessionEnd(Option<Arc<dyn CommandSandbox>>);

impl SessionEnd {
    pub fn new(sandbox: Option<Arc<dyn CommandSandbox>>) -> SessionEnd {
        SessionEnd(sandbox)
    }

    /// Ends the session now.
    pub fn end(mut self) {
        self.run();
    }

    fn run(&mut self) {
        let Some(sandbox) = self.0.take() else {
            return;
        };
        if let Some(text) = sandbox.end_session() {
            let _ = write!(std::io::stderr().lock(), "{}", terminal_safe_text(&text));
        }
    }
}

impl Drop for SessionEnd {
    fn drop(&mut self) {
        self.run();
    }
}

/// The tools' context, with the sandbox's session started first: before the agent runs, so what
/// the sandbox reads from the workspace (on Linux, the ignore rules the git-metadata guard scans
/// with) is what was there before any tool could change it.
pub async fn tool_context(
    workspace: &Path,
    sandbox: Option<Arc<dyn CommandSandbox>>,
    access: FsAccess,
) -> ToolContext {
    let ctx = ToolContext::new(workspace).with_sandbox(sandbox, access);
    if let Some(sandbox) = ctx.sandbox.clone() {
        let workspace = ctx.workspace.clone();
        // It may walk the whole workspace. Should it fail, the first command reads what it needs.
        let _ = tokio::task::spawn_blocking(move || sandbox.start_session(&workspace)).await;
    }
    ctx
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex;

    use harness_core::{agent::NonInteractive, tool::Tool};

    /// Runs commands directly, and records what harness asks of it.
    #[derive(Debug, Default)]
    struct Recording {
        log: Mutex<Vec<String>>,
    }

    impl Recording {
        fn log(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }
    }

    impl CommandSandbox for Recording {
        fn name(&self) -> &'static str {
            "recording"
        }

        fn command(
            &self,
            _access: FsAccess,
            _workspace: &Path,
            program: &str,
            args: &[&str],
        ) -> std::io::Result<tokio::process::Command> {
            self.log.lock().unwrap().push("command".into());
            let mut cmd = tokio::process::Command::new(program);
            cmd.args(args).process_group(0);
            Ok(cmd)
        }

        fn is_denial(&self, _exit_code: Option<i32>, _output: &str) -> bool {
            false
        }

        fn start_session(&self, workspace: &Path) {
            self.log
                .lock()
                .unwrap()
                .push(format!("start_session {}", workspace.display()));
        }

        fn end_session(&self) -> Option<String> {
            self.log.lock().unwrap().push("end_session".into());
            Some("ended\n".into())
        }
    }

    fn ended(sandbox: &Recording) -> usize {
        sandbox
            .log()
            .iter()
            .filter(|entry| *entry == "end_session")
            .count()
    }

    #[test]
    fn the_sandbox_session_ends_once_when_the_turn_ends() {
        let sandbox = Arc::new(Recording::default());
        let shared: Arc<dyn CommandSandbox> = sandbox.clone();
        let session = SessionEnd::new(Some(shared));
        assert_eq!(ended(&sandbox), 0);
        session.end();
        assert_eq!(ended(&sandbox), 1);
    }

    #[test]
    fn the_sandbox_session_ends_however_run_is_left() {
        // An early return drops the guard; so does a panic.
        let sandbox = Arc::new(Recording::default());
        let shared: Arc<dyn CommandSandbox> = sandbox.clone();
        drop(SessionEnd::new(Some(shared)));
        assert_eq!(ended(&sandbox), 1);
        let shared: Arc<dyn CommandSandbox> = sandbox.clone();
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _session = SessionEnd::new(Some(shared));
            panic!("the turn failed");
        }));
        assert!(unwound.is_err());
        assert_eq!(ended(&sandbox), 2);
        // Without a sandbox there is nothing to end.
        SessionEnd::new(None).end();
    }

    #[tokio::test]
    async fn the_sandbox_session_starts_before_the_first_tool_call() {
        let dir = tempfile::tempdir().unwrap();
        let sandbox = Arc::new(Recording::default());
        let shared: Arc<dyn CommandSandbox> = sandbox.clone();
        let ctx = tool_context(dir.path(), Some(shared), FsAccess::WorkspaceWrite).await;
        let started = format!("start_session {}", ctx.workspace.display());
        assert_eq!(sandbox.log(), [started.as_str()]);
        let out = harness_tools::BashTool
            .run(serde_json::json!({"command": "echo hi"}), &ctx)
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(sandbox.log(), [started.as_str(), "command"]);
    }

    #[tokio::test]
    async fn without_a_sandbox_there_is_no_session_to_start() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = tool_context(dir.path(), None, FsAccess::WorkspaceWrite).await;
        assert!(ctx.sandbox.is_none());
    }

    /// Records, when its session ends, whether the harness session file at `path` could be
    /// opened then.
    #[derive(Debug)]
    struct LockProbe {
        path: std::path::PathBuf,
        free: Mutex<Option<bool>>,
    }

    impl CommandSandbox for LockProbe {
        fn name(&self) -> &'static str {
            "lock probe"
        }

        fn command(
            &self,
            _access: FsAccess,
            _workspace: &Path,
            program: &str,
            _args: &[&str],
        ) -> std::io::Result<tokio::process::Command> {
            Ok(tokio::process::Command::new(program))
        }

        fn is_denial(&self, _exit_code: Option<i32>, _output: &str) -> bool {
            false
        }

        fn start_session(&self, _workspace: &Path) {}

        fn end_session(&self) -> Option<String> {
            let free = harness_core::session::Session::open(&self.path).is_ok();
            *self.free.lock().unwrap() = Some(free);
            None
        }
    }

    // Review D M9: ending the sandbox's session takes a few seconds on Linux; the session file
    // is released before, so a `-c` started as soon as the answer prints can use it.
    #[test]
    fn the_session_is_released_before_the_sandbox_session_ends() {
        use harness_core::{
            message::Message,
            session::{EntryKind, Session},
            testing::MockProvider,
            tool::ToolRegistry,
        };
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::create(&dir.path().join("sessions"), dir.path());
        session.append(EntryKind::Message {
            message: Message::User {
                content: "hi".into(),
            },
            display: None,
            note: false,
        });
        let path = session.path().unwrap().to_path_buf();
        let policy = Arc::new(PermissionEngine::new(EngineConfig {
            mode: Mode::Auto,
            workspace: dir.path().to_path_buf(),
            read_dirs: vec![],
            rules: RuleSet::default(),
            sandbox_available: false,
            writes_need_approval: false,
        }));
        let agent = Agent::new(
            MockProvider::new(vec![]),
            ToolRegistry::new(vec![]),
            policy,
            Arc::new(NonInteractive),
            AgentConfig::new("mock/m", "m", "system", dir.path().join("out")),
            ToolContext::new(dir.path()),
        )
        .with_session(session);
        let probe = Arc::new(LockProbe {
            path,
            free: Mutex::new(None),
        });
        let shared: Arc<dyn CommandSandbox> = probe.clone();
        end_run(agent, SessionEnd::new(Some(shared)));
        assert_eq!(*probe.free.lock().unwrap(), Some(true));
    }
}
