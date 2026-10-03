//! Every assistant message carries its role and, when its model is not the one its role would
//! otherwise use, why; every switch of model is an event; and the session file stays in format 2
//! for the builds that read it.

mod common;

use std::{collections::HashMap, sync::Arc};

use common::*;
use futures::future::BoxFuture;
use harness_core::{
    agent::{Agent, NonInteractive, SessionModel},
    event::AgentEvent,
    message::{Message, RequestOptions},
    permission::Mode,
    role::{ModelResolver, Role, RoleConfig, SwitchReason},
    session::{EntryKind, FORMAT_VERSION, Session},
    testing::{MockProvider, Script},
    turn::{TurnInput, TurnModel},
};
use tokio_util::sync::CancellationToken;

struct Models(HashMap<String, TurnModel>);

impl ModelResolver for Models {
    fn resolve(
        &self,
        id: &str,
        _cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<TurnModel, String>> {
        let found = self.0.get(id).cloned().ok_or_else(|| format!("no {id}"));
        Box::pin(async move { found })
    }
}

fn model(id: &str, provider: &Arc<MockProvider>) -> TurnModel {
    TurnModel {
        provider: provider.clone(),
        id: id.into(),
        name: id.rsplit('/').next().unwrap().into(),
        local: false,
        tools: None,
        edit_section: None,
        context_window: Some(100_000),
        request: None,
        text_tool_calls: false,
    }
}

fn with_models(agent: Agent, models: Vec<TurnModel>) -> Agent {
    agent.with_resolver(Arc::new(Models(
        models.into_iter().map(|m| (m.id.clone(), m)).collect(),
    )))
}

/// The role and reason of each assistant message event, in order.
fn attributed(events: &[AgentEvent]) -> Vec<(String, Role, Option<SwitchReason>)> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::AssistantMessage {
                model,
                role,
                switch_reason,
                ..
            } => Some((model.clone(), *role, *switch_reason)),
            _ => None,
        })
        .collect()
}

fn switches(events: &[AgentEvent]) -> Vec<(String, String, Role, SwitchReason)> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ModelSwitched {
                from,
                to,
                role,
                reason,
                ..
            } => Some((from.clone(), to.clone(), *role, *reason)),
            _ => None,
        })
        .collect()
}

/// The (role, switch reason) of each assistant message in the session's active branch.
fn saved(session: &Session) -> Vec<(Role, Option<SwitchReason>)> {
    session
        .branch()
        .into_iter()
        .filter_map(|entry| match &entry.kind {
            EntryKind::Message {
                message: Message::Assistant { .. },
                attribution,
                ..
            } => {
                let a = attribution.expect("an assistant message is attributed");
                Some((a.role, a.switch_reason))
            }
            _ => None,
        })
        .collect()
}

fn switched(id: &str, provider: &Arc<MockProvider>) -> SessionModel {
    SessionModel {
        provider: provider.clone(),
        id: id.into(),
        name: id.rsplit('/').next().unwrap().into(),
        context_window: 100_000,
        request: RequestOptions::default(),
        text_tool_calls: false,
        tools: None,
        edit_section: None,
    }
}

// Spec "Attribution after a switch" (agent-runtime), and the role on every message.
#[tokio::test]
async fn messages_carry_their_role_and_a_user_switch_marks_the_ones_after_it() {
    let dir = tempfile::tempdir().unwrap();
    let first = MockProvider::new(vec![Script::text("one")]);
    let second = MockProvider::new(vec![Script::text("two")]);
    let session = Session::create(&dir.path().join("sessions"), dir.path());
    let mut agent =
        agent(first, Mode::Auto, Arc::new(NonInteractive), dir.path()).with_session(session);
    let (_, events) = run(&mut agent, "q1").await;
    assert_eq!(
        attributed(&events),
        [("mock/m1".to_string(), Role::Main, None)]
    );
    agent.switch_model(switched("other/big", &second));
    let (_, events) = run(&mut agent, "q2").await;
    assert_eq!(
        attributed(&events),
        [(
            "other/big".to_string(),
            Role::Main,
            Some(SwitchReason::User)
        )]
    );
    // The session holds the same, and earlier messages keep their model.
    assert_eq!(
        saved(agent.session()),
        [(Role::Main, None), (Role::Main, Some(SwitchReason::User))]
    );
    let models: Vec<String> = agent
        .session()
        .messages()
        .into_iter()
        .filter_map(|(_, m)| match m {
            Message::Assistant { model, .. } => Some(model),
            _ => None,
        })
        .collect();
    assert_eq!(models, ["mock/m1", "other/big"]);
}

