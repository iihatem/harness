//! Build hands the plan to the `build` role's model: the whole conversation when it fits that
//! model's window, the plan alone when it does not, announced and recorded; `[roles.handoff]
//! mode` forces either.

mod common;

use std::sync::Arc;

use common::{roles::*, *};
use harness_core::{
    agent::{Agent, NonInteractive},
    event::{AgentEvent, TurnEndReason},
    message::Message,
    permission::Mode,
    role::{Handoff, HandoffKind, HandoffMode, Role, RoleConfig, SwitchReason},
    session::{EntryKind, Session},
    testing::{MockProvider, Script},
    turn::{InputPart, TurnInput},
};
use tokio_util::sync::CancellationToken;

const PLAN: &str = "1. Read src/login.rs\n2. Add a limiter\n3. Test it";

fn build_input() -> TurnInput {
    build_input_saying(&format!("Implement this plan:\n{PLAN}"))
}

/// A Build turn whose message says `text`.
fn build_input_saying(text: &str) -> TurnInput {
    TurnInput {
        parts: vec![InputPart::Text(text.into())],
        display: Some("Build the plan".into()),
        plan: Some(PLAN.into()),
        role: Some(Role::Build),
        ..TurnInput::default()
    }
}

async fn build(agent: &mut Agent) -> (TurnEndReason, Vec<AgentEvent>) {
    build_with(agent, build_input()).await
}

async fn build_with(agent: &mut Agent, input: TurnInput) -> (TurnEndReason, Vec<AgentEvent>) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let reason = agent.run_turn(input, &tx, CancellationToken::new()).await;
    drop(tx);
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }
    (reason, events)
}

struct Setup {
    agent: Agent,
    main: Arc<MockProvider>,
    planner: Arc<MockProvider>,
    builder: Arc<MockProvider>,
}

/// An agent in plan mode on `main`, with `roles`, a planner model with a 1,000,000-token window,
/// and a builder (`build/small`) with `build_window` tokens. The planning turn has run, on a
/// question of `question_chars` characters.
async fn after_planning(
    roles: RoleConfig,
    build_window: u64,
    question_chars: usize,
) -> (Setup, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let main = MockProvider::new(vec![Script::text("main again")]);
    let planner = MockProvider::new(vec![Script::text(PLAN)]);
    let builder = MockProvider::new(vec![Script::text("built it")]);
    let mut agent = with_models(
        agent(
            main.clone(),
            Mode::Plan,
            Arc::new(NonInteractive),
            dir.path(),
        ),
        vec![
            model("plan/big", &planner, 1_000_000),
            model("build/small", &builder, build_window),
        ],
    )
    .with_roles(roles)
    .with_session(Session::create(&dir.path().join("sessions"), dir.path()));
    agent.config_mut().context_window = 1_000_000;
    let (reason, _) = run(&mut agent, &"q".repeat(question_chars)).await;
    assert_eq!(reason, TurnEndReason::Completed);
    agent.set_mode(Mode::Auto);
    (
        Setup {
            agent,
            main,
            planner,
            builder,
        },
        dir,
    )
}

fn roles(build: Option<&str>, handoff: Option<HandoffMode>) -> RoleConfig {
    RoleConfig {
        plan: Some("plan/big".into()),
        build: build.map(String::from),
        handoff,
        ..RoleConfig::default()
    }
}

/// The hand-off recorded on the Build message of the session.
fn recorded(agent: &Agent) -> Option<Handoff> {
    agent
        .session()
        .branch()
        .into_iter()
        .find_map(|e| match &e.kind {
            EntryKind::Message {
                message: Message::User { .. },
                plan: Some(_),
                attribution,
                ..
            } => attribution.and_then(|a| a.handoff),
            _ => None,
        })
}

