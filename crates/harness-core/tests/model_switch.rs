//! `/model`: the session continues on another model, whose id each of its replies carries, with
//! the window and request options of its own.

mod common;

use std::sync::Arc;

use common::*;
use harness_core::{
    agent::{NonInteractive, SessionModel},
    message::{Message, RequestOptions},
    permission::Mode,
    session::Session,
    testing::{MockProvider, Script},
};

#[tokio::test]
async fn the_conversation_continues_on_the_new_model() {
    let dir = tempfile::tempdir().unwrap();
    let first = MockProvider::new(vec![Script::text("from the first")]);
    let second = MockProvider::new(vec![Script::text("from the second")]);
    let session = Session::create(&dir.path().join("sessions"), dir.path());
    let mut agent = agent(
        first.clone(),
        Mode::Auto,
        Arc::new(NonInteractive),
        dir.path(),
    )
    .with_session(session);
    run(&mut agent, "question one").await;
    agent.switch_model(SessionModel {
        provider: second.clone(),
        id: "other/big".into(),
        name: "big".into(),
        context_window: 200_000,
        request: RequestOptions {
            max_output_tokens: Some(4_096),
            ..RequestOptions::default()
        },
        text_tool_calls: false,
        tools: None,
        edit_section: None,
    });
    assert_eq!(agent.model_id(), "other/big");
    assert_eq!(agent.context_usage().window, 200_000);
    run(&mut agent, "question two").await;
    // The first model was asked once; the second got the whole conversation.
    assert_eq!(first.requests().len(), 1);
    let request = second.requests().pop().unwrap();
    assert_eq!(request.model, "big");
    assert_eq!(request.options.max_output_tokens, Some(4_096));
    assert_eq!(
        request.messages,
        [
            Message::User {
                content: "question one".into()
            },
            Message::Assistant {
                content: "from the first".into(),
                tool_calls: Vec::new(),
                model: "mock/m1".into(),
            },
            Message::User {
                content: "question two".into()
            },
        ]
    );
    // Each reply carries the model that wrote it.
    let models: Vec<String> = agent
        .history()
        .iter()
        .filter_map(|m| match m {
            Message::Assistant { model, .. } => Some(model.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(models, ["mock/m1", "other/big"]);
}

// Review Focus: switching to a model whose window the conversation already overfills, such as
// a local model after a hosted one. The next turn compacts first, with the new model, so the
// request it sends fits the new window.
#[tokio::test]
async fn a_smaller_window_is_compacted_into_before_the_next_request() {
    let dir = tempfile::tempdir().unwrap();
    let long = "tell me about this: ".to_string() + &"lorem ipsum dolor sit amet ".repeat(200);
    let first = MockProvider::new(vec![
        Script::text("a long answer"),
        Script::text("another answer"),
    ]);
    let small = MockProvider::new(vec![
        Script::text("The user asked about lorem ipsum twice."),
        Script::text("short answer"),
    ]);
    let mut agent = agent(
        first.clone(),
        Mode::Auto,
        Arc::new(NonInteractive),
        dir.path(),
    );
    agent.config_mut().context_window = 200_000;
    run(&mut agent, &long).await;
    run(&mut agent, &long).await;
    agent.switch_model(SessionModel {
        provider: small.clone(),
        id: "local/small".into(),
        name: "small".into(),
        context_window: 2_048,
        request: RequestOptions::default(),
        text_tool_calls: false,
        tools: None,
        edit_section: None,
    });
    let (_, events) = run(&mut agent, "and now?").await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, harness_core::event::AgentEvent::Compacted { .. })),
        "{events:?}"
    );
    let requests = small.requests();
    assert_eq!(requests.len(), 2);
    let answered = &requests[1];
    let sent: String = answered
        .messages
        .iter()
        .map(|m| match m {
            Message::User { content } | Message::Assistant { content, .. } => content.clone(),
            Message::Tool { content, .. } => content.clone(),
        })
        .collect();
    assert!(
        sent.contains("The user asked about lorem ipsum twice."),
        "{sent}"
    );
    assert!(!sent.contains(&long), "the old messages were sent whole");
    assert!(agent.context_usage().total < 2_048);
}

// ---- the model a child is built from -------------------------------------------------------

mod parent_model {
    use super::*;
    use async_trait::async_trait;
    use harness_core::{
        agent::{Agent, AgentConfig},
        agents::ParentModel,
        engine::{EngineConfig, PermissionEngine},
        message::ToolSpec,
        permission::Action,
        tool::{Tool, ToolContext, ToolOutput, ToolRegistry},
        turn::{TurnInput, TurnModel},
    };
    use serde_json::{Value, json};
    use std::sync::Mutex;