// Only the assistant's messages are attributed.
#[tokio::test]
async fn user_and_tool_messages_carry_no_attribution() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call("c1", "echo", serde_json::json!({"text": "hi"})),
        Script::text("done"),
    ]);
    let mut agent = agent(provider, Mode::Auto, Arc::new(NonInteractive), dir.path())
        .with_session(Session::create(&dir.path().join("sessions"), dir.path()));
    run(&mut agent, "go").await;
    for entry in agent.session().branch() {
        if let EntryKind::Message {
            message: Message::User { .. } | Message::Tool { .. },
            attribution,
            ..
        } = &entry.kind
        {
            assert!(attribution.is_none(), "{entry:?}");
        }
    }
    assert_eq!(saved(agent.session()).len(), 2);
}

// Spec "Role and reason recorded", and "A switch between roles of different models ... MUST also
// be announced": a plan mode turn on the plan model is announced when it starts, and the way
// back to main when the plan is built.
#[tokio::test]
async fn a_role_assignment_is_announced_once_and_attributed_to_the_role() {
    let dir = tempfile::tempdir().unwrap();
    let main = MockProvider::new(vec![Script::text("built")]);
    let plan = MockProvider::new(vec![Script::text("plan one"), Script::text("plan two")]);
    let roles = RoleConfig {
        plan: Some("plan/big".into()),
        ..RoleConfig::default()
    };
    let session = Session::create(&dir.path().join("sessions"), dir.path());
    let mut agent = with_models(
        agent(main, Mode::Plan, Arc::new(NonInteractive), dir.path()),
        vec![model("plan/big", &plan)],
    )
    .with_roles(roles)
    .with_session(session);
    let (_, events) = run(&mut agent, "plan it").await;
    assert_eq!(
        switches(&events),
        [(
            "mock/m1".to_string(),
            "plan/big".to_string(),
            Role::Plan,
            SwitchReason::User
        )]
    );
    // The announcement comes before the message it is about.
    let at = |f: fn(&AgentEvent) -> bool| events.iter().position(f).unwrap();
    assert!(
        at(|e| matches!(e, AgentEvent::ModelSwitched { .. }))
            < at(|e| matches!(e, AgentEvent::AssistantMessage { .. }))
    );
    // The model is the role's own, so no reason is given.
    assert_eq!(
        attributed(&events),
        [("plan/big".to_string(), Role::Plan, None)]
    );
    // Planning again: no new announcement.
    let (_, events) = run(&mut agent, "plan more").await;
    assert!(switches(&events).is_empty(), "{events:?}");
    // Back to main.
    agent.set_mode(Mode::Auto);
    let (_, events) = run(&mut agent, "build it").await;
    assert_eq!(
        switches(&events),
        [(
            "plan/big".to_string(),
            "mock/m1".to_string(),
            Role::Main,
            SwitchReason::User
        )]
    );
    assert_eq!(
        saved(agent.session()),
        [(Role::Plan, None), (Role::Plan, None), (Role::Main, None)]
    );
}

#[tokio::test]
async fn a_role_set_for_the_session_marks_its_messages_as_the_users() {
    let dir = tempfile::tempdir().unwrap();
    let plan = MockProvider::new(vec![Script::text("a plan")]);
    let mut agent = with_models(
        agent(
            MockProvider::new(vec![]),
            Mode::Plan,
            Arc::new(NonInteractive),
            dir.path(),
        ),
        vec![model("plan/other", &plan)],
    );
    assert!(agent.set_role_model(Role::Plan, "plan/other"));
    assert_eq!(agent.role_model_id(Role::Plan), "plan/other");
    assert_eq!(agent.role_model_id(Role::Build), "mock/m1");
    let (_, events) = run(&mut agent, "plan it").await;
    assert_eq!(
        attributed(&events),
        [(
            "plan/other".to_string(),
            Role::Plan,
            Some(SwitchReason::User)
        )]
    );
}

