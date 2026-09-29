//! Plan mode: the note that asks for a plan, and the approved plan saved in the session.

mod common;

use std::sync::Arc;

use common::{Echo, agent, run};
use harness_core::{
    agent::{Agent, AgentConfig, NonInteractive},
    engine::{EngineConfig, PermissionEngine},
    message::Message,
    permission::Mode,
    session::Session,
    testing::{MockProvider, Script},
    tool::{ToolContext, ToolRegistry},
    turn::{InputPart, TurnInput},
};

fn notes(provider: &MockProvider) -> String {
    provider
        .requests()
        .last()
        .unwrap()
        .messages
        .iter()
        .filter_map(|m| match m {
            Message::User { content } => Some(content.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn plan_mode_asks_for_a_step_by_step_plan_in_the_conversation() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![Script::text("1. read\n2. change")]);
    let mut agent = agent(
        provider.clone(),
        Mode::Auto,
        Arc::new(NonInteractive),
        dir.path(),
    );
    agent.set_mode(Mode::Plan);
    run(&mut agent, "add a login rate limiter").await;
    let sent = notes(&provider);
    assert!(
        sent.contains("[harness] The approval mode is now plan"),
        "{sent}"
    );
    assert!(
        sent.contains("end your reply with a step-by-step implementation plan"),
        "{sent}"
    );
    // The system prompt is left alone.
    assert_eq!(provider.requests()[0].system, "system prompt");
    // Read-only mode is not plan mode: no plan is asked for.
    agent.set_mode(Mode::ReadOnly);
    let history = agent.history();
    let Message::User { content } = history.last().unwrap() else {
        panic!("a note");
    };
    assert!(!content.contains("plan"), "{content}");
}

fn saved_agent(provider: Arc<MockProvider>, session: Session, dir: &std::path::Path) -> Agent {
    let policy = Arc::new(PermissionEngine::new(EngineConfig {
        mode: Mode::Auto,
        workspace: dir.to_path_buf(),
        read_dirs: vec![],
        rules: Default::default(),
        sandbox_available: false,
        writes_need_approval: false,
    }));
    Agent::new(
        provider,
        ToolRegistry::new(vec![Arc::new(Echo)]),
        policy,
        Arc::new(NonInteractive),
        AgentConfig::new("mock/m1", "m1", "system prompt", dir.join(".spill")),
        ToolContext::new(dir),
    )
    .with_session(session)
}

#[tokio::test]
async fn the_approved_plan_is_saved_with_the_turn_that_builds_it() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let provider = MockProvider::new(vec![Script::text("Done.")]);
    let mut agent = saved_agent(
        provider.clone(),
        Session::create(&sessions, dir.path()),
        dir.path(),
    );
    assert_eq!(agent.approved_plan(), None);
    let plan = "1. Add a limiter\n2. Test it".to_string();
    let input = TurnInput {
        parts: vec![InputPart::Text("Implement the plan above.".into())],
        display: Some("Build the plan".into()),
        plan: Some(plan.clone()),
        ..TurnInput::default()
    };
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    agent
        .run_turn(input, &tx, tokio_util::sync::CancellationToken::new())
        .await;
    assert_eq!(agent.approved_plan(), Some(plan.clone()));
    let path = agent.session().path().unwrap().to_path_buf();
    drop(agent);
    // It is in the file, and read back on resume.
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        text.contains("\"plan\":\"1. Add a limiter\\n2. Test it\""),
        "{text}"
    );
    let (session, _) = Session::open(&path).unwrap();
    let agent = saved_agent(MockProvider::new(vec![]), session, dir.path());
    assert_eq!(agent.approved_plan(), Some(plan));
    assert_eq!(agent.rewind_points().last().unwrap().text, "Build the plan");
}
