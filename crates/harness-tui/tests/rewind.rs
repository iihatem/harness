//! `/rewind`, and Esc twice on empty input: the list of the user's messages in a full-screen
//! view, the choice of code, conversation or both, and undoing the last rewind.

mod common;

use std::{path::Path, sync::Arc};

use common::*;
use harness_core::{
    agent::REWIND_LIMITS,
    checkpoint::Checkpoints,
    message::Message,
    permission::Mode,
    session::Session,
    testing::{MockProvider, Script},
};
use harness_tui::{
    app::{Host, Prepared},
    ui::Ui,
};
use ratatui::{backend::TestBackend, crossterm::event::KeyCode};
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

/// A session in `dir` saved under `data`, with checkpoints there when `checkpoints` is set.
fn open(
    provider: Arc<MockProvider>,
    dir: &Path,
    data: &Path,
    checkpoints: bool,
) -> (Ui<TestBackend>, Log) {
    let session = Session::create(&data.join("sessions"), dir);
    let cp = checkpoints.then(|| {
        Arc::new(Checkpoints::open(&data.join("checkpoints.git"), dir, session.id()).unwrap())
    });
    let agent = agent(provider, dir, Mode::Auto)
        .with_session(session)
        .with_checkpoints(cp);
    start(agent, Box::new(NoCommands), options(dir, Mode::Auto))
}

fn dirs() -> (tempfile::TempDir, tempfile::TempDir) {
    (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap())
}

fn canonical(dir: &tempfile::TempDir) -> std::path::PathBuf {
    dir.path().canonicalize().unwrap()
}