// Spec "Full history": the build turn runs on the build model with the history and the plan.
#[tokio::test]
async fn a_history_that_fits_goes_to_the_build_model_whole() {
    let (mut s, _dir) = after_planning(roles(Some("build/small"), None), 262_144, 120_000).await;
    let (reason, events) = build(&mut s.agent).await;
    assert_eq!(reason, TurnEndReason::Completed);
    let request = s.builder.requests().pop().unwrap();
    assert_eq!(request.model, "small");
    let users = user_texts(&request.messages);
    assert_eq!(users.len(), 2, "the question and the build message");
    assert!(users[1].contains(PLAN));
    assert_eq!(request.messages.len(), 3, "question, plan, build message");
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::HandoffReduced { .. }))
    );
    assert_eq!(
        switches(&events),
        [(
            "plan/big".into(),
            "build/small".into(),
            Role::Build,
            SwitchReason::User
        )]
    );
    let handoff = recorded(&s.agent).unwrap();
    assert_eq!(
        (handoff.kind, handoff.forced),
        (HandoffKind::History, false)
    );
    // Attributed to the build role.
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::AssistantMessage { role: Role::Build, model, .. } if model == "build/small"
    )));
}

// Spec "History does not fit": 60,000 tokens against an effective window of 32,768.
#[tokio::test]
async fn a_history_that_does_not_fit_is_reduced_to_the_plan() {
    let (mut s, _dir) = after_planning(roles(Some("build/small"), None), 32_768, 240_000).await;
    let (reason, events) = build(&mut s.agent).await;
    assert_eq!(reason, TurnEndReason::Completed);
    let request = s.builder.requests().pop().unwrap();
    assert_eq!(request.system, "system prompt", "the system prompt stays");
    assert_eq!(request.messages.len(), 1, "{:?}", request.messages);
    assert!(user_texts(&request.messages)[0].contains(PLAN));
    let notice = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::HandoffReduced {
                to,
                history_tokens,
                window,
                forced,
            } => Some((to.clone(), *history_tokens, *window, *forced)),
            _ => None,
        })
        .expect("a HandoffReduced event");
    assert_eq!(notice.0, "build/small");
    assert!((60_000..61_000).contains(&notice.1), "{notice:?}");
    assert_eq!((notice.2, notice.3), (32_768, false));
    // Recorded in the session.
    let handoff = recorded(&s.agent).unwrap();
    assert_eq!(handoff.kind, HandoffKind::PlanOnly);
    assert_eq!(handoff.window, 32_768);
    // The conversation is whole again afterwards: the next turn, on main, has all of it.
    assert_eq!(s.agent.history().len(), 5, "{:?}", s.agent.history());
    let (_, _) = run(&mut s.agent, "and now?").await;
    let messages = s.main.requests().pop().unwrap().messages;
    assert!(user_texts(&messages)[0].starts_with("qqq"));
    assert!(messages.len() >= 5);
}

// Spec "Forced plan only": the history would fit, and the build turn gets the plan alone.
#[tokio::test]
async fn plan_only_can_be_forced() {
    let (mut s, _dir) = after_planning(
        roles(Some("build/small"), Some(HandoffMode::PlanOnly)),
        262_144,
        400,
    )
    .await;
    let (_, events) = build(&mut s.agent).await;
    assert_eq!(s.builder.requests()[0].messages.len(), 1);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::HandoffReduced { forced: true, .. }))
    );
    let handoff = recorded(&s.agent).unwrap();
    assert_eq!(
        (handoff.kind, handoff.forced),
        (HandoffKind::PlanOnly, true)
    );
}

#[tokio::test]
async fn history_can_be_forced_and_is_then_not_reduced_whatever_its_size() {
    let (mut s, _dir) = after_planning(
        roles(Some("build/small"), Some(HandoffMode::History)),
        262_144,
        400,
    )
    .await;
    let (_, events) = build(&mut s.agent).await;
    assert_eq!(s.builder.requests()[0].messages.len(), 3);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::HandoffReduced { .. }))
    );
    let handoff = recorded(&s.agent).unwrap();
    assert_eq!((handoff.kind, handoff.forced), (HandoffKind::History, true));
}

// The fit is measured against the compaction threshold of the build model's window: a history
// the model would compact at once is not handed over.
#[tokio::test]
async fn the_history_fits_up_to_the_compaction_threshold_of_the_window() {
    // 100,000 tokens of window, 80% threshold: about 70,000 tokens fit, 85,000 do not.
    let (mut fits, _a) = after_planning(roles(Some("build/small"), None), 100_000, 280_000).await;
    let (_, events) = build(&mut fits.agent).await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::HandoffReduced { .. }))
    );
    let (mut over, _b) = after_planning(roles(Some("build/small"), None), 100_000, 340_000).await;
    let (_, events) = build(&mut over.agent).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::HandoffReduced { .. }))
    );
}

