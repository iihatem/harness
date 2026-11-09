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
        })
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
        Endpoint {
            key: id.split('/').next().unwrap_or(id).to_string(),
            local,
        }
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
                config.edit_section = model.edit_section.clone();
                let tools = model.tools.clone().unwrap_or_else(|| parent.tools.clone());
                (model.provider.clone(), tools)
            }
            None => (parent.provider.clone(), parent.tools.clone()),
        };
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
        let ctx = ToolContext::new(&workspace).with_sandbox(sandbox, mode.fs_access());
        // What it changes is told by comparing the workspace with a snapshot of its own, so what
        // a shell command edited counts, and the gates see it.
        let checkpoints =
            crate::sessions::child_checkpoints(&self.setup, session.id(), &self.writable);
        let mut agent = Agent::new(provider, tools, policy, approver, config, ctx)
            .with_redactor(self.setup.redactor.clone())
            .with_session(session)
            .with_checkpoints(checkpoints)
            .with_gates(self.setup.config.gates.clone())
            .with_roles(self.setup.config.roles.clone())
            .with_resolver(self.resolver.clone());
        if let Some(meter) = meter {
            agent = agent.with_meter(meter);
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
        let dir = tempfile::tempdir().unwrap();
        let (home, workspace) = (dir.path().join("home"), dir.path().join("work"));
        std::fs::create_dir_all(home.join("config")).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(home.join("config/config.toml"), config).unwrap();
        let workspace = workspace.canonicalize().unwrap();
        let setup = setup_with(&home, &workspace, &[("OPENAI_API_KEY", "sk-test")]);
        let provider = MockProvider::new(script);
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
            Sandboxes::default(),
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
}