    /// (model id, window, local) as the parent says it is on, each time `peek` is called.
    type Seen = Arc<Mutex<Vec<(String, u64, bool)>>>;

    /// Notes which model the parent says it is on when it is called.
    struct Peek {
        cell: ParentModel,
        seen: Seen,
    }

    #[async_trait]
    impl Tool for Peek {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "peek".into(),
                description: "looks".into(),
                parameters: json!({"type": "object"}),
            }
        }
        fn action(&self, _args: &Value, ctx: &ToolContext) -> Action {
            Action::Read(ctx.workspace.clone())
        }
        async fn run(&self, _args: Value, _ctx: &ToolContext) -> ToolOutput {
            let now = self.cell.get().expect("the agent published its model");
            self.seen
                .lock()
                .unwrap()
                .push((now.id, now.context_window, now.local));
            ToolOutput::ok("peeked")
        }
    }

    fn parent(dir: &std::path::Path, provider: Arc<MockProvider>) -> (Agent, ParentModel, Seen) {
        let cell = ParentModel::default();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let tools = ToolRegistry::new(vec![Arc::new(Peek {
            cell: cell.clone(),
            seen: seen.clone(),
        })]);
        let policy = Arc::new(PermissionEngine::new(EngineConfig {
            mode: Mode::Auto,
            workspace: dir.to_path_buf(),
            read_dirs: vec![],
            rules: Default::default(),
            sandbox_available: false,
            writes_need_approval: false,
        }));
        let mut config = AgentConfig::new("mock/m1", "m1", "prompt", dir.join(".spill"));
        config.context_window = 50_000;
        let agent = Agent::new(
            provider,
            tools,
            policy,
            Arc::new(NonInteractive),
            config,
            ToolContext::new(dir),
        )
        .with_model_cell(cell.clone());
        (agent, cell, seen)
    }

    #[tokio::test]
    async fn the_model_a_child_is_built_from_follows_the_session_and_its_turns() {
        let dir = tempfile::tempdir().unwrap();
        let first = MockProvider::new(vec![
            Script::tool_call("c1", "peek", json!({})),
            Script::text("one"),
        ]);
        let (mut agent, cell, seen) = parent(dir.path(), first);
        // Before any turn it is the model the session started on.
        let at_start = cell.get().unwrap();
        assert_eq!(
            (at_start.id.as_str(), at_start.context_window),
            ("mock/m1", 50_000)
        );
        run(&mut agent, "go").await;
        // `/model`: the next child is built from the new one, window, options and side included.
        let second = MockProvider::new(vec![
            Script::tool_call("c2", "peek", json!({})),
            Script::text("two"),
            Script::tool_call("c3", "peek", json!({})),
            Script::text("three"),
        ]);
        agent.switch_model(SessionModel {
            provider: second.clone(),
            id: "other/big".into(),
            name: "big".into(),
            context_window: 200_000,
            request: RequestOptions {
                local: true,
                max_output_tokens: Some(4_096),
                ..RequestOptions::default()
            },
            text_tool_calls: false,
            tools: None,
            edit_section: None,
        });
        let now = cell.get().unwrap();
        assert_eq!(
            (now.id.as_str(), now.context_window, now.local),
            ("other/big", 200_000, true)
        );
        assert_eq!(now.request.max_output_tokens, Some(4_096));
        run(&mut agent, "again").await;
        // A turn on a role's model: a child spawned during it is built from that one; after it,
        // from the session's again.
        let turn_model = TurnModel {
            provider: second,
            id: "role/plan".into(),
            name: "plan".into(),
            local: false,
            tools: None,
            edit_section: None,
            context_window: Some(90_000),
            request: None,
            text_tool_calls: false,
        };
        let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
        agent
            .run_turn(
                TurnInput {
                    parts: vec![harness_core::turn::InputPart::Text("plan".into())],
                    model: Some(turn_model),
                    ..TurnInput::default()
                },
                &events,
                tokio_util::sync::CancellationToken::new(),
            )
            .await;
        assert_eq!(
            *seen.lock().unwrap(),
            vec![
                ("mock/m1".to_string(), 50_000, false),
                ("other/big".to_string(), 200_000, true),
                ("role/plan".to_string(), 90_000, false),
            ]
        );
        assert_eq!(cell.get().unwrap().id, "other/big");
    }
}