/// The user and assistant text of the provider's last request.
fn last_request(provider: &MockProvider) -> Vec<String> {
    provider
        .requests()
        .last()
        .map(|r| {
            r.messages
                .iter()
                .filter_map(|m| match m {
                    Message::User { content } => Some(content.clone()),
                    Message::Assistant { content, .. } => Some(content.clone()),
                    Message::Tool { .. } => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn rewinding_the_conversation_goes_back_and_gives_the_message_back_to_edit() {
    let (dir, data) = dirs();
    let dir = canonical(&dir);
    let provider = MockProvider::new(vec![
        Script::text("Answer one."),
        Script::text("Answer two."),
        Script::text("Answer three."),
    ]);
    let (mut ui, log) = open(provider.clone(), &dir, data.path(), false);
    send(&mut ui, "first question");
    settle(&mut ui).await;
    send(&mut ui, "second question");
    settle(&mut ui).await;
    send(&mut ui, "/rewind");
    until_armed(&ui).await;
    let shown = screen(&ui);
    assert_eq!(shown[0], "Rewind to before which message?");
    let second = shown
        .iter()
        .position(|r| r.contains("second question"))
        .expect("the second message");
    let first = shown
        .iter()
        .position(|r| r.contains("first question"))
        .expect("the first message");
    // The latest first.
    assert!(second < first, "{shown:#?}");
    assert!(
        shown
            .iter()
            .any(|r| r.contains("Rewinding restores files in the workspace only")),
        "{shown:#?}"
    );
    press(&mut ui, KeyCode::Enter);
    until_armed(&ui).await;
    let shown = screen(&ui);
    assert_eq!(shown[0], "Restore what, to before this message?");
    for choice in ["code and conversation", "conversation only", "code only"] {
        assert!(shown.iter().any(|r| r.contains(choice)), "{shown:#?}");
    }
    assert!(
        shown.iter().any(|r| r.contains("second question")),
        "{shown:#?}"
    );
    press(&mut ui, KeyCode::Down);
    press(&mut ui, KeyCode::Enter);
    settle(&mut ui).await;
    assert!(ui.app().picker().is_none());
    assert_eq!(*log.lock().unwrap(), ["enter", "leave"]);
    assert!(shows(
        &ui,
        "rewound the conversation to before: second question"
    ));
    // The message is back in the input, to send again as it is or changed.
    assert_eq!(ui.app().editor().text(), "second question");
    ctrl(&mut ui, 'u');
    send(&mut ui, "a different second question");
    settle(&mut ui).await;
    let sent = last_request(&provider);
    assert!(sent.iter().any(|m| m == "first question"), "{sent:?}");
    assert!(sent.iter().any(|m| m == "Answer one."), "{sent:?}");
    assert!(!sent.iter().any(|m| m == "second question"), "{sent:?}");
    assert!(!sent.iter().any(|m| m == "Answer two."), "{sent:?}");
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn rewinding_code_restores_the_files_and_can_be_undone() {
    let (dir, data) = dirs();
    let dir = canonical(&dir);
    std::fs::write(dir.join("notes.txt"), "original\n").unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call("r1", "read", json!({"path": "notes.txt"})),
        Script::tool_call(
            "w1",
            "write",
            json!({"path": "notes.txt", "content": "changed\n"}),
        ),
        Script::text("Wrote it."),
    ]);
    let (mut ui, _log) = open(provider, &dir, data.path(), true);
    send(&mut ui, "change the notes");
    settle(&mut ui).await;
    assert_eq!(
        std::fs::read_to_string(dir.join("notes.txt")).unwrap(),
        "changed\n"
    );
    send(&mut ui, "/rewind");
    until_armed(&ui).await;
    press(&mut ui, KeyCode::Enter);
    until_armed(&ui).await;
    // Code and conversation.
    press(&mut ui, KeyCode::Enter);
    settle(&mut ui).await;
    assert!(shows(
        &ui,
        "rewound the code and the conversation to before: change the notes"
    ));
    assert_eq!(
        std::fs::read_to_string(dir.join("notes.txt")).unwrap(),
        "original\n"
    );
    ctrl(&mut ui, 'u');
    // The list now offers to undo it; the message itself is no longer in the conversation.
    send(&mut ui, "/rewind");
    until_armed(&ui).await;
    let shown = screen(&ui);
    assert!(shown[3].contains("undo the last rewind"), "{shown:#?}");
    assert!(
        !shown.iter().any(|r| r.contains("change the notes")),
        "{shown:#?}"
    );
    press(&mut ui, KeyCode::Enter);
    settle(&mut ui).await;
    assert!(shows(&ui, "undid the last rewind"));
    assert_eq!(
        std::fs::read_to_string(dir.join("notes.txt")).unwrap(),
        "changed\n"
    );
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn rewinding_code_without_checkpoints_is_refused() {
    let (dir, data) = dirs();
    let dir = canonical(&dir);
    let provider = MockProvider::new(vec![Script::text("Hi.")]);
    let (mut ui, _log) = open(provider, &dir, data.path(), false);
    send(&mut ui, "hello");
    settle(&mut ui).await;
    send(&mut ui, "/rewind");
    until_armed(&ui).await;
    press(&mut ui, KeyCode::Enter);
    until_armed(&ui).await;
    press(&mut ui, KeyCode::End);
    press(&mut ui, KeyCode::Enter);
    settle(&mut ui).await;
    assert!(shows(
        &ui,
        "error: the rewind failed: checkpoints are disabled for this session"
    ));
    assert!(!ui.app().busy());
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn esc_twice_on_empty_input_opens_the_rewind_list() {
    let (dir, data) = dirs();
    let dir = canonical(&dir);
    let provider = MockProvider::new(vec![Script::text("Hi.")]);
    let (mut ui, _log) = open(provider, &dir, data.path(), false);
    send(&mut ui, "hello");
    settle(&mut ui).await;
    // Not with text in the input.
    type_text(&mut ui, "draft");
    press(&mut ui, KeyCode::Esc);
    press(&mut ui, KeyCode::Esc);
    assert!(ui.app().picker().is_none());
    ctrl(&mut ui, 'u');
    press(&mut ui, KeyCode::Esc);
    assert!(ui.app().picker().is_none());
    press(&mut ui, KeyCode::Esc);
    assert!(ui.app().picker().is_some());
    assert_eq!(screen(&ui)[0], "Rewind to before which message?");
    until_armed(&ui).await;
    press(&mut ui, KeyCode::Esc);
    assert!(ui.app().picker().is_none());
    send(&mut ui, "/help");
    assert!(shows(&ui, "Esc twice  rewind to before an earlier message"));
    ui.finish().await.unwrap();
}

// The choice of what to restore is a picker of its own, and takes keys only after a pause, as
// the first one does: an Enter typed ahead cannot confirm a rewind.
#[tokio::test]
async fn keys_typed_ahead_of_the_rewind_choice_do_not_confirm_it() {
    let (dir, data) = dirs();
    let dir = canonical(&dir);
    let provider = MockProvider::new(vec![Script::text("Hi.")]);
    let (mut ui, _log) = open(provider, &dir, data.path(), false);
    send(&mut ui, "hello");
    settle(&mut ui).await;
    send(&mut ui, "/rewind");
    until_armed(&ui).await;
    press(&mut ui, KeyCode::Enter);
    assert_eq!(screen(&ui)[0], "Restore what, to before this message?");
    press(&mut ui, KeyCode::Enter);
    press(&mut ui, KeyCode::Enter);
    assert_eq!(screen(&ui)[0], "Restore what, to before this message?");
    assert!(!shows(&ui, "rewound"));
    until_armed(&ui).await;
    press(&mut ui, KeyCode::Down);
    press(&mut ui, KeyCode::Enter);
    settle(&mut ui).await;
    assert!(shows(&ui, "rewound the conversation to before: hello"));
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn with_nothing_to_rewind_it_says_so() {
    let (dir, data) = dirs();
    let dir = canonical(&dir);
    let (mut ui, log) = open(MockProvider::new(Vec::new()), &dir, data.path(), false);
    send(&mut ui, "/rewind");
    assert!(ui.app().picker().is_none());
    assert!(shows(&ui, "there is nothing to rewind yet"));
    assert!(log.lock().unwrap().is_empty());
    ui.finish().await.unwrap();
}

#[test]
fn the_limits_are_those_the_spec_states() {
    for effect in ["network calls", "databases", "pushed commits", "submodules"] {
        assert!(REWIND_LIMITS.contains(effect), "{effect}");
    }
}

// Review A minor 4: the rewind cannot be stopped, so its banner does not say Esc stops it.
#[tokio::test]
async fn the_rewinding_banner_does_not_offer_to_stop_it() {
    let (dir, data) = dirs();
    let dir = canonical(&dir);
    let provider = MockProvider::new(vec![Script::text("Answer one.")]);
    let (mut ui, _log) = open(provider, &dir, data.path(), false);
    send(&mut ui, "first question");
    settle(&mut ui).await;
    send(&mut ui, "/rewind");
    until_armed(&ui).await;
    press(&mut ui, KeyCode::Enter);
    until_armed(&ui).await;
    press(&mut ui, KeyCode::Down);
    press(&mut ui, KeyCode::Enter);
    let shown = screen(&ui);
    assert!(shown.iter().any(|r| r.contains("rewinding…")), "{shown:#?}");
    assert!(
        !shown.iter().any(|r| r.contains("Esc to stop")),
        "{shown:#?}"
    );
    settle(&mut ui).await;
    ui.finish().await.unwrap();
}

// Review A minor 6: what the user typed while the rewind ran is not replaced by the message it
// gives back: both are kept, the message in the history.
#[tokio::test]
async fn what_was_typed_during_a_rewind_is_kept_beside_the_message_it_gives_back() {
    let (dir, data) = dirs();
    let dir = canonical(&dir);
    let provider = MockProvider::new(vec![Script::text("Answer one.")]);
    let (mut ui, _log) = open(provider, &dir, data.path(), false);
    send(&mut ui, "first question");
    settle(&mut ui).await;
    send(&mut ui, "/rewind");
    until_armed(&ui).await;
    press(&mut ui, KeyCode::Enter);
    until_armed(&ui).await;
    press(&mut ui, KeyCode::Down);
    press(&mut ui, KeyCode::Enter);
    type_text(&mut ui, "typed meanwhile");
    settle(&mut ui).await;
    assert_eq!(ui.app().editor().text(), "typed meanwhile");
    assert!(shows(&ui, "first question"));
    ctrl(&mut ui, 'u');
    press(&mut ui, KeyCode::Up);
    assert_eq!(ui.app().editor().text(), "first question");
    ui.finish().await.unwrap();
}
