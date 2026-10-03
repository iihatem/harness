//! Starting a session's agent, as `harness ask` and the interactive terminal both do: the
//! sandbox and its warnings, the permission engine, the tools' context, the model's profile and
//! window, the system prompt, the checkpoints, and the agent itself.

use std::{io::Write, path::Path, sync::Arc};

use harness_config::config::LinuxGitProtection;
use harness_core::{
    agent::{Agent, AgentConfig, Approver, Sandboxes},
    edit_format::EditFormat,
    engine::{EngineConfig, PermissionEngine, RuleSet},
    message::RequestOptions,
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
    pub setup: &'a Arc<Setup>,
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
    /// Where the context window's size comes from, for `/context`.
    pub window_note: String,
    /// What the modes that write lack, for an interactive session that did not start in one to
    /// say when it first switches to one.
    pub write_mode_warning: Option<String>,
    /// Where sandboxed commands can write, which a session's checkpoints must stay out of.
    pub writable: Vec<std::path::PathBuf>,
    /// Keeps the usage ledger and the budgets.
    pub meter: Arc<harness_usage::meter::UsageMeter>,
    /// The language servers, for the session to stop when it ends.
    pub diagnostics: Arc<harness_lsp::LspDiagnostics>,
    /// Makes the models of roles ready: the agent's, and the host's for `/model --role`.
    pub resolver: Arc<crate::routes::CliResolver>,
}

