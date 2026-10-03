//! Edges of roles and the hand-off that a person at the keyboard meets: Esc while a model is made
//! ready, a stopped or failed Build turn that went with the plan alone, and a compaction inside
//! one.

mod common;

use std::{sync::Arc, time::Duration};

use common::{roles::*, *};
use futures::future::BoxFuture;
use harness_core::{
    agent::{Agent, NonInteractive},
    compaction::SUMMARY_PREFIX,
    event::{AgentEvent, TurnEndReason},
    message::Message,
    permission::Mode,
    provider::{ProviderError, ProviderEvent},
    role::{ModelResolver, Role, RoleConfig},
    session::Session,
    testing::{MockProvider, Script},
    turn::{InputPart, TurnInput, TurnModel},
};
use serde_json::json;
use tokio_util::sync::CancellationToken;

const PLAN: &str = "1. Read src/login.rs\n2. Add a limiter";

/// A model that is never ready until the wait is stopped, as a local server loading its model.
struct Loading;

impl ModelResolver for Loading {
    fn chain(&self, _model_id: &str) -> Vec<String> {
        vec!["chain/slow".to_string()]
    }

    fn resolve(
        &self,
        _id: &str,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<TurnModel, String>> {
        Box::pin(async move {
            cancel.cancelled().await;
            Err("stopped".to_string())
        })
    }
}

// Esc while the plan role's model is being made ready: the turn is interrupted, nothing is sent,
// and no error is shown.
#[tokio::test]
async fn esc_while_a_role_model_is_made_ready_interrupts_the_turn_quietly() {
    let dir = tempfile::tempdir().unwrap();
    let main = MockProvider::new(vec![Script::text("never")]);
    let mut agent = agent(
        main.clone(),
        Mode::Plan,
        Arc::new(NonInteractive),
        dir.path(),
    )
    .with_roles(RoleConfig {
        plan: Some("plan/slow".into()),
        ..RoleConfig::default()
    })
    .with_resolver(Arc::new(Loading));
    let cancel = CancellationToken::new();
    let stop = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        stop.cancel();
    });
    let (reason, events) = run_with(&mut agent, "plan it", cancel).await;
    assert_eq!(reason, TurnEndReason::Interrupted);
    assert!(main.requests().is_empty());
    assert!(
        !events.iter().any(|e| matches!(e, AgentEvent::Error { .. })),
        "{events:?}"
    );
    // The session accepts the next turn.
    assert!(agent.history().is_empty());
}

/// An agent that planned on `plan/big` and is about to build on `build/small` (3,000 tokens of
/// window) with a conversation too big for it, so the plan goes alone.
async fn reduced(
    builder_script: Vec<Script>,
    main_script: Vec<Script>,
) -> (
    Agent,
    Arc<MockProvider>,
    Arc<MockProvider>,
    tempfile::TempDir,
) {
    let dir = tempfile::tempdir().unwrap();
    let main = MockProvider::new(main_script);
    let planner = MockProvider::new(vec![Script::text(PLAN)]);
    let builder = MockProvider::new(builder_script);
    let mut agent = with_models(
        agent(
            main.clone(),
            Mode::Plan,
            Arc::new(NonInteractive),
            dir.path(),
        ),
        vec![
            model("plan/big", &planner, 1_000_000),
            model("build/small", &builder, 3_000),
        ],
    )
    .with_roles(RoleConfig {
        plan: Some("plan/big".into()),
        build: Some("build/small".into()),
        ..RoleConfig::default()
    })
    .with_session(Session::create(&dir.path().join("sessions"), dir.path()));
    agent.config_mut().context_window = 1_000_000;
    run(&mut agent, &"q".repeat(12_000)).await;
    agent.set_mode(Mode::Auto);
    (agent, main, builder, dir)
}

