//! 3.7: the outcome records of a fallback turn, an escalated turn and a reduced hand-off, from a
//! real agent writing through the usage meter.

use std::{collections::HashMap, future::Future, pin::Pin, sync::Arc};

use harness_core::{
    agent::{Agent, AgentConfig, NonInteractive, SessionModel},
    engine::{EngineConfig, PermissionEngine, RuleSet},
    message::RequestOptions,
    permission::Mode,
    provider::ProviderError,
    retry::RetryPolicy,
    role::{ModelResolver, RoleConfig, SwitchReason},
    session::Session,
    testing::{MockProvider, Script},
    tool::{ToolContext, ToolRegistry},
    turn::{InputPart, TurnInput, TurnModel},
};
use harness_usage::{meter::UsageMeter, outcomes::OutcomeLog, paths::Dirs};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

type Ready = Pin<Box<dyn Future<Output = Result<TurnModel, String>> + Send>>;

struct Models {
    models: HashMap<String, TurnModel>,
    chain: Vec<String>,
}

impl ModelResolver for Models {
    fn resolve(&self, id: &str, _cancel: CancellationToken) -> Ready {
        let found = self
            .models
            .get(id)
            .cloned()
            .ok_or_else(|| format!("no {id}"));
        Box::pin(std::future::ready(found))
    }

    fn chain(&self, _model_id: &str) -> Vec<String> {
        self.chain.clone()
    }
}

fn model(id: &str, provider: &Arc<MockProvider>, window: u64) -> TurnModel {
    TurnModel {
        provider: provider.clone(),
        id: id.into(),
        name: id.rsplit('/').next().unwrap().into(),
        local: false,
        tools: None,
        edit_section: None,
        context_window: Some(window),
        request: None,
        text_tool_calls: false,
    }
}

struct Fixture {
    agent: Agent,
    data: tempfile::TempDir,
    _dir: tempfile::TempDir,
}

fn fixture(
    primary: &Arc<MockProvider>,
    models: Vec<TurnModel>,
    chain: &[&str],
    roles: RoleConfig,
    mode: Mode,
) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let policy = Arc::new(PermissionEngine::new(EngineConfig {
        mode,
        workspace: dir.path().to_path_buf(),
        read_dirs: vec![],
        rules: RuleSet::default(),
        sandbox_available: true,
        writes_need_approval: false,
    }));
    let mut config = AgentConfig::new("mock/main", "main", "system", dir.path().join("out"));
    config.context_window = 1_000_000;
    config.retry = RetryPolicy {
        max_attempts: 5,
        base_delay: std::time::Duration::from_millis(1),
        max_delay: std::time::Duration::from_millis(2),
    };
    let agent = Agent::new(
        primary.clone(),
        ToolRegistry::new(vec![]),
        policy,
        Arc::new(NonInteractive),
        config,
        ToolContext::new(dir.path()),
    )
    .with_session(Session::create(&dir.path().join("sessions"), dir.path()))
    .with_roles(roles)
    .with_resolver(Arc::new(Models {
        models: models.into_iter().map(|m| (m.id.clone(), m)).collect(),
        chain: chain.iter().map(|c| c.to_string()).collect(),
    }))
    .with_meter(Arc::new(UsageMeter::open(data.path(), dir.path())));
    Fixture {
        agent,
        data,
        _dir: dir,
    }
}

async fn run(agent: &mut Agent, input: impl Into<TurnInput>) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    agent.run_turn(input, &tx, CancellationToken::new()).await;
    drop(tx);
    while rx.recv().await.is_some() {}
}

fn outcomes(data: &std::path::Path) -> Vec<Value> {
    OutcomeLog::new(&Dirs::under(data).outcomes)
        .files()
        .iter()
        .flat_map(|f| {
            std::fs::read_to_string(f)
                .unwrap()
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect::<Vec<Value>>()
        })
        .collect()
}

fn big(provider: &Arc<MockProvider>) -> SessionModel {
    SessionModel {
        provider: provider.clone(),
        id: "big/model".into(),
        name: "model".into(),
        context_window: 1_000_000,
        request: RequestOptions::default(),
        text_tool_calls: false,
        tools: None,
        edit_section: None,
    }
}

