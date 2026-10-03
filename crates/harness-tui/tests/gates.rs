//! Gates in the interactive session: the last failure is shown, and queued and send-now input
//! wait for the test run.

mod common;

use std::{sync::Arc, time::Duration};

use common::{agent, options, press, screen, shows, start, type_text};
use harness_core::{
    gate::Gates,
    message::Message,
    permission::Mode,
    testing::{MockProvider, Script},
};
use harness_tui::app::{Host, Prepared};
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use serde_json::json;

struct NoCommands;

impl Host for NoCommands {
    fn is_command(&self, _name: &str) -> bool {
        false
    }
    fn prepare(&mut self, _typed: &str) -> Prepared {
        unreachable!()
    }
}

fn user_messages(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .filter_map(|m| match m {
            Message::User { content } => Some(content.clone()),
            _ => None,
        })
        .collect()
}

fn ctrl_s(ui: &mut harness_tui::ui::Ui<ratatui::backend::TestBackend>) {
    ui.handle(Event::Key(KeyEvent::new(
        KeyCode::Char('s'),
        KeyModifiers::CONTROL,
    )))
    .unwrap();
}

/// A test command that says it started, takes a second and fails: the time to type while it runs.
fn slow_failing(dir: &std::path::Path) -> String {
    format!(
        "touch {}/started; sleep 1; echo 'FAILED: 1 test'; exit 1",
        dir.display()
    )
}

/// Until the test command has started.
async fn while_the_tests_run(
    ui: &mut harness_tui::ui::Ui<ratatui::backend::TestBackend>,
    dir: &std::path::Path,
) {
    let started = dir.join("started");
    tokio::time::timeout(Duration::from_secs(10), ui.until(move |_| started.exists()))
        .await
        .expect("the test command starts")
        .unwrap();
}

async fn settle(ui: &mut harness_tui::ui::Ui<ratatui::backend::TestBackend>) {
    tokio::time::timeout(Duration::from_secs(20), ui.settle())
        .await
        .expect("the turns end")
        .unwrap();
}

fn script() -> Arc<MockProvider> {
    MockProvider::new(vec![
        Script::tool_call("c1", "write", json!({"path": "a.txt", "content": "x\n"})),
        Script::text("done"),
        Script::text("still done"),
        Script::text("README updated."),
    ])
}

// Spec "Gates and queued input are ordered", the queued input: delivered after the turn,
// continuations included.
#[tokio::test]
async fn queued_input_waits_for_the_test_run_and_the_continuations() {
    let dir = tempfile::tempdir().unwrap();
    let provider = script();
    let agent = agent(provider.clone(), dir.path(), Mode::Auto).with_gates(Gates {
        test: Some(slow_failing(dir.path())),
        ..Gates::default()
    });
    let (mut ui, _) = start(agent, Box::new(NoCommands), options(dir.path(), Mode::Auto));
    type_text(&mut ui, "write a.txt");
    press(&mut ui, KeyCode::Enter);
    while_the_tests_run(&mut ui, dir.path()).await;
    type_text(&mut ui, "also update the README");
    press(&mut ui, KeyCode::Enter);
    settle(&mut ui).await;
    let requests = provider.requests();
    assert_eq!(requests.len(), 4, "{requests:#?}");
    // The gate's continuation is part of the first turn: the model saw the gate result before the
    // queued message, which is a turn of its own.
    let third = user_messages(&requests[2].messages);
    assert!(
        third.last().unwrap().contains("test gate failed"),
        "{third:#?}"
    );
    assert!(!third.iter().any(|m| m.contains("README")), "{third:#?}");
    // The failure that stopped the turn is in the history, so the next message carries it.
    let fourth = user_messages(&requests[3].messages);
    assert!(
        fourth.last().unwrap().ends_with("also update the README"),
        "{fourth:#?}"
    );
    ui.finish().await.unwrap();
}

// Spec "Send-now input during the test run".
#[tokio::test]
async fn send_now_input_during_the_test_run_comes_with_the_gate_result() {
    let dir = tempfile::tempdir().unwrap();
    let provider = script();
    let agent = agent(provider.clone(), dir.path(), Mode::Auto).with_gates(Gates {
        test: Some(slow_failing(dir.path())),
        ..Gates::default()
    });
    let (mut ui, _) = start(agent, Box::new(NoCommands), options(dir.path(), Mode::Auto));
    type_text(&mut ui, "write a.txt");
    press(&mut ui, KeyCode::Enter);
    while_the_tests_run(&mut ui, dir.path()).await;
    type_text(&mut ui, "skip the flaky test");
    ctrl_s(&mut ui);
    settle(&mut ui).await;
    let third = user_messages(&provider.requests()[2].messages);
    let last = third.last().unwrap();
    assert!(last.contains("test gate failed"), "{last}");
    assert!(last.ends_with("skip the flaky test"), "{last}");
    ui.finish().await.unwrap();
}

// Spec "Identical failure": the interactive session shows the last failure.
#[tokio::test]
async fn a_turn_that_ends_on_a_gate_failure_shows_the_last_failure() {
    let dir = tempfile::tempdir().unwrap();
    let provider = script();
    let agent = agent(provider.clone(), dir.path(), Mode::Auto).with_gates(Gates {
        test: Some(slow_failing(dir.path())),
        ..Gates::default()
    });
    let (mut ui, _) = start(agent, Box::new(NoCommands), options(dir.path(), Mode::Auto));
    type_text(&mut ui, "write a.txt");
    press(&mut ui, KeyCode::Enter);
    settle(&mut ui).await;
    assert!(
        shows(&ui, "tests failed (exit code 1)"),
        "{:#?}",
        screen(&ui)
    );
    assert!(shows(&ui, "FAILED: 1 test"), "{:#?}", screen(&ui));
    assert!(shows(&ui, "the tests still fail"), "{:#?}", screen(&ui));
    // The session accepts input afterwards.
    assert!(!ui.app().transcript.busy());
    ui.finish().await.unwrap();
}

// Review Focus: terminal escapes in a failing command's output are not passed to the terminal.
#[tokio::test]
async fn escapes_in_a_gate_failure_do_not_reach_the_screen() {
    let dir = tempfile::tempdir().unwrap();
    let provider = script();
    let agent = agent(provider, dir.path(), Mode::Auto).with_gates(Gates {
        test: Some(r"printf '\033[31mRED\033[0m \033]0;owned\007 end\n'; exit 1".into()),
        ..Gates::default()
    });
    let (mut ui, _) = start(agent, Box::new(NoCommands), options(dir.path(), Mode::Auto));
    type_text(&mut ui, "write a.txt");
    press(&mut ui, KeyCode::Enter);
    settle(&mut ui).await;
    assert!(shows(&ui, "RED"), "{:#?}", screen(&ui));
    for row in common::everything(&ui) {
        assert!(!row.contains('\u{1b}') && !row.contains('\u{7}'), "{row:?}");
    }
    ui.finish().await.unwrap();
}