fn build_input() -> TurnInput {
    TurnInput {
        parts: vec![InputPart::Text("Implement the plan above.".into())],
        plan: Some(PLAN.into()),
        role: Some(Role::Build),
        ..TurnInput::default()
    }
}

async fn build_with(
    agent: &mut Agent,
    cancel: CancellationToken,
) -> (TurnEndReason, Vec<AgentEvent>) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let reason = agent.run_turn(build_input(), &tx, cancel).await;
    drop(tx);
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }
    (reason, events)
}

fn users(agent: &Agent) -> Vec<String> {
    user_texts(agent.history())
}

// The conversation set aside for a build turn that goes with the plan alone comes back when the
// turn is stopped...
#[tokio::test]
async fn a_stopped_reduced_build_turn_gives_the_conversation_back() {
    let (mut agent, _main, _builder, _dir) = reduced(
        vec![Script::Hang(vec![ProviderEvent::TextDelta(
            "working".into(),
        )])],
        vec![],
    )
    .await;
    let cancel = CancellationToken::new();
    let stop = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        stop.cancel();
    });
    let (reason, events) = build_with(&mut agent, cancel).await;
    assert_eq!(reason, TurnEndReason::Interrupted);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::HandoffReduced { .. }))
    );
    let users = users(&agent);
    assert!(users[0].starts_with("qqq"), "the question is back");
    assert_eq!(users.last().unwrap(), "Implement the plan above.");
}

// ... and when it fails.
#[tokio::test]
async fn a_failed_reduced_build_turn_gives_the_conversation_back() {
    let (mut agent, _main, _builder, _dir) = reduced(
        vec![Script::error(ProviderError::Http {
            status: 401,
            body: "bad key".into(),
            retry_after: None,
        })],
        vec![Script::text("main answers")],
    )
    .await;
    let (reason, _) = build_with(&mut agent, CancellationToken::new()).await;
    assert_eq!(reason, TurnEndReason::Error);
    assert!(users(&agent)[0].starts_with("qqq"));
    // The next turn, on main, has the whole conversation.
    let (reason, _) = run(&mut agent, "what now?").await;
    assert_eq!(reason, TurnEndReason::Completed);
}

// A compaction in the middle of a build turn that went with the plan alone rebuilds the history
// from the session, and the turn and the session carry on.
#[tokio::test]
async fn a_compaction_inside_a_reduced_build_turn_leaves_a_consistent_session() {
    let (mut agent, main, builder, _dir) = reduced(
        vec![
            // A step whose call and result fill the build model's window.
            Script::tool_call("c1", "echo", json!({"text": "x".repeat(12_000)})),
            Script::text("built"),
        ],
        // The summary is main's to write (no `background` role), then an ordinary answer.
        vec![Script::text("the build so far"), Script::text("main again")],
    )
    .await;
    let (reason, events) = build_with(&mut agent, CancellationToken::new()).await;
    assert_eq!(reason, TurnEndReason::Completed, "{events:?}");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Compacted { .. })),
        "{events:?}"
    );
    assert_eq!(builder.requests().len(), 2);
    // The history is what the session says: a summary first.
    assert!(matches!(
        &agent.history()[0],
        Message::User { content } if content.starts_with(SUMMARY_PREFIX)
    ));
    // The session reads back, and the next turn works on it.
    let path = agent.session().path().unwrap().to_path_buf();
    let (reply, _) = run(&mut agent, "and next?").await;
    assert_eq!(reply, TurnEndReason::Completed);
    assert_eq!(main.requests().len(), 2);
    drop(agent);
    let (reopened, warnings) = Session::open(&path).unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    assert!(!reopened.messages().is_empty());
}

