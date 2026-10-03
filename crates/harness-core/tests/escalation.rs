//! Escalation is suggested, never automatic: three invalid tool calls, three identical failing
//! results, or the same gate failing twice in a turn, once each, when `escalation.to` is set.

mod common;

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use common::{
    gates::{ScriptedBash, Setup, failing, passing},
    roles::*,
    *,
};
use harness_core::{
    agent::{Agent, AgentConfig, NonInteractive, SessionModel},
    engine::{EngineConfig, PermissionEngine, RuleSet},
    event::{AgentEvent, EscalationTrigger, TurnEndReason},
    gate::Gates,
    message::{RequestOptions, ToolSpec},
    permission::{Action, Mode},
    role::{Role, SwitchReason},
    testing::{MockProvider, Script},
    tool::{Tool, ToolContext, ToolOutput, ToolRegistry},
};
use serde_json::{Value, json};

/// Fails with the same text every time.
struct Same;

#[async_trait]
impl Tool for Same {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "same".into(),
            description: "always fails the same way".into(),
            parameters: json!({"type": "object"}),
        }
    }
    fn action(&self, _args: &Value, ctx: &ToolContext) -> Action {
        Action::Read(ctx.workspace.clone())
    }
    async fn run(&self, _args: Value, _ctx: &ToolContext) -> ToolOutput {
        ToolOutput::error("tool exploded\nwith a second line")
    }
}

/// Fails differently each time.
struct Varying(AtomicUsize);

#[async_trait]
impl Tool for Varying {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "varying".into(),
            description: "fails with a new message".into(),
            parameters: json!({"type": "object"}),
        }
    }
    fn action(&self, _args: &Value, ctx: &ToolContext) -> Action {
        Action::Read(ctx.workspace.clone())
    }
    async fn run(&self, _args: Value, _ctx: &ToolContext) -> ToolOutput {
        let n = self.0.fetch_add(1, Ordering::SeqCst);
        ToolOutput::error(format!("failure number {n}"))
    }
}

fn agent_with_tools(provider: Arc<MockProvider>, to: Option<&str>) -> Agent {
    let dir = tempfile::tempdir().unwrap().keep();
    let policy = Arc::new(PermissionEngine::new(EngineConfig {
        mode: Mode::Auto,
        workspace: dir.clone(),
        read_dirs: vec![],
        rules: RuleSet::default(),
        sandbox_available: true,
        writes_need_approval: false,
    }));
    Agent::new(
        provider,
        ToolRegistry::new(vec![
            Arc::new(Same) as Arc<dyn Tool>,
            Arc::new(Varying(AtomicUsize::new(0))),
        ]),
        policy,
        Arc::new(NonInteractive),
        AgentConfig::new("mock/m1", "m1", "system prompt", dir.join("out")),
        ToolContext::new(&dir),
    )
    .with_escalation(to.map(String::from))
}

fn suggestions(events: &[AgentEvent]) -> Vec<(EscalationTrigger, u32, Option<String>, String)> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::EscalationSuggested {
                trigger,
                count,
                first_line,
                to,
            } => Some((*trigger, *count, first_line.clone(), to.clone())),
            _ => None,
        })
        .collect()
}

fn calls(name: &str, n: usize) -> Vec<Script> {
    (0..n)
        .map(|i| Script::tool_call(&format!("c{i}"), name, json!({})))
        .collect()
}

// Spec "Three invalid calls": one suggestion, with the count and the first line of the last
// failure, and the model is unchanged.
#[tokio::test]
async fn three_invalid_calls_suggest_once_and_change_nothing() {
    let mut script = calls("nope", 5);
    script.push(Script::text("giving up"));
    let provider = MockProvider::new(script);
    let mut agent = agent_with_tools(provider.clone(), Some("big/model"));
    let (reason, events) = run(&mut agent, "go").await;
    assert_eq!(reason, TurnEndReason::Completed);
    let found = suggestions(&events);
    assert_eq!(found.len(), 1, "once per trigger per turn: {found:?}");
    let (trigger, count, first, to) = &found[0];
    assert_eq!(
        (*trigger, *count, to.as_str()),
        (EscalationTrigger::InvalidToolCalls, 3, "big/model")
    );
    assert!(
        first.as_deref().unwrap().starts_with("unknown tool `nope`"),
        "{first:?}"
    );
    assert!(!first.as_deref().unwrap().contains('\n'));
    // Nothing switched: every request went to the same provider.
    assert!(switches(&events).is_empty());
    assert_eq!(provider.requests().len(), 6);
    // The next turn can suggest again.
    let mut script = calls("nope", 3);
    script.push(Script::text("again"));
    let provider = MockProvider::new(script);
    let mut agent = agent_with_tools(provider, Some("big/model"));
    let (_, first_turn) = run(&mut agent, "one").await;
    assert_eq!(suggestions(&first_turn).len(), 1);
}