/// The model a session starts on when it is not chosen another way: the `--model` flag, then
/// `[roles] main`, then `model`.
pub fn configured_model(setup: &Setup, flag: Option<String>) -> Option<String> {
    flag.or_else(|| setup.config.roles.main.clone())
        .or_else(|| setup.config.model.clone())
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
    let env_says_none = std::env::var("HARNESS_SANDBOX").as_deref() == Ok("none");
    let sandbox_disabled_by_env = env_says_none && mode != Mode::FullAccess;
    // Write access to `/`, `$HOME` or an ancestor of it would cover the user's dotfiles. Plan and
    // read-only modes keep their read-only sandbox.
    let workspace_too_broad = harness_sandbox::workspace_is_too_broad(&setup.workspace);
    let write_too_broad = mode.fs_access() == FsAccess::WorkspaceWrite && workspace_too_broad;
    let required = setup.config.linux_git_protection == LinuxGitProtection::Required;
    let settings = harness_sandbox::SandboxSettings {
        extra_writable: setup.config.writable_roots.clone(),
        allow_localhost: setup.config.allow_localhost,
        quarantine_dir: Some(setup.paths.data_dir.join("quarantine")),
        require_full_git_protection: required,
    };
    // `harness ask` needs a sandbox for its one mode only. The interactive session looks for one
    // in every mode, since the user may switch to a mode that uses it.
    let look = if interactive {
        !env_says_none
    } else {
        !(mode == Mode::FullAccess || sandbox_disabled_by_env || write_too_broad)
    };
    let detected = if look {
        harness_sandbox::detect(settings.clone())
    } else {
        None
    };
    let (sandboxes, write_warning) =
        sandbox::for_modes(detected.clone(), workspace_too_broad, required);
    let sandbox = sandboxes.for_mode(mode);
    let lack = write_modes(
        write_warning.clone(),
        sandboxes.workspace_write.is_some(),
        env_says_none,
        workspace_too_broad,
        &setup.workspace,
    );
    // Only a mode that writes through the sandbox has anything to warn about.
    let warning = write_warning.filter(|_| matches!(mode, Mode::Ask | Mode::Auto));
    if let Some(warning) = &warning {
        notices.warn(warning);
    }
    // What was said about the sandbox as the session starts.
    let mut said = warning.clone();
    // From here on, however the run is left, the sandbox's session ends: on Linux that ends what
    // sandboxed commands left running, and checks git metadata once more.
    let session_sandbox = if interactive {
        detected
    } else {
        sandbox.clone()
    };
    let sandbox_session = SessionEnd::new(session_sandbox.clone());
    let sandboxed = sandbox.is_some();
    if mode != Mode::FullAccess && !sandboxed && warning.is_none() {
        let text = if sandbox_disabled_by_env {
            "the sandbox is disabled by HARNESS_SANDBOX=none; every shell command will need approval".to_string()
        } else if write_too_broad {
            format!(
                "the workspace {} is your home directory or above, where the sandbox would make your dotfiles writable, so it is off; every shell command will need approval",
                setup.workspace.display()
            )
        } else {
            "no OS sandbox is available; every shell command will need approval".to_string()
        };
        notices.warn(&text);
        said = Some(text);
    }
    // An interactive session that starts in another mode is told when it first switches to one
    // that writes, unless that was said already.
    let write_mode_warning = lack
        .warning
        .filter(|warning| interactive && said.as_ref() != Some(warning));
    let mut read_dirs = setup.config.read_dirs.clone();
    read_dirs.push(output_dir.clone());
    let mut policy = PermissionEngine::new(EngineConfig {
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
    });
    if let Some(why) = lack.unsandboxed {
        policy = policy.with_unsandboxed_reason(why);
    }
    let policy = Arc::new(policy);
    for rule in policy.unknown_rules() {
        notices.warn(&format!(
            "rule `{rule}` names an unknown tool (use bash:, read:, or write:)"
        ));
    }
    let ctx = tool_context(&setup.workspace, sandbox, mode.fs_access()).await;
    // A session that starts without a sandbox may switch to a mode that uses one.
    if ctx.sandbox.is_none()
        && let Some(sandbox) = session_sandbox
    {
        start_sandbox_session(&ctx.workspace, sandbox).await;
    }
    let model = model_setup(setup, &resolved, &cancel).await?;
    for warning in &model.warnings {
        notices.warn(warning);
    }
    let context_window = model.context_window;
    let mut config = AgentConfig::new(
        resolved.id.clone(),
        resolved.model.clone(),
        crate::context::system_prompt(
            setup,
            &prompt::with_edit_section(
                &prompt::base_prompt(mode, sandboxed, interactive),
                model.edit_format,
            ),
            context_window,
            notices,
        ),
        output_dir,
    );
    config.edit_section = Some(prompt::edit_section(model.edit_format));
    config.context_window = context_window;
    config.request = model.request;
    config.text_tool_calls = model.text_tool_calls;
    if let Some(steps) = setup.config.max_steps {
        config.max_steps = steps;
    }
    config.compaction = harness_core::compaction::CompactionConfig {
        threshold: setup.config.compaction.threshold(),
        keep_recent: setup.config.compaction.keep_recent(),
    };
    let writable = exposed_roots(
        interactive,
        sandboxed,
        &sandboxes,
        &settings,
        &setup.workspace,
    );
    let checkpoints = crate::sessions::checkpoints(setup, &session, &writable, notices);
    let writable_roots = writable;
    let budgets = &setup.config.budgets;
    let meter = Arc::new(
        harness_usage::meter::UsageMeter::open(&setup.paths.data_dir, &setup.workspace)
            .with_pricing(crate::pricing::load(setup))
            .with_baseline(setup.config.usage.baseline.clone())
            .with_outcomes(!setup.config.outcomes_disabled)
            .with_budgets(harness_usage::budget::Budgets {
                session_usd: budgets.session_usd,
                daily_usd: budgets.daily_usd,
                monthly_usd: budgets.monthly_usd,
            }),
    );
    let diagnostics = crate::lsp::diagnostics(setup, interactive.then(|| approver.clone()));
    let resolver = crate::routes::CliResolver::new(setup.clone());
    let mut agent = Agent::new(
        resolved.provider,
        harness_tools::builtin_for(model.edit_format),
        policy.clone(),
        approver,
        config,
        ctx,
    )
    .with_redactor(setup.redactor.clone())
    .with_session(session)
    .with_checkpoints(checkpoints)
    .with_meter(meter.clone())
    .with_gates(setup.config.gates.clone())
    .with_diagnostics(diagnostics.clone())
    .with_roles(setup.config.roles.clone())
    .with_escalation(setup.config.escalation_to.clone())
    .with_resolver(resolver.clone());
    if interactive {
        agent = agent.with_sandboxes(sandboxes);
    }
    Some(Started {
        agent,
        sandbox_session,
        policy,
        window_note: model.window_note.into(),
        write_mode_warning,
        writable: writable_roots,
        meter,
        diagnostics,
        resolver,
    })
}

/// What sandboxed commands can write to, which the checkpoint repository must stay out of, since
/// they run without approval and harness's own git reads it outside the sandbox. A run in a mode
/// with a sandbox (`sandboxed`) needs it; an interactive session also needs it whenever the modes
/// that write (ask, auto) have a sandbox, since it can switch to them.
fn exposed_roots(
    interactive: bool,
    sandboxed: bool,
    sandboxes: &Sandboxes,
    settings: &harness_sandbox::SandboxSettings,
    workspace: &Path,
) -> Vec<std::path::PathBuf> {
    if sandboxed || (interactive && sandboxes.workspace_write.is_some()) {
        harness_sandbox::writable_roots(settings, workspace)
    } else {
        Vec::new()
    }
}

/// What the modes that write through the sandbox (ask, auto) lack.
#[derive(Debug)]
struct WriteModes {
    /// Said when the session starts in one of them, or first switches to one.
    warning: Option<String>,
    /// Why they have no sandbox, when it is not that the system has none, for the reason to ask
    /// about each command.
    unsandboxed: Option<&'static str>,
}