// Spec "Same model": nothing is announced and the turn goes on as before; a forced mode does not
// make a hand-off where there is none.
#[tokio::test]
async fn the_same_model_has_no_handoff() {
    let dir = tempfile::tempdir().unwrap();
    let main = MockProvider::new(vec![Script::text(PLAN), Script::text("built it")]);
    let mut agent = agent(
        main.clone(),
        Mode::Plan,
        Arc::new(NonInteractive),
        dir.path(),
    )
    .with_roles(RoleConfig {
        handoff: Some(HandoffMode::PlanOnly),
        ..RoleConfig::default()
    })
    .with_session(Session::create(&dir.path().join("sessions"), dir.path()));
    run(&mut agent, "make a plan").await;
    agent.set_mode(Mode::Auto);
    let (_, events) = build(&mut agent).await;
    assert!(switches(&events).is_empty(), "{events:?}");
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::HandoffReduced { .. }))
    );
    let sent = main.requests().pop().unwrap();
    assert!(sent.messages.len() >= 3, "the whole conversation");
    let handoff = recorded(&agent).unwrap();
    assert_eq!(handoff.kind, HandoffKind::SameModel);
    // The turn is the build role's all the same.
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::AssistantMessage {
            role: Role::Build,
            ..
        }
    )));
}

// A plan role is set and the build role is not: the build turn runs on main, a hand-off from the
// model that planned.
#[tokio::test]
async fn an_unset_build_role_is_main_and_that_can_be_a_handoff() {
    let (mut s, _dir) = after_planning(roles(None, None), 262_144, 400).await;
    let (_, events) = build(&mut s.agent).await;
    assert_eq!(
        switches(&events),
        [(
            "plan/big".into(),
            "mock/m1".into(),
            Role::Build,
            SwitchReason::User
        )]
    );
    // It ran on main, which has 1,000,000 tokens here.
    assert_eq!(s.main.requests().len(), 1);
    assert!(s.builder.requests().is_empty());
    assert_eq!(recorded(&s.agent).unwrap().kind, HandoffKind::History);
    let _ = s.planner;
}

// What is dropped for the build turn comes back in the session as it was.
#[tokio::test]
async fn the_session_keeps_everything_a_reduced_handoff_left_out() {
    let (mut s, _dir) = after_planning(roles(Some("build/small"), None), 32_768, 240_000).await;
    build(&mut s.agent).await;
    let path = s.agent.session().path().unwrap().to_path_buf();
    drop(s);
    let (session, _) = Session::open(&path).unwrap();
    let users = user_texts(
        &session
            .messages()
            .into_iter()
            .map(|(_, m)| m)
            .collect::<Vec<_>>(),
    );
    // The question, the mode note, and the Build message.
    assert_eq!(users.len(), 3);
    assert!(users[0].starts_with("qqq") && users[2].contains(PLAN));
}

// An approved plan that was not edited is built with "Implement the plan above.", which points at
// the planning turn's reply: a build turn that goes without the conversation is sent the plan in
// that message, and the session keeps the message as it was.
#[tokio::test]
async fn a_message_that_points_at_the_plan_carries_it_when_the_plan_goes_alone() {
    let (mut s, _dir) = after_planning(roles(Some("build/small"), None), 32_768, 240_000).await;
    build_with(
        &mut s.agent,
        build_input_saying("Implement the plan above."),
    )
    .await;
    let sent = s.builder.requests().pop().unwrap();
    assert_eq!(sent.messages.len(), 1);
    let text = &user_texts(&sent.messages)[0];
    assert!(text.contains(PLAN) && text.contains("Implement"), "{text}");
    // Afterwards the history holds the message as it was typed.
    let users = user_texts(s.agent.history());
    assert_eq!(users.last().unwrap(), "Implement the plan above.");
}
