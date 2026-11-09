//! Child agents in a session: the CLI's side of `harness_core::agents::AgentFactory`.
//!
//! A child shares its parent's provider, tools, sandbox, gates and settings unless its spec names
//! another model, a narrower set of tools or a prompt of its own. Its permissions, approver,
//! meter and session come from the supervisor.

use std::{path::PathBuf, sync::Arc};

use async_trait::async_trait;
use harness_core::{
    agent::{Agent, AgentConfig, Sandboxes},
    agents::{AgentFactory, AgentSpec, ChildSetup, CurrentModel, Endpoint, ParentModel},
    edit_format::EditFormat,
    provider::Provider,
    role::ModelResolver,
    tool::{Tool, ToolContext, ToolRegistry},
};
use tokio_util::sync::CancellationToken;

use crate::{routes::CliResolver, setup::Setup};

/// Makes child agents from the session's own pieces.
pub struct CliFactory {
    setup: Arc<Setup>,
    resolver: Arc<CliResolver>,
    /// The parent's settings other than its model (steps, limits, retries, compaction).
    template: AgentConfig,
    /// The model the parent is on now, which a child that names none of its own is built from:
    /// published by the parent agent as it changes, and the one it started on until it has.
    current: ParentModel,
    started_on: CurrentModel,
    /// The sandbox for each mode. A child that runs in a mode with none asks about every shell
    /// command, as its parent would.
    sandboxes: Sandboxes,
    /// What sandboxed commands can write to, which a child's checkpoints must stay out of.
    writable: Vec<PathBuf>,
    /// Why shell commands have no sandbox, when that is not just that the system has none: said
    /// when a child is asked to approve one, as it is for its parent.
    unsandboxed: Option<String>,
    /// Where each child's tool output goes: a directory of its own under this one.
    output_root: PathBuf,
    /// Stops a child's wait for a model's server.
    cancel: CancellationToken,
    /// The user confirmed `full-access` for the program-run children of this run
    /// (`ask --yes-full-access`): they are not asked again.
    full_access: std::sync::atomic::AtomicBool,
    /// Nobody can be asked a question (`harness ask`): a model that needs an answer first and
    /// has no fallback cannot be used.
    headless: std::sync::atomic::AtomicBool,
}

