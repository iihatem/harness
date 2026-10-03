//! `/escalate` and the suggestion: the next turn runs on the escalation model when the user asks,
//! and a suggestion changes nothing by itself.

mod common;

use std::sync::{Arc, Mutex};

use common::*;
use futures::future::BoxFuture;
use harness_core::{
    agent::SessionModel,
    event::{AgentEvent, EscalationTrigger},
    message::RequestOptions,
    permission::Mode,
    provider::ProviderEvent,
    testing::{MockProvider, Script},
};
use harness_tui::{
    app::{Host, ModelSwitch, Prepared},
    style::Theme,
    transcript::Transcript,
};
use serde_json::json;
use tokio_util::sync::CancellationToken;

struct Escalating {
    to: Option<&'static str>,
    big: Arc<MockProvider>,
    switched: Arc<Mutex<Vec<String>>>,
}

impl Host for Escalating {
    fn is_command(&self, _name: &str) -> bool {
        false
    }
    fn prepare(&mut self, _typed: &str) -> Prepared {
        unreachable!()
    }
    fn escalation(&self) -> Option<String> {
        self.to.map(String::from)
    }
    fn switch_model(
        &self,
        id: &str,
        _cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<ModelSwitch, String>> {
        self.switched.lock().unwrap().push(id.to_string());
        let model = SessionModel {
            provider: self.big.clone(),
            id: id.to_string(),
            name: id.rsplit('/').next().unwrap().to_string(),
            context_window: 100_000,
            request: RequestOptions::default(),
            text_tool_calls: false,
            tools: None,
            edit_section: None,
        };
        Box::pin(async move {
            Ok(ModelSwitch {
                model,
                window_note: "from the test".into(),
                warnings: Vec::new(),
            })
        })
    }
}

type Ui = harness_tui::ui::Ui<ratatui::backend::TestBackend>;

fn open(
    to: Option<&'static str>,
    script: Vec<Script>,
    big: &Arc<MockProvider>,
) -> (tempfile::TempDir, Ui, Arc<Mutex<Vec<String>>>) {
    let dir = tempfile::tempdir().unwrap();
    let switched = Arc::new(Mutex::new(Vec::new()));
    let (ui, _) = start(
        agent(MockProvider::new(script), dir.path(), Mode::Auto)
            .with_escalation(to.map(String::from)),
        Box::new(Escalating {
            to,
            big: big.clone(),
            switched: switched.clone(),
        }),
        options(dir.path(), Mode::Auto),
    );
    (dir, ui, switched)
}

// Spec "Escalating": the next turn runs on `escalation.to`, and a `ModelSwitched` event with
// reason `escalation` is shown.
#[tokio::test]
async fn escalate_runs_the_next_turn_on_the_escalation_model() {
    let big = MockProvider::new(vec![Script::text("from the big one")]);
    let (_dir, mut ui, switched) = open(Some("chatgpt/gpt-5"), vec![], &big);
    send(&mut ui, "/escalate");
    settle(&mut ui).await;
    assert_eq!(*switched.lock().unwrap(), ["chatgpt/gpt-5"]);
    assert!(
        shows(
            &ui,
            "switched to chatgpt/gpt-5 (main role, from mock/m; escalation)"
        ),
        "{:#?}",
        everything(&ui)
    );
    send(&mut ui, "try this");
    settle(&mut ui).await;
    assert_eq!(big.requests().len(), 1);
    assert!(shows(&ui, "from the big one"));
}

// Spec "Nothing configured".
#[tokio::test]
async fn escalate_without_a_model_says_so_and_changes_nothing() {
    let big = MockProvider::new(vec![]);
    let (_dir, mut ui, switched) = open(None, vec![], &big);
    send(&mut ui, "/escalate");
    assert!(
        shows_wrapped(&ui, "no escalation model is configured"),
        "{:#?}",
        everything(&ui)
    );
    assert!(switched.lock().unwrap().is_empty());
}

#[tokio::test]
async fn escalate_on_the_escalation_model_says_so() {
    let big = MockProvider::new(vec![]);
    let (_dir, mut ui, switched) = open(Some("mock/m"), vec![], &big);
    send(&mut ui, "/escalate");
    assert!(shows(&ui, "already on mock/m"));
    assert!(switched.lock().unwrap().is_empty());
}

#[tokio::test]
async fn escalate_waits_for_the_turn() {
    let big = MockProvider::new(vec![]);
    let (_dir, mut ui, switched) = open(
        Some("chatgpt/gpt-5"),
        vec![Script::Hang(vec![ProviderEvent::TextDelta(
            "thinking".into(),
        )])],
        &big,
    );
    send(&mut ui, "a long task");
    until(&mut ui, |app| app.busy()).await;
    send(&mut ui, "/escalate");
    assert!(shows(
        &ui,
        "works between turns: press Esc to stop this one"
    ));
    assert!(switched.lock().unwrap().is_empty());
}

// Spec "Three invalid calls": the suggestion is shown with the hint `/escalate`, once, and the
// model is unchanged.
#[tokio::test]
async fn the_suggestion_is_shown_with_the_hint_and_changes_nothing() {
    let big = MockProvider::new(vec![]);
    let mut script: Vec<Script> = (0..4)
        .map(|i| Script::tool_call(&format!("c{i}"), "nope", json!({})))
        .collect();
    script.push(Script::text("giving up"));
    let (_dir, mut ui, switched) = open(Some("chatgpt/gpt-5"), script, &big);
    send(&mut ui, "go");
    settle(&mut ui).await;
    let lines: Vec<String> = everything(&ui)
        .into_iter()
        .filter(|r| r.contains("escalation suggested"))
        .collect();
    assert_eq!(lines.len(), 1, "{:#?}", everything(&ui));
    assert!(
        lines[0].contains("3 invalid tool calls") || shows_wrapped(&ui, "3 invalid tool calls")
    );
    assert!(shows_wrapped(
        &ui,
        "/escalate runs the next turn on chatgpt/gpt-5"
    ));
    assert!(switched.lock().unwrap().is_empty());
}

fn text_of(event: AgentEvent) -> String {
    let mut transcript = Transcript::new(Theme::monochrome());
    transcript.on_event(&event, 300);
    transcript
        .take_finished()
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .filter(|text| !text.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn each_trigger_says_what_happened() {
    let suggested = |trigger, count, first: Option<&str>| AgentEvent::EscalationSuggested {
        trigger,
        count,
        first_line: first.map(String::from),
        to: "chatgpt/gpt-5".into(),
    };
    assert_eq!(
        text_of(suggested(
            EscalationTrigger::InvalidToolCalls,
            3,
            Some("unknown tool `x`")
        )),
        "escalation suggested: 3 invalid tool calls this turn (last: unknown tool `x`); /escalate runs the next turn on chatgpt/gpt-5"
    );
    assert_eq!(
        text_of(suggested(EscalationTrigger::IdenticalFailures, 3, None)),
        "escalation suggested: 3 identical failing tool results this turn; /escalate runs the next turn on chatgpt/gpt-5"
    );
    assert_eq!(
        text_of(suggested(
            EscalationTrigger::GateFailed,
            2,
            Some("FAILED: 1 test")
        )),
        "escalation suggested: the same gate failed 2 times this turn (last: FAILED: 1 test); /escalate runs the next turn on chatgpt/gpt-5"
    );
}
