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