// Spec "Selected by a fallback": its record has `selected_by` `fallback` and the model that
// finished the turn.
#[tokio::test]
async fn a_fallback_turn_is_recorded_as_one_on_the_model_that_finished_it() {
    let quota = || {
        Script::error(ProviderError::Http {
            status: 429,
            body: r#"{"error":{"type":"usage_limit_reached"}}"#.into(),
            retry_after: None,
        })
    };
    let primary = MockProvider::new(vec![quota(), Script::text("main again")]);
    let fallback = MockProvider::new(vec![Script::text("from the fallback")]);
    let mut f = fixture(
        &primary,
        vec![model("fb/x", &fallback, 1_000_000)],
        &["fb/x"],
        RoleConfig::default(),
        Mode::Auto,
    );
    run(&mut f.agent, "go").await;
    run(&mut f.agent, "and again").await;
    let records = outcomes(f.data.path());
    assert_eq!(records.len(), 2);
    assert_eq!(records[0]["selected_by"], "fallback");
    assert_eq!(records[0]["model"], "fb/x");
    assert_eq!(records[0]["role"], "main");
    assert_eq!(records[0]["finish_reason"], "completed");
    // The next turn is back on the configured model.
    assert_eq!(records[1]["selected_by"], "config");
    assert_eq!(records[1]["model"], "mock/main");
}

#[tokio::test]
async fn an_escalated_turn_is_recorded_as_one_and_a_user_switch_as_the_users() {
    let primary = MockProvider::new(vec![]);
    let escalated = MockProvider::new(vec![Script::text("one")]);
    let chosen = MockProvider::new(vec![Script::text("two")]);
    let mut f = fixture(&primary, vec![], &[], RoleConfig::default(), Mode::Auto);
    f.agent
        .switch_model_as(big(&escalated), SwitchReason::Escalation);
    run(&mut f.agent, "go").await;
    f.agent.switch_model(big(&chosen));
    run(&mut f.agent, "again").await;
    let records = outcomes(f.data.path());
    assert_eq!(records[0]["selected_by"], "escalation");
    assert_eq!(records[0]["model"], "big/model");
    assert_eq!(records[1]["selected_by"], "user");
}

// Spec "A completed turn" (no hand-off) and tasks.md 3.7: a reduced hand-off is recorded on the
// build turn, and only there.
#[tokio::test]
async fn a_reduced_handoff_is_recorded_on_the_build_turn_only() {
    let primary = MockProvider::new(vec![]);
    let planner = MockProvider::new(vec![Script::text("1. do it")]);
    let builder = MockProvider::new(vec![Script::text("built")]);
    let roles = RoleConfig {
        plan: Some("plan/big".into()),
        build: Some("build/small".into()),
        ..RoleConfig::default()
    };
    let mut f = fixture(
        &primary,
        vec![
            model("plan/big", &planner, 1_000_000),
            model("build/small", &builder, 3_000),
        ],
        &[],
        roles,
        Mode::Plan,
    );
    run(&mut f.agent, "q".repeat(12_000)).await;
    f.agent.set_mode(Mode::Auto);
    let build = TurnInput {
        parts: vec![InputPart::Text("Implement the plan above.".into())],
        plan: Some("1. do it".into()),
        role: Some(harness_core::role::Role::Build),
        ..TurnInput::default()
    };
    run(&mut f.agent, build).await;
    let records = outcomes(f.data.path());
    assert_eq!(records.len(), 2);
    assert_eq!(records[0]["role"], "plan");
    assert!(records[0]["handoff"].is_null());
    assert_eq!(records[1]["role"], "build");
    assert_eq!(records[1]["model"], "build/small");
    assert_eq!(
        records[1]["handoff"],
        serde_json::json!({"kind": "plan_only", "forced": false, "result": "none"})
    );
    // Counts, kinds and ids only.
    let text =
        std::fs::read_to_string(&OutcomeLog::new(&Dirs::under(f.data.path()).outcomes).files()[0])
            .unwrap();
    assert!(!text.contains("do it") && !text.contains("qqq"), "{text}");
}