impl CliFactory {
    #[allow(clippy::too_many_arguments, reason = "what a session knows, passed on")]
    pub fn new(
        setup: Arc<Setup>,
        resolver: Arc<CliResolver>,
        provider: Arc<dyn Provider>,
        template: AgentConfig,
        edit_format: EditFormat,
        sandboxes: Sandboxes,
        parent_local: bool,
        writable: Vec<PathBuf>,
        unsandboxed: Option<String>,
        output_root: PathBuf,
        cancel: CancellationToken,
    ) -> Arc<CliFactory> {
        let started_on = CurrentModel {
            provider,
            id: template.model_id.clone(),
            name: template.model_name.clone(),
            context_window: template.context_window,
            request: template.request.clone(),
            text_tool_calls: template.text_tool_calls,
            local: parent_local,
            tools: harness_tools::builtin_for(edit_format),
            system_prompt: template.system_prompt.clone(),
            edit_section: template.edit_section.clone(),
        };
        Arc::new(CliFactory {
            setup,
            resolver,
            template,
            current: ParentModel::default(),
            started_on,
            sandboxes,
            writable,
            unsandboxed,
            output_root,
            cancel,
            full_access: std::sync::atomic::AtomicBool::new(false),
            headless: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Children that run in Claude Code or Codex in `full-access` mode are not asked to confirm
    /// it: the user already did (a headless run started with `--yes-full-access`).
    pub fn confirm_full_access(&self) {
        self.full_access
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// The run has no one to ask (`harness ask`).
    pub fn set_headless(&self) {
        self.headless
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Where the parent agent publishes the model it is on, for it to be given
    /// ([`Agent::with_model_cell`]).
    pub fn parent_model(&self) -> ParentModel {
        self.current.clone()
    }

    /// The model the parent is on now.
    fn parent_now(&self) -> CurrentModel {
        self.current
            .get()
            .unwrap_or_else(|| self.started_on.clone())
    }
}

/// The system prompt of a child: its parent's `prompt`, with the edit section of the parent's
/// model (`parent_section`) swapped for that of the child's own (`own_section`) when they differ
/// and the first is in the prompt, then the child's `instructions` after a gap.
fn child_prompt(
    prompt: &str,
    parent_section: Option<&str>,
    own_section: Option<&str>,
    instructions: Option<&str>,
) -> String {
    let mut prompt = match (parent_section, own_section) {
        (Some(old), Some(new)) if old != new => prompt.replacen(old, new, 1),
        _ => prompt.to_string(),
    };
    if let Some(instructions) = instructions.map(str::trim).filter(|i| !i.is_empty()) {
        prompt.push_str("\n\n");
        prompt.push_str(instructions);
    }
    prompt
}

/// Whether `workspace` is the session's own (however it is spelled).
fn workspace_is_parents(workspace: &std::path::Path, parent: &std::path::Path) -> bool {
    match (workspace.canonicalize(), parent.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => workspace == parent,
    }
}

/// The model a spec runs on when it names one other than its parent's.
fn own_model<'a>(spec: &'a AgentSpec, parent: &CurrentModel) -> Option<&'a str> {
    spec.model.as_deref().filter(|id| *id != parent.id)
}

#[async_trait]
impl AgentFactory for CliFactory {
    fn endpoint(&self, spec: &AgentSpec) -> Endpoint {
        let parent = self.parent_now();
        let (id, local) = match own_model(spec, &parent) {
            Some(id) => (id, self.resolver.runs_locally(id).unwrap_or(parent.local)),
            None => (parent.id.as_str(), parent.local),
        };
        // Claude Code and Codex are programs of their own, each an endpoint with its own limit;
        // they are never "local": their requests go to the program, not a server of the user's.
        let delegated = harness_core::agents::is_delegated_model(id);
        Endpoint {
            key: id.split('/').next().unwrap_or(id).to_string(),
            local: local && !delegated,
            delegated,
        }
    }

    async fn usable(&self, model: &str) -> Result<(), String> {
        if !harness_core::agents::is_delegated_model(model) {
            return Ok(());
        }
        // The program is there and in its tested range; then the account (a Claude whose opt-in
        // was declined and has no key says so). Not yet answered is usable: it asks at its start.
        crate::delegate::ready(&self.setup, model).await?;
        let resolved = self.resolver.resolve(model, self.cancel.clone()).await?;
        // Where nobody can answer the Claude subscription question and there is no key, the
        // child could only fail at its start: the call is refused instead.
        if self.headless.load(std::sync::atomic::Ordering::SeqCst)
            && let Some(why) = resolved
                .provider
                .delegated()
                .and_then(|program| program.unusable_unasked())
        {
            return Err(why);
        }
        Ok(())
    }

    async fn build(&self, child: ChildSetup) -> Result<Agent, String> {
        let ChildSetup {
            tag,
            spec,
            policy,
            mode,
            approver,
            meter,
            session,
            env,
        } = child;
        // A child that names no model is built from the one its parent is on now (after `/model`,
        // a role's model, a fallback), the provider, window, request settings and side included.
        let parent = self.parent_now();
        let mut config = self.template.clone();
        config.output_dir = self.output_root.join(tag.id.as_str());
        config.model_id = parent.id.clone();
        config.model_name = parent.name.clone();
        config.context_window = parent.context_window;
        config.request = parent.request.clone();
        config.text_tool_calls = parent.text_tool_calls;
        config.edit_section = parent.edit_section.clone();
        config.system_prompt = parent.system_prompt.clone();
        if let Some(prompt) = &spec.system_prompt {
            config.system_prompt = prompt.clone();
        }
        if let Some(steps) = spec.max_steps {
            config.max_steps = steps;
        }
        // A model that runs in another program has no system prompt of harness's: what the
        // definition says goes to it in front of the task.
        config.brief = spec.instructions.clone();
        let (provider, mut tools) = match own_model(&spec, &parent) {
            Some(id) => {
                let model = self.resolver.resolve(id, self.cancel.clone()).await?;
                config.model_id = model.id.clone();
                config.model_name = model.name.clone();
                if let Some(window) = model.context_window {
                    config.context_window = window;
                }
                if let Some(request) = model.request.clone() {
                    config.request = request;
                }
                config.text_tool_calls = model.text_tool_calls;
                // Its own edit format: the prompt describes the tool it has.
                config.system_prompt = child_prompt(
                    &config.system_prompt,
                    parent.edit_section.as_deref(),
                    model.edit_section.as_deref(),
                    None,
                );
                config.edit_section = model.edit_section.clone().or(parent.edit_section.clone());
                let tools = model.tools.clone().unwrap_or_else(|| parent.tools.clone());
                (model.provider.clone(), tools)
            }
            None => (parent.provider.clone(), parent.tools.clone()),
        };
        config.system_prompt = child_prompt(
            &config.system_prompt,
            None,
            None,
            spec.instructions.as_deref(),
        );
        // Whatever the parent's model offers, a child cannot start subagents: nesting is one
        // level deep.
        tools = tools.without(&[harness_core::agents::SPAWN_TOOL.to_string()]);
        if let Some(names) = &spec.tools {
            let allowed: Vec<Arc<dyn Tool>> = tools
                .specs()
                .into_iter()
                .filter(|tool| names.contains(&tool.name))
                .filter_map(|tool| tools.get(&tool.name))
                .collect();
            tools = ToolRegistry::new(allowed);
        }
        let workspace = spec.workspace.resolve(&self.setup.workspace);
        let sandbox = self.sandboxes.for_mode(mode);
        harness_core::permission::PermissionPolicy::set_sandbox_available(
            &*policy,
            sandbox.is_some(),
        );
        if sandbox.is_none()
            && let Some(why) = &self.unsandboxed
        {
            harness_core::permission::PermissionPolicy::set_unsandboxed_reason(&*policy, why);
        }
        let in_worktree = !workspace_is_parents(&workspace, &self.setup.workspace);
        // A worktree is a workspace the sandbox has not seen: on Linux its guard reads what it
        // needs from it before any command can have changed it.
        if in_worktree && let Some(sandbox) = &sandbox {
            crate::start::start_sandbox_session(&workspace, sandbox.clone()).await;
        }
        let ctx = ToolContext::new(&workspace)
            .with_sandbox(sandbox, mode.fs_access())
            .with_env(env);
        // What it changes is told by comparing the workspace with a snapshot of its own, so what
        // a shell command edited counts, and the gates see it.
        let checkpoints = crate::sessions::child_checkpoints(
            &self.setup,
            session.id(),
            &self.writable,
            &workspace,
        );
        let mut agent = Agent::new(provider, tools, policy, approver, config, ctx)
            .with_redactor(self.setup.redactor.clone())
            .with_session(session)
            // Both sandboxes, as the parent has them: what its parent's mode does to the child
            // while it runs also changes the sandbox its next command gets.
            .with_sandboxes(self.sandboxes.clone())
            .with_checkpoints(checkpoints)
            .with_gates(self.setup.config.gates.clone())
            .with_roles(self.setup.config.roles.clone())
            .with_resolver(self.resolver.clone());
        if let Some(meter) = meter {
            agent = agent.with_meter(meter);
        }
        if self.full_access.load(std::sync::atomic::Ordering::SeqCst) {
            agent.confirm_full_access();
        }
        Ok(agent)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use harness_core::{
        agent::{AgentConfig, Sandboxes},
        agents::{AgentFactory, AgentSpec, AgentStatus, Limits, ParentContext, Supervisor},
        edit_format::EditFormat,
        engine::{EngineConfig, PermissionEngine, RuleSet},
        permission::Mode,
        testing::{MockProvider, Script},
    };
    use tokio_util::sync::CancellationToken;

    use super::CliFactory;
    use crate::host::tests::setup_with;

    const LOCAL: &str =
        "[providers.mine]\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1:9/v1\"\n";

    struct Fixture {
        _dir: tempfile::TempDir,
        factory: Arc<CliFactory>,
        provider: Arc<MockProvider>,
        workspace: std::path::PathBuf,
        data: std::path::PathBuf,
    }

    fn fixture(config: &str, script: Vec<Script>) -> Fixture {
        fixture_with(config, |_| script, Sandboxes::default())
    }

    /// A fixture whose script is made once the workspace's path is known.
    fn fixture_with(
        config: &str,
        script: impl FnOnce(&std::path::Path) -> Vec<Script>,
        sandboxes: Sandboxes,
    ) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let (home, workspace) = (dir.path().join("home"), dir.path().join("work"));
        std::fs::create_dir_all(home.join("config")).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(home.join("config/config.toml"), config).unwrap();
        let workspace = workspace.canonicalize().unwrap();
        let setup = setup_with(&home, &workspace, &[("OPENAI_API_KEY", "sk-test")]);
        let provider = MockProvider::new(script(&workspace));
        let template = AgentConfig::new(
            "openai/gpt-5",
            "gpt-5",
            "the parent's prompt",
            dir.path().join("out"),
        );
        let factory = CliFactory::new(
            setup.clone(),
            crate::routes::CliResolver::new(setup.clone()),
            provider.clone(),
            template,
            EditFormat::StrReplace,
            sandboxes,
            false,
            Vec::new(),
            Some("the sandbox is disabled by HARNESS_SANDBOX=none".into()),
            dir.path().join("out"),
            CancellationToken::new(),
        );
        Fixture {
            data: setup.paths.data_dir.clone(),
            _dir: dir,
            factory,
            provider,
            workspace,
        }
    }

    fn parent(
        f: &Fixture,
        mode: Mode,
    ) -> (
        ParentContext,
        tokio::sync::mpsc::UnboundedReceiver<harness_core::agents::AgentEnvelope>,
        Arc<dyn harness_core::agent::Approver>,
    ) {
        let config = EngineConfig {
            mode,
            workspace: f.workspace.clone(),
            read_dirs: vec![],
            rules: RuleSet::default(),
            sandbox_available: false,
            writes_need_approval: false,
        };
        let approver: Arc<dyn harness_core::agent::Approver> =
            Arc::new(harness_core::agent::NonInteractive);
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (
            ParentContext {
                session: "parent-session".into(),
                cancel: CancellationToken::new(),
                policy: Arc::new(PermissionEngine::new(config.clone())),
                engine: config,
                approver: approver.clone(),
                meter: None,
                sessions_dir: Some(f.workspace.join("sessions")),
                workspace: f.workspace.clone(),
            },
            rx,
            approver,
        )
    }

    #[test]
    fn a_childs_endpoint_is_its_models_provider_and_side() {
        let f = fixture(LOCAL, vec![]);
        let endpoint = |model: Option<&str>| {
            let mut spec = AgentSpec::new("x", "y");
            spec.model = model.map(String::from);
            f.factory.endpoint(&spec)
        };
        let parents = endpoint(None);
        assert_eq!((parents.key.as_str(), parents.local), ("openai", false));
        let local = endpoint(Some("mine/x"));
        assert_eq!((local.key.as_str(), local.local), ("mine", true));
        assert!(endpoint(Some("ollama/llama3")).local);
        assert!(!parents.delegated && !local.delegated);
    }

    /// A program that says its version and does nothing else.
    fn fake_program(dir: &std::path::Path, name: &str, version: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\necho '{version}'\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn fixture_with_vars(
        config: &str,
        vars: &[(&str, &str)],
    ) -> (tempfile::TempDir, Arc<CliFactory>) {
        let dir = tempfile::tempdir().unwrap();
        let (home, workspace) = (dir.path().join("home"), dir.path().join("work"));
        std::fs::create_dir_all(home.join("config")).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(home.join("config/config.toml"), config).unwrap();
        let workspace = workspace.canonicalize().unwrap();
        let setup = setup_with(&home, &workspace, vars);
        let factory = CliFactory::new(
            setup.clone(),
            crate::routes::CliResolver::new(setup.clone()),
            MockProvider::new(vec![]),
            AgentConfig::new("openai/gpt-5", "gpt-5", "p", dir.path().join("out")),
            EditFormat::StrReplace,
            Sandboxes::default(),
            false,
            Vec::new(),
            None,
            dir.path().join("out"),
            CancellationToken::new(),
        );
        (dir, factory)
    }

    #[tokio::test]
    async fn a_program_that_is_missing_or_untested_cannot_be_used_and_the_reason_is_given() {
        let tools = tempfile::tempdir().unwrap();
        let missing = tools.path().join("nope");
        let (_d, f) = fixture_with_vars("", &[("HARNESS_CODEX_BIN", missing.to_str().unwrap())]);
        let why = f.usable("codex/gpt-5").await.unwrap_err();
        assert!(why.contains("codex"), "{why}");
        let old = fake_program(tools.path(), "codex", "codex-cli 0.1.0");
        let (_d, f) = fixture_with_vars("", &[("HARNESS_CODEX_BIN", old.to_str().unwrap())]);
        let why = f.usable("codex/gpt-5").await.unwrap_err();
        assert!(why.contains("tested range"), "{why}");
        // A model of harness's own is always fine here.
        assert!(f.usable("openai/gpt-5").await.is_ok());
    }

    #[tokio::test]
    async fn claude_is_usable_with_the_opt_in_or_a_key_or_while_undecided_and_not_when_refused() {
        let tools = tempfile::tempdir().unwrap();
        let claude = fake_program(tools.path(), "claude", "2.1.312 (Claude Code)");
        let bin = ("HARNESS_CLAUDE_BIN", claude.to_str().unwrap());
        let (_d, f) = fixture_with_vars("[delegate.claude]\nuse_subscription = true\n", &[bin]);
        assert!(f.usable("claude-code/sonnet").await.is_ok());
        // Not answered yet: the child asks at its start.
        let (_d, f) = fixture_with_vars("", &[bin]);
        assert!(f.usable("claude-code/sonnet").await.is_ok());
        // Declined, and no key: it says how to get one.
        let (_d, f) = fixture_with_vars("[delegate.claude]\nuse_subscription = false\n", &[bin]);
        let why = f.usable("claude-code/sonnet").await.unwrap_err();
        assert!(why.contains("harness auth add anthropic"), "{why}");
        let (_d, f) = fixture_with_vars(
            "[delegate.claude]\nuse_subscription = false\n",
            &[bin, ("ANTHROPIC_API_KEY", "sk-ant-api03-test")],
        );
        assert!(f.usable("claude-code/sonnet").await.is_ok());
    }

    #[tokio::test]
    async fn headless_with_the_claude_question_unanswered_and_no_key_is_not_usable() {
        let tools = tempfile::tempdir().unwrap();
        let claude = fake_program(tools.path(), "claude", "2.1.312 (Claude Code)");
        let bin = ("HARNESS_CLAUDE_BIN", claude.to_str().unwrap());
        // Nobody can answer the question, and there is no key to fall back on: the whole call
        // is refused before anything starts.
        let (_d, f) = fixture_with_vars("", &[bin]);
        f.set_headless();
        let why = f.usable("claude-code/sonnet").await.unwrap_err();
        assert!(why.contains("harness auth add anthropic"), "{why}");
        // With a key, with the opt-in, or in the terminal (where it can be asked) it is fine,
        // and Codex needs no answer.
        let (_d, f) = fixture_with_vars("", &[bin, ("ANTHROPIC_API_KEY", "sk-ant-api03-test")]);
        f.set_headless();
        assert!(f.usable("claude-code/sonnet").await.is_ok());
        let (_d, f) = fixture_with_vars("[delegate.claude]\nuse_subscription = true\n", &[bin]);
        f.set_headless();
        assert!(f.usable("claude-code/sonnet").await.is_ok());
        let (_d, f) = fixture_with_vars("", &[bin]);
        assert!(f.usable("claude-code/sonnet").await.is_ok());
    }

    #[test]
    fn a_child_on_claude_code_or_codex_is_an_endpoint_of_its_own_program_with_its_own_class() {
        let f = fixture(LOCAL, vec![]);
        let endpoint = |model: &str| {
            let mut spec = AgentSpec::new("x", "y");
            spec.model = Some(model.into());
            f.factory.endpoint(&spec)
        };
        let claude = endpoint("claude-code/sonnet");
        assert_eq!(
            (claude.key.as_str(), claude.local, claude.delegated),
            ("claude-code", false, true)
        );
        // Two models of one program share its limit; the other program has its own.
        assert_eq!(endpoint("claude-code/opus").key, claude.key);
        let codex = endpoint("codex/gpt-5");
        assert_eq!(
            (codex.key.as_str(), codex.local, codex.delegated),
            ("codex", false, true)
        );
    }

    #[tokio::test]
    async fn a_child_runs_through_the_cli_factory_with_the_parents_model_and_its_own_prompt() {
        let f = fixture("", vec![Script::text("found it")]);
        let (ctx, _rx, _a) = parent(&f, Mode::ReadOnly);
        let (tx, mut events) = tokio::sync::mpsc::unbounded_channel();
        let sup = Supervisor::new(Limits::default(), f.factory.clone(), ctx, tx);
        let mut spec = AgentSpec::new("explore", "where is main?");
        spec.system_prompt = Some("you only look".into());
        spec.tools = Some(vec!["read".into()]);
        spec.mode = Some(Mode::Auto);
        let report = sup.spawn(spec).wait().await;
        assert_eq!(report.status, AgentStatus::Done, "{report:?}");
        assert_eq!(report.summary, "found it");
        let request = &f.provider.requests()[0];
        assert_eq!(request.system, "you only look");
        assert_eq!(request.model, "gpt-5");
        // Only the tool it was given.
        let names: Vec<&str> = request.tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["read"]);
        // Its transcript is a session of its own beside its parent's.
        let sessions = f.workspace.join("sessions");
        assert_eq!(
            harness_core::session::children_of(&sessions, "parent-session").len(),
            1
        );
        assert!(events.try_recv().is_ok());
        let _ = &f.data;
    }

    #[tokio::test]
    async fn a_child_naming_no_model_is_built_from_the_one_its_parent_switched_to() {
        use harness_core::{
            agent::{Agent, SessionModel},
            message::RequestOptions,
            tool::{ToolContext, ToolRegistry},
        };
        let f = fixture("", vec![Script::text("from the old model")]);
        let switched = MockProvider::new(vec![Script::text("from the new model")]);
        // The parent publishes what it is on; `/model` changes it.
        let mut parent_agent = Agent::new(
            f.provider.clone(),
            ToolRegistry::new(Vec::new()),
            Arc::new(PermissionEngine::new(EngineConfig {
                mode: Mode::Auto,
                workspace: f.workspace.clone(),
                read_dirs: vec![],
                rules: RuleSet::default(),
                sandbox_available: false,
                writes_need_approval: false,
            })),
            Arc::new(harness_core::agent::NonInteractive),
            AgentConfig::new("openai/gpt-5", "gpt-5", "p", f.workspace.join("out")),
            ToolContext::new(&f.workspace),
        )
        .with_model_cell(f.factory.parent_model());
        let spec = || AgentSpec::new("x", "y");
        let endpoint = f.factory.endpoint(&spec());
        assert_eq!((endpoint.key.as_str(), endpoint.local), ("openai", false));
        parent_agent.switch_model(SessionModel {
            provider: switched.clone(),
            id: "mine/big".into(),
            name: "big".into(),
            context_window: 123_000,
            request: RequestOptions {
                local: true,
                max_output_tokens: Some(999),
                ..RequestOptions::default()
            },
            text_tool_calls: false,
            tools: None,
            edit_section: None,
        });
        // Its limits are those of the side and server it is on now.
        let endpoint = f.factory.endpoint(&spec());
        assert_eq!((endpoint.key.as_str(), endpoint.local), ("mine", true));
        let (ctx, _rx, _a) = parent(&f, Mode::Auto);
        let (tx, _events) = tokio::sync::mpsc::unbounded_channel();
        let sup = Supervisor::new(Limits::default(), f.factory.clone(), ctx, tx);
        let report = sup.spawn(spec()).wait().await;
        assert_eq!(report.summary, "from the new model", "{report:?}");
        assert!(f.provider.requests().is_empty());
        let request = &switched.requests()[0];
        assert_eq!(request.model, "big");
        assert_eq!(request.options.max_output_tokens, Some(999));
        assert!(request.options.local);
    }

    #[tokio::test]
    async fn a_childs_approval_for_an_unsandboxed_command_says_why_there_is_no_sandbox() {
        use harness_core::agent::{ApprovalDecision, ApprovalRequest, Approver};
        struct Capture(std::sync::Mutex<Vec<String>>);
        #[async_trait::async_trait]
        impl Approver for Capture {
            async fn decide(&self, request: &ApprovalRequest) -> ApprovalDecision {
                self.0.lock().unwrap().push(request.reason.clone());
                ApprovalDecision::Deny { feedback: None }
            }
        }
        let f = fixture(
            "",
            vec![
                Script::tool_call("c1", "bash", serde_json::json!({"command": "ls"})),
                Script::text("could not"),
            ],
        );
        let (mut ctx, _rx, _a) = parent(&f, Mode::Auto);
        let capture = Arc::new(Capture(Default::default()));
        ctx.approver = capture.clone();
        let (tx, _events) = tokio::sync::mpsc::unbounded_channel();
        let sup = Supervisor::new(Limits::default(), f.factory.clone(), ctx, tx);
        sup.spawn(AgentSpec::new("lister", "list")).wait().await;
        let reasons = capture.0.lock().unwrap();
        assert!(
            reasons[0].contains("no sandbox: the sandbox is disabled by HARNESS_SANDBOX=none"),
            "{reasons:?}"
        );
    }

    #[tokio::test]
    async fn a_file_a_childs_shell_command_wrote_is_in_its_report() {
        let f = fixture(
            "",
            vec![
                Script::tool_call(
                    "c1",
                    "bash",
                    serde_json::json!({"command": "echo hi > made.txt"}),
                ),
                Script::text("wrote it"),
            ],
        );
        let (mut ctx, _rx, _a) = parent(&f, Mode::FullAccess);
        ctx.sessions_dir = Some(f.data.join("sessions"));
        let (tx, _events) = tokio::sync::mpsc::unbounded_channel();
        let sup = Supervisor::new(Limits::default(), f.factory.clone(), ctx, tx);
        let report = sup.spawn(AgentSpec::new("writer", "write it")).wait().await;
        assert_eq!(report.status, AgentStatus::Done, "{report:?}");
        assert_eq!(report.files_changed, vec!["made.txt"], "{report:?}");
    }

    #[test]
    fn a_childs_prompt_follows_its_models_edit_format_and_ends_with_its_instructions() {
        let old = "Edit with `edit`.\n";
        let new = "Edit with `apply_patch`.\n";
        let prompt = format!("You are an agent.\n{old}Project notes.");
        // Another model's format replaces the section, where it was.
        assert_eq!(
            super::child_prompt(&prompt, Some(old), Some(new), None),
            format!("You are an agent.\n{new}Project notes.")
        );
        // The same format, no section known, or one that is not in the prompt: unchanged.
        assert_eq!(
            super::child_prompt(&prompt, Some(old), Some(old), None),
            prompt
        );
        assert_eq!(super::child_prompt(&prompt, None, Some(new), None), prompt);
        assert_eq!(
            super::child_prompt(&prompt, Some("gone"), Some(new), None),
            prompt
        );
        // Its instructions come after, with a gap.
        assert_eq!(
            super::child_prompt("base", None, None, Some("be brief")),
            "base\n\nbe brief"
        );
        assert_eq!(super::child_prompt("base", None, None, Some("  ")), "base");
    }

    #[tokio::test]
    async fn a_childs_instructions_come_after_its_parents_prompt_and_replace_nothing() {
        let f = fixture("", vec![Script::text("ok")]);
        let (ctx, _rx, _a) = parent(&f, Mode::ReadOnly);
        let (tx, _events) = tokio::sync::mpsc::unbounded_channel();
        let sup = Supervisor::new(Limits::default(), f.factory.clone(), ctx, tx);
        let mut spec = AgentSpec::new("explore", "where is main?");
        spec.instructions = Some("You only look.".into());
        sup.spawn(spec).wait().await;
        assert_eq!(
            f.provider.requests()[0].system,
            "the parent's prompt\n\nYou only look."
        );
    }

    #[tokio::test]
    async fn a_child_is_never_offered_the_tool_that_starts_subagents() {
        use harness_core::{
            agent::{Agent, SessionModel},
            agents::SPAWN_TOOL,
            message::{RequestOptions, ToolSpec},
            permission::Action,
            tool::{Tool, ToolContext, ToolOutput, ToolRegistry},
        };
        struct Named(&'static str);
        #[async_trait::async_trait]
        impl Tool for Named {
            fn spec(&self) -> ToolSpec {
                ToolSpec {
                    name: self.0.into(),
                    description: String::new(),
                    parameters: serde_json::json!({"type": "object"}),
                }
            }
            fn action(&self, _: &serde_json::Value, ctx: &ToolContext) -> Action {
                Action::Read(ctx.workspace.clone())
            }
            async fn run(&self, _: serde_json::Value, _: &ToolContext) -> ToolOutput {
                ToolOutput::ok("")
            }
        }
        let f = fixture("", vec![Script::text("ok")]);
        // Whatever the parent's model offers, the child is not given the spawn tool.
        let mut parent_agent = Agent::new(
            f.provider.clone(),
            ToolRegistry::new(Vec::new()),
            Arc::new(PermissionEngine::new(EngineConfig {
                mode: Mode::Auto,
                workspace: f.workspace.clone(),
                read_dirs: vec![],
                rules: RuleSet::default(),
                sandbox_available: false,
                writes_need_approval: false,
            })),
            Arc::new(harness_core::agent::NonInteractive),
            AgentConfig::new("openai/gpt-5", "gpt-5", "p", f.workspace.join("out")),
            ToolContext::new(&f.workspace),
        )
        .with_model_cell(f.factory.parent_model());
        parent_agent.switch_model(SessionModel {
            provider: f.provider.clone(),
            id: "openai/gpt-5".into(),
            name: "gpt-5".into(),
            context_window: 100_000,
            request: RequestOptions::default(),
            text_tool_calls: false,
            tools: Some(ToolRegistry::new(vec![
                Arc::new(Named("read")),
                Arc::new(Named(SPAWN_TOOL)),
            ])),
            edit_section: None,
        });
        let (ctx, _rx, _a) = parent(&f, Mode::Auto);
        let (tx, _events) = tokio::sync::mpsc::unbounded_channel();
        let sup = Supervisor::new(Limits::default(), f.factory.clone(), ctx, tx);
        sup.spawn(AgentSpec::new("x", "y")).wait().await;
        let names: Vec<String> = f.provider.requests()[0]
            .tools
            .iter()
            .map(|t| t.name.clone())
            .collect();
        assert_eq!(names, ["read"]);
    }

    #[tokio::test]
    async fn a_model_that_cannot_be_used_fails_the_child_and_says_why() {
        let f = fixture("", vec![]);
        let (ctx, _rx, _a) = parent(&f, Mode::Auto);
        let (tx, _events) = tokio::sync::mpsc::unbounded_channel();
        let sup = Supervisor::new(Limits::default(), f.factory.clone(), ctx, tx);
        let mut spec = AgentSpec::new("x", "y");
        spec.model = Some("nowhere/x".into());
        let report = sup.spawn(spec).wait().await;
        assert_eq!(report.status, AgentStatus::Failed);
        assert!(report.error.unwrap().contains("nowhere"));
    }

    // ---- worktrees --------------------------------------------------------------------------

    use harness_core::worktree::{
        Worktrees,
        testing::{git, init_repo},
    };

    /// A supervisor over a fixture whose workspace is a git repository.
    fn isolating(f: &Fixture, mode: Mode) -> (Arc<Supervisor>, Arc<Worktrees>) {
        init_repo(&f.workspace);
        let (mut ctx, _rx, _a) = parent(f, mode);
        ctx.sessions_dir = Some(f.data.join("sessions"));
        let (tx, _events) = tokio::sync::mpsc::unbounded_channel();
        let sup = Supervisor::new(Limits::default(), f.factory.clone(), ctx, tx);
        let wts = Arc::new(Worktrees::new(&f.workspace, f.data.join("worktrees/p")).unwrap());
        sup.set_worktrees(wts.clone());
        (sup, wts)
    }

    fn isolated(label: &str) -> AgentSpec {
        let mut spec = AgentSpec::new(label, "do it");
        spec.isolation = true;
        spec
    }

    #[tokio::test]
    async fn a_child_that_needs_a_worktree_hears_why_there_is_none() {
        let f = fixture("", vec![Script::text("done")]);
        let (mut ctx, _rx, _a) = parent(&f, Mode::FullAccess);
        ctx.sessions_dir = Some(f.data.join("sessions"));
        let (tx, _events) = tokio::sync::mpsc::unbounded_channel();
        let sup = Supervisor::new(Limits::default(), f.factory.clone(), ctx, tx);
        sup.set_worktrees_unavailable("git is too old for worktrees (2.30.2)".into());
        assert!(sup.take_notices()[0].contains("2.30.2"));
        let report = sup.spawn(isolated("writer")).wait().await;
        assert_eq!(report.status, AgentStatus::Failed, "{report:?}");
        assert!(report.error.unwrap().contains("2.30.2"));
    }

    #[tokio::test]
    async fn an_isolated_child_works_in_its_worktree_and_its_commands_know_who_it_is() {
        let f = fixture(
            "",
            vec![
                Script::tool_call(
                    "c1",
                    "bash",
                    serde_json::json!({"command": "echo \"$HARNESS_AGENT_ID/$HARNESS_PORT_OFFSET\" > ids.txt"}),
                ),
                Script::tool_call(
                    "c2",
                    "write",
                    serde_json::json!({"path": "src/new.rs", "content": "pub fn two() {}\n"}),
                ),
                Script::text("done"),
            ],
        );
        let (sup, wts) = isolating(&f, Mode::FullAccess);
        let report = sup.spawn(isolated("writer")).wait().await;
        assert_eq!(report.status, AgentStatus::Done, "{report:?}");
        // Its edits are in the worktree, its checkpoint saw them (the shell's too), and the
        // main checkout is as it was.
        assert!(
            report.files_changed.contains(&"ids.txt".to_string()),
            "{report:?}"
        );
        assert!(
            report.files_changed.contains(&"src/new.rs".to_string()),
            "{report:?}"
        );
        assert!(!f.workspace.join("ids.txt").exists());
        assert!(!f.workspace.join("src/new.rs").exists());
        let outcome = report.worktree.expect("committed");
        assert_eq!(
            git(
                &f.workspace,
                &["show", &format!("{}:ids.txt", outcome.branch)]
            ),
            "a1/100\n"
        );
        assert!(outcome.files.contains(&"src/new.rs".to_string()));
        assert_eq!(wts.records().len(), 1);
    }

    #[tokio::test]
    async fn an_isolated_child_cannot_write_outside_its_worktree_or_into_git() {
        let f = fixture_with(
            "",
            |main| {
                vec![
                    // The main checkout, by its absolute path.
                    Script::tool_call(
                        "c1",
                        "write",
                        serde_json::json!({"path": main.join("outside.txt"), "content": "escaped\n"}),
                    ),
                    // Git's own files, where the worktree's `.git` is (a second child).
                    Script::tool_call(
                        "c2",
                        "write",
                        serde_json::json!({"path": ".git", "content": "gitdir: /elsewhere\n"}),
                    ),
                ]
            },
            Sandboxes::default(),
        );
        let (sup, wts) = isolating(&f, Mode::Auto);
        let first = sup.spawn(isolated("writer")).wait().await;
        // Nobody can answer the approval it needs, so it fails there.
        assert_eq!(first.status, AgentStatus::Failed, "{first:?}");
        assert!(
            first
                .error
                .as_deref()
                .unwrap()
                .contains("outside the workspace"),
            "{first:?}"
        );
        assert!(
            !f.workspace.join("outside.txt").exists(),
            "wrote into the main checkout"
        );
        let second = sup.spawn(isolated("writer")).wait().await;
        assert_eq!(second.status, AgentStatus::Failed, "{second:?}");
        assert!(
            second.error.as_deref().unwrap().contains(".git"),
            "{second:?}"
        );
        assert_eq!(git(&f.workspace, &["status", "--porcelain"]).trim(), "");
        // They changed nothing, so nothing was kept, and git finds the repository sound.
        assert!(first.worktree.is_none() && second.worktree.is_none());
        assert!(wts.records().is_empty());
        assert_eq!(git(&f.workspace, &["worktree", "list"]).lines().count(), 1);
        git(&f.workspace, &["fsck", "--no-progress"]);
    }

    #[derive(Debug, Default)]
    struct Recording(std::sync::Mutex<Vec<std::path::PathBuf>>);

    impl harness_core::tool::CommandSandbox for Recording {
        fn name(&self) -> &'static str {
            "recording"
        }
        fn command(
            &self,
            _access: harness_core::permission::FsAccess,
            _workspace: &std::path::Path,
            program: &str,
            args: &[&str],
        ) -> std::io::Result<tokio::process::Command> {
            let mut command = tokio::process::Command::new(program);
            command.args(args).process_group(0);
            Ok(command)
        }
        fn is_denial(&self, _code: Option<i32>, _output: &str) -> bool {
            false
        }
        fn start_session(&self, workspace: &std::path::Path) {
            self.0.lock().unwrap().push(workspace.to_path_buf());
        }
    }

    #[tokio::test]
    async fn the_sandbox_is_primed_for_each_worktree_a_child_works_in() {
        let recording = Arc::new(Recording::default());
        let sandboxes = Sandboxes {
            read_only: None,
            workspace_write: Some(recording.clone()),
        };
        let f = fixture_with(
            "",
            |_| {
                vec![
                    Script::tool_call(
                        "c1",
                        "write",
                        serde_json::json!({"path": "a.txt", "content": "x\n"}),
                    ),
                    Script::text("ok"),
                ]
            },
            sandboxes,
        );
        let (sup, wts) = isolating(&f, Mode::Auto);
        let report = sup.spawn(isolated("writer")).wait().await;
        assert_eq!(report.status, AgentStatus::Done, "{report:?}");
        let primed = recording.0.lock().unwrap().clone();
        assert_eq!(primed.len(), 1, "{primed:?}");
        let record = wts.records().remove(0);
        assert_eq!(
            primed[0].canonicalize().unwrap(),
            record.path.canonicalize().unwrap()
        );
    }
}