// Spec "Not configured".
#[tokio::test]
async fn without_an_escalation_model_nothing_is_suggested() {
    let mut script = calls("nope", 3);
    script.push(Script::text("done"));
    let mut agent = agent_with_tools(MockProvider::new(script), None);
    let (_, events) = run(&mut agent, "go").await;
    assert!(suggestions(&events).is_empty());
}

#[tokio::test]
async fn a_model_that_is_already_the_escalation_model_is_not_told_to_escalate() {
    let mut script = calls("nope", 3);
    script.push(Script::text("done"));
    let mut agent = agent_with_tools(MockProvider::new(script), Some("mock/m1"));
    let (_, events) = run(&mut agent, "go").await;
    assert!(suggestions(&events).is_empty());
}

// D6: three identical failing results, by the tool and the output; invalid calls are their own
// trigger and do not count twice.
#[tokio::test]
async fn three_identical_failures_suggest_but_different_ones_do_not() {
    let mut script = calls("same", 3);
    script.push(Script::text("done"));
    let mut agent = agent_with_tools(MockProvider::new(script), Some("big/model"));
    let (_, events) = run(&mut agent, "go").await;
    let found = suggestions(&events);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].0, EscalationTrigger::IdenticalFailures);
    assert_eq!(found[0].1, 3);
    assert_eq!(found[0].2.as_deref(), Some("tool exploded"));

    let mut script = calls("varying", 4);
    script.push(Script::text("done"));
    let mut agent = agent_with_tools(MockProvider::new(script), Some("big/model"));
    let (_, events) = run(&mut agent, "go").await;
    assert!(suggestions(&events).is_empty(), "{events:?}");

    // Invalid calls are not failing results of a tool.
    let mut script = calls("nope", 3);
    script.push(Script::text("done"));
    let mut agent = agent_with_tools(MockProvider::new(script), Some("big/model"));
    let (_, events) = run(&mut agent, "go").await;
    assert_eq!(
        suggestions(&events).iter().map(|s| s.0).collect::<Vec<_>>(),
        [EscalationTrigger::InvalidToolCalls]
    );
}

// Spec "Repeated gate failure": the same gate fails twice in a turn.
#[tokio::test]
async fn the_same_gate_failing_twice_suggests() {
    let dir = tempfile::tempdir().unwrap();
    let (bash, _) = ScriptedBash::new(vec![(
        "cargo test",
        vec![failing(1, "first\n"), failing(1, "second\n"), passing()],
    )]);
    let provider = MockProvider::new(vec![
        Script::tool_call(
            "e1",
            "edit",
            json!({"path": "src.rs", "content": "fn x() {}\n"}),
        ),
        Script::text("a"),
        Script::text("b"),
        Script::text("c"),
    ]);
    let mut agent = common::gates::agent(
        provider,
        dir.path(),
        bash,
        Setup {
            gates: Gates {
                test: Some("cargo test".into()),
                max_retries: 3,
                ..Gates::default()
            },
            ..Setup::default()
        },
    )
    .with_escalation(Some("big/model".into()));
    let (reason, events) = run(&mut agent, "change it").await;
    assert_eq!(reason, TurnEndReason::Completed);
    let found = suggestions(&events);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!((found[0].0, found[0].1), (EscalationTrigger::GateFailed, 2));
    assert!(
        found[0].2.as_deref().unwrap().contains("second"),
        "{:?}",
        found[0].2
    );
}

// `/escalate` sets `main` for the next turn: the switch is an escalation, and the messages after it
// say so.
#[tokio::test]
async fn an_escalation_switch_is_attributed_as_one() {
    let dir = tempfile::tempdir().unwrap();
    let big = MockProvider::new(vec![Script::text("from the big one")]);
    let mut agent = common::agent(
        MockProvider::new(vec![]),
        Mode::Auto,
        Arc::new(NonInteractive),
        dir.path(),
    );
    let event = agent.switch_model_as(
        SessionModel {
            provider: big,
            id: "big/model".into(),
            name: "model".into(),
            context_window: 100_000,
            request: RequestOptions::default(),
            text_tool_calls: false,
            tools: None,
            edit_section: None,
        },
        SwitchReason::Escalation,
    );
    assert_eq!(
        event,
        AgentEvent::ModelSwitched {
            from: "mock/m1".into(),
            to: "big/model".into(),
            role: Role::Main,
            reason: SwitchReason::Escalation,
            detail: None,
        }
    );
    let (_, events) = run(&mut agent, "now").await;
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::AssistantMessage { model, switch_reason: Some(SwitchReason::Escalation), .. }
            if model == "big/model"
    )));
}