// A compaction inside a plan-only build turn summarizes the conversation that was set aside too:
// the summary replaces everything before the kept part, so one that never saw it would lose the
// planning for the next turns. The history after the turn is what it would have been with no
// hand-off: a summary of the conversation and the build turn so far, then the rest of the turn.
#[tokio::test]
async fn a_compaction_inside_a_reduced_build_turn_keeps_the_set_aside_conversation() {
    let (mut agent, main, builder, _dir) = reduced(
        vec![
            Script::tool_call("c1", "echo", json!({"text": "x".repeat(12_000)})),
            Script::text("built"),
        ],
        vec![
            Script::text("planned and built so far"),
            Script::text("main again"),
        ],
    )
    .await;
    let (reason, events) = build_with(&mut agent, CancellationToken::new()).await;
    assert_eq!(reason, TurnEndReason::Completed, "{events:?}");
    assert_eq!(builder.requests().len(), 2);
    // The summary request saw the planning conversation and the plan, not only the build steps.
    let summary_request = main.requests().remove(0);
    let Message::User { content } = &summary_request.messages[0] else {
        panic!("{summary_request:?}");
    };
    assert!(content.contains("qqq"), "the question: {content:.300}");
    assert!(content.contains("Read src/login.rs"), "the plan");
    // The Build message as the session has it, not the plan the build model was sent in its place.
    assert!(content.contains("Implement the plan above."), "{content}");
    // The history is the summary, then what the compaction kept of the turn.
    let history = agent.history();
    assert!(matches!(
        &history[0],
        Message::User { content } if content.starts_with(SUMMARY_PREFIX)
    ));
    assert!(
        !users(&agent).iter().any(|u| u.starts_with("qqq")),
        "the set-aside question is in the summary, not kept whole beside it"
    );
    // The next turn on main is sent that history.
    let (reply, _) = run(&mut agent, "and next?").await;
    assert_eq!(reply, TurnEndReason::Completed);
    let next = main.requests().pop().unwrap();
    assert!(matches!(
        &next.messages[0],
        Message::User { content } if content.starts_with(SUMMARY_PREFIX)
    ));
}

// Esc while the background model is made ready for a compaction is a stop, not a failure to
// compact: no warning.
#[tokio::test]
async fn esc_while_the_background_model_is_made_ready_gives_no_warning() {
    let dir = tempfile::tempdir().unwrap();
    let main = MockProvider::new(vec![Script::text("noted"), Script::text("never")]);
    let mut agent = agent(
        main.clone(),
        Mode::Auto,
        Arc::new(NonInteractive),
        dir.path(),
    )
    .with_roles(RoleConfig {
        background: Some("bg/slow".into()),
        ..RoleConfig::default()
    })
    .with_resolver(Arc::new(Loading));
    run(&mut agent, &"x".repeat(8_000)).await;
    agent.config_mut().context_window = 2_500;
    let cancel = CancellationToken::new();
    let stop = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        stop.cancel();
    });
    let (reason, events) = run_with(&mut agent, "short question", cancel).await;
    assert_eq!(reason, TurnEndReason::Interrupted, "{events:?}");
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::Warning { .. } | AgentEvent::Error { .. })),
        "{events:?}"
    );
}

// Esc while a fallback candidate is made ready ends the turn as a stop, with no list of skipped
// candidates.
#[tokio::test]
async fn esc_while_a_fallback_model_is_made_ready_gives_no_warning() {
    let dir = tempfile::tempdir().unwrap();
    let main = MockProvider::new(vec![Script::error(ProviderError::Http {
        status: 429,
        body: r#"{"error":{"type":"usage_limit_reached"}}"#.into(),
        retry_after: None,
    })]);
    let mut agent = agent(
        main.clone(),
        Mode::Auto,
        Arc::new(NonInteractive),
        dir.path(),
    )
    .with_resolver(Arc::new(Loading));
    let cancel = CancellationToken::new();
    let stop = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        stop.cancel();
    });
    let (reason, events) = run_with(&mut agent, "go", cancel).await;
    assert_eq!(reason, TurnEndReason::Interrupted, "{events:?}");
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::Warning { .. } | AgentEvent::Error { .. })),
        "{events:?}"
    );
}