/// What the modes that write lack, given the warning [`sandbox::for_modes`] gave for them
/// (`warning`), whether they have a sandbox (`sandboxed`), whether `HARNESS_SANDBOX=none` turned
/// it off (`env_says_none`), and whether `workspace` is too broad to make writable.
fn write_modes(
    warning: Option<String>,
    sandboxed: bool,
    env_says_none: bool,
    too_broad: bool,
    workspace: &Path,
) -> WriteModes {
    if sandboxed {
        return WriteModes {
            warning,
            unsandboxed: None,
        };
    }
    if env_says_none {
        return WriteModes {
            warning: Some(
                "the sandbox is disabled by HARNESS_SANDBOX=none; every shell command will need approval".into(),
            ),
            unsandboxed: Some("the sandbox is disabled by HARNESS_SANDBOX=none"),
        };
    }
    if too_broad {
        return WriteModes {
            warning: Some(format!(
                "the workspace {} is your home directory or above, where the sandbox would make your dotfiles writable, so it is off; every shell command will need approval",
                workspace.display()
            )),
            unsandboxed: Some("the workspace is your home directory or above"),
        };
    }
    match warning {
        // The Linux basic tier, with git metadata protection required.
        Some(warning) => WriteModes {
            warning: Some(warning),
            unsandboxed: Some(
                "git metadata protection is required, but user namespaces are unavailable",
            ),
        },
        None => WriteModes {
            warning: Some(
                "no OS sandbox is available; every shell command will need approval".into(),
            ),
            unsandboxed: None,
        },
    }
}

/// What harness knows of a model before asking it anything: its window and where that comes
/// from, what its profile sets for requests, and what to warn about.
pub struct ModelSetup {
    /// How the model edits files, from its profile.
    pub edit_format: EditFormat,
    pub context_window: u64,
    pub window_note: &'static str,
    pub request: RequestOptions,
    pub text_tool_calls: bool,
    pub warnings: Vec<String>,
}

/// The setup of `resolved`, from its profile and, for a local server, the window the server
/// runs it with. `None` when `cancel` stops the wait for the server's answer.
pub async fn model_setup(
    setup: &Setup,
    resolved: &Resolved,
    cancel: &CancellationToken,
) -> Option<ModelSetup> {
    let local = profiles::is_local(&resolved.id, &resolved.base_url);
    let profile = profiles::resolve(&resolved.id, local, &setup.config.profiles);
    let (running, server) = running_window(setup, resolved, cancel).await?;
    let window_note = window_note(running.tokens(), profile.context_window);
    let window = window::effective_window(&resolved.id, &profile, running, server);
    Some(ModelSetup {
        edit_format: profile.edit_format,
        context_window: window.tokens,
        window_note,
        request: profile.request_options(),
        text_tool_calls: profile.text_tool_calls,
        warnings: window.warnings,
    })
}

/// The window the server really runs `resolved` with, when it is a local server that says (and
/// that server). `None` when `cancel` stops the wait for its answer.
pub async fn running_window(
    setup: &Setup,
    resolved: &Resolved,
    cancel: &CancellationToken,
) -> Option<(window::Running, Option<window::Server>)> {
    let provider = resolved.id.split('/').next().unwrap_or_default();
    let server = window::Server::of(provider, &setup.config.providers);
    let running = match server {
        Some(server) => tokio::select! {
            tokens = window::running_context(server, &resolved.base_url, &resolved.model, PROBE_TIMEOUT, LOAD_TIMEOUT) => tokens,
            _ = cancel.cancelled() => return None,
        },
        None => window::Running::Unknown,
    };
    Some((running, server))
}

/// Where the window comes from: the smaller of what the server runs the model with (`running`)
/// and the model's profile (`profile`), or neither.
fn window_note(running: Option<u64>, profile: Option<u64>) -> &'static str {
    match (running, profile) {
        (Some(running), Some(profile)) if profile < running => "from the model's profile",
        (Some(_), _) => "what the server runs the model with",
        (None, Some(_)) => "from the model's profile",
        (None, None) => "assumed: neither the server nor a profile gives it",
    }
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
        start_sandbox_session(&ctx.workspace, sandbox).await;
    }
    ctx
}

