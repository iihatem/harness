//! `/mode` and `/compact`, and the rule for the commands that change the session: they run
//! between turns.

mod common;

use common::*;
use harness_core::{
    message::Message,
    permission::Mode,
    provider::ProviderEvent,
    testing::{MockProvider, Script},
};
use harness_tui::app::{Host, Prepared};
use ratatui::crossterm::event::KeyCode;

struct NoCommands;

impl Host for NoCommands {
    fn is_command(&self, _name: &str) -> bool {
        false
    }
    fn prepare(&mut self, _typed: &str) -> Prepared {
        unreachable!()
    }
}

fn open(
    provider: std::sync::Arc<MockProvider>,
    dir: &std::path::Path,
) -> (harness_tui::ui::Ui<ratatui::backend::TestBackend>, Log) {
    start(
        agent(provider, dir, Mode::Auto),
        Box::new(NoCommands),
        options(dir, Mode::Auto),
    )
}

/// Everything the provider was sent in its last request, as text.
fn last_request(provider: &MockProvider) -> String {
    provider
        .requests()
        .last()
        .map(|r| {
            r.messages
                .iter()
                .map(|m| match m {
                    Message::User { content } => content.clone(),
                    Message::Assistant { content, .. } => content.clone(),
                    Message::Tool { content, .. } => content.clone(),
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn mode_with_a_name_switches_and_tells_the_model() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![Script::text("Planning.")]);
    let (mut ui, log) = open(provider.clone(), dir.path());
    send(&mut ui, "/mode plan");
    settle(&mut ui).await;
    assert_eq!(ui.app().mode(), Mode::Plan);
    assert!(shows(&ui, "switched to plan mode"));
    assert!(shows(&ui, "mock/m · plan"));
    send(&mut ui, "look around");
    settle(&mut ui).await;
    assert!(
        last_request(&provider).contains("[harness] The approval mode is now plan"),
        "{}",
        last_request(&provider)
    );
    // No picker opened.
    assert!(log.lock().unwrap().is_empty());
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn mode_alone_opens_a_picker_in_a_full_screen_view() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(Vec::new());
    let (mut ui, log) = open(provider, dir.path());
    send(&mut ui, "/mode");
    assert!(ui.app().picker().is_some());
    assert!(ui.terminal().in_full_screen());
    let shown = screen(&ui);
    assert_eq!(shown[0], "Choose the approval mode");
    for mode in ["plan", "read-only", "ask", "auto"] {
        assert!(
            shown.iter().any(|r| r.trim_start().starts_with(mode)),
            "{shown:#?}"
        );
    }
    // full-access is not among the items; the footer says how to get it.
    assert!(
        !shown.iter().any(|r| r.starts_with("  full-access")),
        "{shown:#?}"
    );
    assert!(
        shown
            .iter()
            .any(|r| r.contains("full-access is chosen only when harness starts")),
        "{shown:#?}"
    );
    // The current mode is where it starts.
    assert!(
        shown
            .iter()
            .any(|r| r.contains("auto") && r.contains("(current)")),
        "{shown:#?}"
    );
    until_armed(&ui).await;
    type_text(&mut ui, "ask");
    press(&mut ui, KeyCode::Enter);
    settle(&mut ui).await;
    assert!(ui.app().picker().is_none());
    assert!(!ui.terminal().in_full_screen());
    assert_eq!(*log.lock().unwrap(), ["enter", "leave"]);
    assert_eq!(ui.app().mode(), Mode::Ask);
    assert!(shows(&ui, "switched to ask mode"));
    ui.finish().await.unwrap();
}

// Keys typed ahead of a picker went to the input: they neither choose an item nor close it, and
// the picker takes keys after a pause, as an approval does.
#[tokio::test]
async fn keys_typed_ahead_of_a_picker_do_not_choose_in_it() {
    let dir = tempfile::tempdir().unwrap();
    let (mut ui, _log) = open(MockProvider::new(Vec::new()), dir.path());
    send(&mut ui, "/mode");
    assert!(ui.app().picker().is_some());
    type_text(&mut ui, "ask");
    press(&mut ui, KeyCode::Enter);
    press(&mut ui, KeyCode::Esc);
    assert!(ui.app().picker().is_some());
    assert_eq!(ui.app().mode(), Mode::Auto);
    until_armed(&ui).await;
    press(&mut ui, KeyCode::Esc);
    assert!(ui.app().picker().is_none());
    assert_eq!(ui.app().mode(), Mode::Auto);
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn esc_closes_the_mode_picker_without_a_switch() {
    let dir = tempfile::tempdir().unwrap();
    let (mut ui, log) = open(MockProvider::new(Vec::new()), dir.path());
    send(&mut ui, "/mode");
    until_armed(&ui).await;
    press(&mut ui, KeyCode::Esc);
    assert!(ui.app().picker().is_none());
    assert_eq!(*log.lock().unwrap(), ["enter", "leave"]);
    assert_eq!(ui.app().mode(), Mode::Auto);
    assert!(!shows(&ui, "switched to"));
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn full_access_and_unknown_modes_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (mut ui, _log) = open(MockProvider::new(Vec::new()), dir.path());
    send(&mut ui, "/mode full-access");
    assert!(shows(
        &ui,
        "error: full-access is chosen only when harness starts"
    ));
    send(&mut ui, "/mode fast");
    assert!(shows(&ui, "error: unknown mode `fast`"));
    send(&mut ui, "/mode auto");
    assert!(shows(&ui, "already in auto mode"));
    assert_eq!(ui.app().mode(), Mode::Auto);
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn compact_summarizes_the_conversation_and_shows_the_summary() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::text("The answer is 4."),
        Script::text("Also 5."),
        Script::text("The user asked two sums."),
        Script::text("Six."),
    ]);
    let (mut ui, _log) = open(provider.clone(), dir.path());
    send(&mut ui, "what is 2+2?");
    settle(&mut ui).await;
    send(&mut ui, "and 2+3?");
    settle(&mut ui).await;
    send(&mut ui, "/compact keep the numbers");
    settle(&mut ui).await;
    assert!(!ui.app().busy());
    assert!(shows(&ui, "compacted the conversation from about"));
    assert!(shows(&ui, "The user asked two sums."));
    // The summary request carried the focus.
    assert!(
        last_request(&provider).contains("Focus especially on: keep the numbers"),
        "{}",
        last_request(&provider)
    );
    send(&mut ui, "and 3+3?");
    settle(&mut ui).await;
    let next = last_request(&provider);
    assert!(next.contains("The user asked two sums."), "{next}");
    assert!(!next.contains("what is 2+2?"), "{next}");
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn compact_with_nothing_to_compact_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let (mut ui, _log) = open(MockProvider::new(Vec::new()), dir.path());
    send(&mut ui, "/compact");
    settle(&mut ui).await;
    assert!(shows(&ui, "there is nothing to compact yet"));
    assert!(!ui.app().busy());
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn esc_stops_a_compaction() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::text("One."),
        Script::Hang(vec![ProviderEvent::TextDelta("A summ".into())]),
    ]);
    let (mut ui, _log) = open(provider, dir.path());
    send(&mut ui, "hello");
    settle(&mut ui).await;
    send(&mut ui, "/compact");
    assert!(ui.app().busy());
    assert!(
        screen(&ui)
            .iter()
            .any(|r| r.contains("compacting the conversation")),
        "{:#?}",
        screen(&ui)
    );
    press(&mut ui, KeyCode::Esc);
    settle(&mut ui).await;
    assert!(!ui.app().busy());
    assert!(shows(
        &ui,
        "the conversation was not compacted: interrupted"
    ));
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn commands_that_change_the_session_wait_for_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![Script::Hang(vec![ProviderEvent::TextDelta(
        "thinking".into(),
    )])]);
    let (mut ui, _log) = open(provider, dir.path());
    send(&mut ui, "a long task");
    until(&mut ui, |app| app.busy()).await;
    for command in ["/mode plan", "/compact"] {
        send(&mut ui, command);
        assert_eq!(ui.app().editor().text(), command);
        assert!(
            screen(&ui)
                .iter()
                .any(|r| r.contains("works between turns: press Esc to stop this one")),
            "{:#?}",
            screen(&ui)
        );
        ctrl(&mut ui, 'u');
    }
    assert_eq!(ui.app().mode(), Mode::Auto);
    press(&mut ui, KeyCode::Esc);
    settle(&mut ui).await;
    send(&mut ui, "/mode plan");
    settle(&mut ui).await;
    assert_eq!(ui.app().mode(), Mode::Plan);
    ui.finish().await.unwrap();
}

