//! 1.11: the runtime reports one record for each finished turn: counts and ids only.

mod common;

use std::sync::{Arc, Mutex};

use common::*;
use harness_core::{
    agent::{NonInteractive, SessionModel},
    event::TurnEndReason,
    message::Usage,
    meter::{AccountKind, Avoided, Meter, RequestCost, RequestRecord, TurnRecord},
    permission::Mode,
    provider::{FinishReason, ProviderError, ProviderEvent},
    session::{RewindScope, Session},
    testing::{MockProvider, Script},
};
use serde_json::json;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Recording {
    turns: Mutex<Vec<TurnRecord>>,
    rewound: Mutex<Vec<(String, Vec<String>)>>,
}

impl Meter for Recording {
    fn record_request(&self, _request: &RequestRecord) -> RequestCost {
        RequestCost {
            account: AccountKind::ApiKey,
            billed_usd: Some(0.0),
            list_usd: Some(0.0),
            avoided: Avoided::NotApplicable,
        }
    }
    fn record_turn(&self, turn: &TurnRecord) {
        self.turns.lock().unwrap().push(turn.clone());
    }
    fn turns_rewound(&self, session: &str, turns: &[String]) {
        self.rewound
            .lock()
            .unwrap()
            .push((session.to_string(), turns.to_vec()));
    }
}

fn call(id: &str, name: &str, args: serde_json::Value) -> Script {
    Script::Reply(vec![
        Ok(ProviderEvent::ToolCall(harness_core::message::ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: args.to_string(),
        })),
        Ok(ProviderEvent::Usage(Usage {
            input_tokens: 100,
            output_tokens: 10,
            ..Usage::default()
        })),
        Ok(ProviderEvent::Finished(FinishReason::ToolCalls)),
    ])
}

fn answer() -> Script {
    Script::Reply(vec![
        Ok(ProviderEvent::TextDelta("done".into())),
        Ok(ProviderEvent::Usage(Usage {
            input_tokens: 200,
            output_tokens: 20,
            ..Usage::default()
        })),
        Ok(ProviderEvent::Finished(FinishReason::Stop)),
    ])
}

// A turn on role `main` makes 4 tool calls, 1 of them invalid, and finishes with `completed`.
#[tokio::test]
async fn a_completed_turn_is_recorded_with_its_counts() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        call("c1", "echo", json!({"text": "a"})),
        call("c2", "nonexistent_tool", json!({})),
        call("c3", "echo", json!({"text": "b"})),
        call("c4", "echo", json!({"text": "c"})),
        answer(),
    ]);
    let meter = Arc::new(Recording::default());
    let mut agent = agent(provider, Mode::Auto, Arc::new(NonInteractive), dir.path())
        .with_session(Session::create(&dir.path().join("sessions"), dir.path()))
        .with_meter(meter.clone());
    let (reason, _) = run(&mut agent, "do the work").await;
    assert_eq!(reason, TurnEndReason::Completed);
    let turns = meter.turns.lock().unwrap().clone();
    assert_eq!(turns.len(), 1);
    let turn = &turns[0];
    assert_eq!(turn.role, "main");
    assert_eq!(turn.model, "mock/m1");
    assert_eq!(turn.selected_by, "config");
    assert_eq!(turn.tool_calls, 4);
    assert_eq!(turn.invalid_calls, 1);
    assert_eq!(turn.retries, 0);
    assert_eq!(turn.finish_reason, "completed");
    assert_eq!(turn.input_tokens, 4 * 100 + 200);
    assert_eq!(turn.output_tokens, 4 * 10 + 20);
    assert!(turn.first_token_ms.is_some());
    assert!(turn.ended_at >= turn.started_at && turn.started_at > 1_700_000_000);
    assert_eq!(turn.session, agent.session().id());
    // The turn is named by its user message's entry in the session.
    let first_user = agent.rewind_points()[0].entry.clone();
    assert_eq!(turn.turn, first_user);
    assert_eq!(
        turn.gates.passed + turn.gates.failed + turn.gates.skipped,
        0
    );
}

#[tokio::test(start_paused = true)]
async fn retries_and_an_interrupted_turn_are_counted() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::error(ProviderError::Http {
            status: 503,
            body: String::new(),
            retry_after: Some(std::time::Duration::from_millis(1)),
        }),
        answer(),
        Script::Hang(vec![ProviderEvent::TextDelta("x".into())]),
    ]);
    let meter = Arc::new(Recording::default());
    let mut agent =
        agent(provider, Mode::Auto, Arc::new(NonInteractive), dir.path()).with_meter(meter.clone());
    run(&mut agent, "one").await;
    let cancel = CancellationToken::new();
    let stop = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        stop.cancel();
    });
    run_with(&mut agent, "two", cancel).await;
    let turns = meter.turns.lock().unwrap().clone();
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[0].retries, 1);
    assert_eq!(turns[0].finish_reason, "completed");
    assert_eq!(turns[1].finish_reason, "interrupted");
    // Counts do not carry over from one turn to the next.
    assert_eq!(turns[1].retries, 0);
}

#[tokio::test]
async fn a_model_the_user_switched_to_is_selected_by_user() {
    let dir = tempfile::tempdir().unwrap();
    let first = MockProvider::new(vec![answer()]);
    let second = MockProvider::new(vec![answer()]);
    let meter = Arc::new(Recording::default());
    let mut agent =
        agent(first, Mode::Auto, Arc::new(NonInteractive), dir.path()).with_meter(meter.clone());
    run(&mut agent, "one").await;
    agent.switch_model(SessionModel {
        provider: second,
        id: "mock/m2".into(),
        name: "m2".into(),
        context_window: 32_768,
        request: Default::default(),
        text_tool_calls: false,
        tools: None,
        edit_section: None,
    });
    run(&mut agent, "two").await;
    let turns = meter.turns.lock().unwrap().clone();
    assert_eq!(turns[0].selected_by, "config");
    assert_eq!(
        (turns[1].model.as_str(), turns[1].selected_by.as_str()),
        ("mock/m2", "user")
    );
}

// The user rewinds to before a turn: the meter is told which turns that undoes.
#[tokio::test]
async fn a_rewind_tells_the_meter_which_turns_were_undone() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![answer(), answer(), answer()]);
    let meter = Arc::new(Recording::default());
    let mut agent = agent(provider, Mode::Auto, Arc::new(NonInteractive), dir.path())
        .with_session(Session::create(&dir.path().join("sessions"), dir.path()))
        .with_meter(meter.clone());
    for text in ["one", "two", "three"] {
        run(&mut agent, text).await;
    }
    let points = agent.rewind_points();
    agent
        .rewind(&points[1].entry, RewindScope::Conversation)
        .await
        .unwrap();
    let rewound = meter.rewound.lock().unwrap().clone();
    assert_eq!(rewound.len(), 1);
    assert_eq!(rewound[0].0, agent.session().id());
    // "two" and "three" are undone; "one" stays.
    assert_eq!(
        rewound[0].1,
        vec![points[1].entry.clone(), points[2].entry.clone()]
    );
}