/// Starts `sandbox`'s session for `workspace`, off the async runtime: it may walk the whole
/// workspace. Should it fail, the first command reads what it needs.
async fn start_sandbox_session(workspace: &Path, sandbox: Arc<dyn CommandSandbox>) {
    let workspace = workspace.to_path_buf();
    let _ = tokio::task::spawn_blocking(move || sandbox.start_session(&workspace)).await;
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

    fn settings() -> harness_sandbox::SandboxSettings {
        harness_sandbox::SandboxSettings {
            extra_writable: Vec::new(),
            allow_localhost: false,
            quarantine_dir: None,
            require_full_git_protection: false,
        }
    }

    // Review D I4: an interactive session can switch to ask or auto, whose sandboxed commands
    // write without approval, whatever mode it starts in; the checkpoints stay out of what those
    // commands can write to then too.
    #[test]
    fn checkpoints_stay_out_of_what_any_mode_the_session_can_switch_to_writes() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().canonicalize().unwrap();
        let sandbox: Arc<dyn CommandSandbox> = Arc::new(Recording::default());
        let both = Sandboxes {
            read_only: Some(sandbox.clone()),
            workspace_write: Some(sandbox.clone()),
        };
        // Started in full-access: no sandbox now, but auto is a Shift+Tab away.
        let exposed = exposed_roots(true, false, &both, &settings(), &workspace);
        assert!(exposed.contains(&workspace), "{exposed:?}");
        // `harness ask` stays in its mode.
        assert!(exposed_roots(false, false, &both, &settings(), &workspace).is_empty());
        assert!(exposed_roots(false, true, &both, &settings(), &workspace).contains(&workspace));
        // Without a sandbox for the modes that write, no command writes without approval.
        let read_only = Sandboxes {
            read_only: Some(sandbox),
            workspace_write: None,
        };
        assert!(exposed_roots(true, false, &read_only, &settings(), &workspace).is_empty());
    }

    // Review D M2: what the modes that write lack is said whichever mode the session starts in,
    // and the reason to ask about a command says why it has no sandbox.
    #[test]
    fn what_the_modes_that_write_lack_is_said_by_cause() {
        let workspace = Path::new("/Users/someone");
        let basic = "user namespaces are unavailable (…), so the sandbox can only check git hooks";
        let cases = [
            // (warning for the modes that write, sandbox for them, env says none, too broad)
            (None, true, false, false, None, None),
            (
                None,
                false,
                true,
                false,
                Some(
                    "the sandbox is disabled by HARNESS_SANDBOX=none; every shell command will need approval",
                ),
                Some("the sandbox is disabled by HARNESS_SANDBOX=none"),
            ),
            (
                None,
                false,
                false,
                true,
                Some(
                    "the workspace /Users/someone is your home directory or above, where the sandbox would make your dotfiles writable, so it is off; every shell command will need approval",
                ),
                Some("the workspace is your home directory or above"),
            ),
            (
                Some(
                    "git metadata protection is required (…); every shell command will need approval",
                ),
                false,
                false,
                false,
                Some(
                    "git metadata protection is required (…); every shell command will need approval",
                ),
                Some("git metadata protection is required, but user namespaces are unavailable"),
            ),
            (Some(basic), true, false, false, Some(basic), None),
            (
                None,
                false,
                false,
                false,
                Some("no OS sandbox is available; every shell command will need approval"),
                None,
            ),
        ];
        for (warning, sandboxed, env_says_none, too_broad, said, reason) in cases {
            let lack = write_modes(
                warning.map(String::from),
                sandboxed,
                env_says_none,
                too_broad,
                workspace,
            );
            assert_eq!(lack.warning.as_deref(), said);
            assert_eq!(lack.unsandboxed, reason);
        }
    }

    // The session's model: the flag, then `[roles] main`, then `model`.
    #[test]
    fn the_session_model_is_the_flag_then_roles_main_then_model() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let workspace = workspace.path().canonicalize().unwrap();
        std::fs::create_dir_all(home.path().join("config")).unwrap();
        let configured = |text: &str| {
            std::fs::write(home.path().join("config/config.toml"), text).unwrap();
            crate::host::tests::setup_in(home.path(), &workspace)
        };
        let both = configured("model = \"ollama/a\"\n[roles]\nmain = \"ollama/b\"\n");
        assert_eq!(configured_model(&both, None).as_deref(), Some("ollama/b"));
        assert_eq!(
            configured_model(&both, Some("ollama/c".into())).as_deref(),
            Some("ollama/c")
        );
        let only_model = configured("model = \"ollama/a\"\n");
        assert_eq!(
            configured_model(&only_model, None).as_deref(),
            Some("ollama/a")
        );
        assert_eq!(configured_model(&configured(""), None), None);
    }

    // `/context` says where the window comes from, now that it is no longer assumed.
    #[test]
    fn the_window_note_says_where_the_window_comes_from() {
        assert_eq!(
            window_note(Some(4_096), Some(32_768)),
            "what the server runs the model with"
        );
        assert_eq!(
            window_note(Some(65_536), Some(32_768)),
            "from the model's profile"
        );
        assert_eq!(window_note(None, Some(200_000)), "from the model's profile");
        assert_eq!(
            window_note(Some(8_192), None),
            "what the server runs the model with"
        );
        assert_eq!(
            window_note(None, None),
            "assumed: neither the server nor a profile gives it"
        );
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
            plan: None,
            attribution: None,
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