// Review Focus: Ctrl+C, the reflex for "stop", in a picker. It closes the picker, back to the
// inline screen, and a second Ctrl+C exits as it does anywhere.
#[tokio::test]
async fn ctrl_c_closes_a_picker_and_a_second_one_exits() {
    let dir = tempfile::tempdir().unwrap();
    let (mut ui, log) = open(MockProvider::new(Vec::new()), dir.path());
    send(&mut ui, "/mode");
    assert!(ui.app().picker().is_some());
    ctrl(&mut ui, 'c');
    assert!(ui.app().picker().is_none());
    assert!(!ui.terminal().in_full_screen());
    assert_eq!(*log.lock().unwrap(), ["enter", "leave"]);
    assert_eq!(ui.app().mode(), Mode::Auto);
    assert!(
        screen(&ui)
            .iter()
            .any(|r| r.contains("press Ctrl+C again to exit"))
    );
    let flow = ui
        .handle(ratatui::crossterm::event::Event::Key(
            ratatui::crossterm::event::KeyEvent::new(
                KeyCode::Char('c'),
                ratatui::crossterm::event::KeyModifiers::CONTROL,
            ),
        ))
        .unwrap();
    assert_eq!(flow, harness_tui::ui::Flow::Quit);
    ui.finish().await.unwrap();
}