// A slash command that names a model asks for it, so its messages are marked as the user's.
#[tokio::test]
async fn a_commands_model_marks_its_messages_as_the_users() {
    let dir = tempfile::tempdir().unwrap();
    let command = MockProvider::new(vec![Script::text("from the command")]);
    let mut agent = agent(
        MockProvider::new(vec![]),
        Mode::Auto,
        Arc::new(NonInteractive),
        dir.path(),
    );
    let input = TurnInput {
        model: Some(model("cmd/x", &command)),
        ..TurnInput::from("go")
    };
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    agent.run_turn(input, &tx, CancellationToken::new()).await;
    drop(tx);
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }
    assert_eq!(
        attributed(&events),
        [("cmd/x".to_string(), Role::Main, Some(SwitchReason::User))]
    );
}

/// What a harness that wrote format 2 before roles reads of a session line: its own types, which
/// name no attribution, and ignore what they do not know.
#[allow(
    dead_code,
    reason = "a frozen copy of the old reader's types, read for what it rejects"
)]
mod frozen_reader {
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    pub struct Entry {
        pub id: String,
        pub parent_id: Option<String>,
        #[serde(flatten)]
        pub kind: Kind,
    }

    #[derive(Debug, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum Kind {
        Session {
            version: u32,
        },
        Message {
            message: Message,
            #[serde(default)]
            display: Option<String>,
            #[serde(default)]
            note: bool,
            #[serde(default)]
            plan: Option<String>,
        },
        Checkpoint {
            commit: String,
        },
        Compaction {
            summary: String,
        },
    }

    #[derive(Debug, Deserialize)]
    #[serde(tag = "role", rename_all = "snake_case")]
    pub enum Message {
        User { content: String },
        Assistant { content: String, model: String },
        Tool { call_id: String },
    }
}

// Tasks.md 3.3: sessions remain format 2, and a build that predates roles reads them.
#[tokio::test]
async fn a_session_with_attribution_is_still_format_two_and_reads_as_before() {
    let dir = tempfile::tempdir().unwrap();
    let plan = MockProvider::new(vec![Script::text("a plan")]);
    let roles = RoleConfig {
        plan: Some("plan/big".into()),
        ..RoleConfig::default()
    };
    let session = Session::create(&dir.path().join("sessions"), dir.path());
    let mut agent = with_models(
        agent(
            MockProvider::new(vec![]),
            Mode::Plan,
            Arc::new(NonInteractive),
            dir.path(),
        ),
        vec![model("plan/big", &plan)],
    )
    .with_roles(roles)
    .with_session(session);
    run(&mut agent, "plan it").await;
    let path = agent.session().path().unwrap().to_path_buf();
    drop(agent);
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("\"attribution\""), "{text}");
    let mut parents = std::collections::HashSet::new();
    let mut messages = 0;
    for line in text.lines() {
        let entry: frozen_reader::Entry = serde_json::from_str(line).unwrap_or_else(|e| {
            panic!("a build that predates roles cannot read {line}: {e}");
        });
        // Every entry hangs off one the old reader also read: no entry is one it skips.
        if let Some(parent) = &entry.parent_id {
            assert!(parents.contains(parent), "{line}");
        }
        parents.insert(entry.id.clone());
        match entry.kind {
            frozen_reader::Kind::Session { version } => assert_eq!(version, FORMAT_VERSION),
            frozen_reader::Kind::Message { message, .. } => {
                messages += 1;
                if let frozen_reader::Message::Assistant { model, .. } = message {
                    assert_eq!(model, "plan/big");
                }
            }
            _ => {}
        }
    }
    assert_eq!(FORMAT_VERSION, 2);
    assert_eq!(messages, 2);
    // And this build reads its own attribution back.
    let (reopened, warnings) = Session::open(&path).unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(saved(&reopened), [(Role::Plan, None)]);
}
