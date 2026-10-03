//! `/new` and `/resume`: a new session, or another of the project's sessions, continued by the
//! same agent, and the session picker.

mod common;

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use common::*;
use futures::future::BoxFuture;
use harness_core::{
    message::Message,
    permission::Mode,
    session::{self, EntryKind, Session, SessionSummary},
    testing::{MockProvider, Script},
};
use harness_tui::{
    app::{Host, OpenedSession, Prepared},
    ui::Ui,
};
use ratatui::{backend::TestBackend, crossterm::event::KeyCode};
use tokio_util::sync::CancellationToken;

/// The project's sessions, saved in `dir`.
struct Sessions {
    dir: PathBuf,
    workspace: PathBuf,
    /// Opening a session waits for this (or for the user to stop it), as a slow disk or a slow
    /// `git` would make it.
    gate: Option<Arc<tokio::sync::Notify>>,
}

impl Host for Sessions {
    fn is_command(&self, _name: &str) -> bool {
        false
    }
    fn prepare(&mut self, _typed: &str) -> Prepared {
        unreachable!()
    }
    fn sessions(&self) -> Vec<SessionSummary> {
        session::list(&self.dir)
    }
    fn open_session(
        &self,
        id: Option<&str>,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<OpenedSession, String>> {
        let (dir, workspace, gate) = (self.dir.clone(), self.workspace.clone(), self.gate.clone());
        let id = id.map(str::to_string);
        Box::pin(async move {
            if let Some(gate) = gate {
                tokio::select! {
                    () = gate.notified() => {}
                    () = cancel.cancelled() => return Err("stopped".to_string()),
                }
            }
            let session = match id {
                None => Session::create(&dir, &workspace),
                Some(id) => {
                    let path = dir.join(format!("{id}.jsonl"));
                    if !path.exists() {
                        return Err(format!("there is no session {id} in this project"));
                    }
                    Session::open(&path).map_err(|e| e.to_string())?.0
                }
            };
            Ok(OpenedSession {
                session,
                checkpoints: None,
                warnings: vec![
                    "a warning about the file".into(),
                    "note: a note about the file".into(),
                ],
            })
        })
    }
}

/// A saved session in `dir` whose conversation is `exchanges` of question and answer.
fn saved(dir: &Path, workspace: &Path, exchanges: &[(&str, &str)]) -> String {
    let mut session = Session::create(dir, workspace);
    for (question, answer) in exchanges {
        for message in [
            Message::User {
                content: question.to_string(),
            },
            Message::Assistant {
                content: answer.to_string(),
                tool_calls: Vec::new(),
                model: "mock/old".into(),
            },
        ] {
            session.append(EntryKind::Message {
                message,
                display: None,
                note: false,
                plan: None,
            });
        }
    }
    session.id().to_string()
}

fn open(provider: Arc<MockProvider>, sessions: &Path, workspace: &Path) -> (Ui<TestBackend>, Log) {
    open_with(provider, sessions, workspace, None)
}

fn open_with(
    provider: Arc<MockProvider>,
    sessions: &Path,
    workspace: &Path,
    gate: Option<Arc<tokio::sync::Notify>>,
) -> (Ui<TestBackend>, Log) {
    let agent =
        agent(provider, workspace, Mode::Auto).with_session(Session::create(sessions, workspace));
    let host = Sessions {
        dir: sessions.to_path_buf(),
        workspace: workspace.to_path_buf(),
        gate,
    };
    start(agent, Box::new(host), options(workspace, Mode::Auto))
}

fn user_messages(provider: &MockProvider) -> Vec<String> {
    provider
        .requests()
        .last()
        .map(|r| {
            r.messages
                .iter()
                .filter_map(|m| match m {
                    Message::User { content } => Some(content.clone()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn new_starts_an_empty_conversation_in_a_new_session() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let provider = MockProvider::new(vec![Script::text("Hi."), Script::text("Fresh.")]);
    let (mut ui, _log) = open(provider.clone(), &sessions, dir.path());
    send(&mut ui, "hello");
    settle(&mut ui).await;
    send(&mut ui, "/new");
    settle(&mut ui).await;
    assert!(shows(
        &ui,
        "started a new session; /resume goes back to another"
    ));
    assert!(shows(&ui, "warning: a warning about the file"));
    // Review B M5: a note is shown as a note, not as a warning that starts "note:".
    assert!(shows(&ui, "a note about the file"));
    assert!(!shows(&ui, "warning: note:"), "{:#?}", everything(&ui));
    send(&mut ui, "a fresh start");
    settle(&mut ui).await;
    assert_eq!(user_messages(&provider), ["a fresh start"]);
    let listed = session::list(&sessions);
    assert_eq!(listed.len(), 2);
    assert!(
        listed
            .iter()
            .any(|s| s.first_message.as_deref() == Some("hello"))
    );
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn resume_opens_a_picker_of_the_other_sessions_and_continues_the_chosen_one() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let old = saved(
        &sessions,
        dir.path(),
        &[
            ("old question", "old answer"),
            ("second old question", "second old answer"),
        ],
    );
    let provider = MockProvider::new(vec![Script::text("Now."), Script::text("Resumed.")]);
    let (mut ui, log) = open(provider.clone(), &sessions, dir.path());
    send(&mut ui, "the current session");
    settle(&mut ui).await;
    send(&mut ui, "/resume");
    let shown = screen(&ui);
    assert_eq!(shown[0], "Resume which session?");
    let row = shown
        .iter()
        .find(|r| r.contains("old question"))
        .unwrap_or_else(|| panic!("{shown:#?}"));
    assert!(row.contains(&old), "{row}");
    // The session in use is not offered.
    assert!(
        !shown.iter().any(|r| r.contains("the current session")),
        "{shown:#?}"
    );
    until_armed(&ui).await;
    press(&mut ui, KeyCode::Enter);
    settle(&mut ui).await;
    assert_eq!(*log.lock().unwrap(), ["enter", "leave"]);
    assert!(shows(&ui, &format!("resumed session {old}")));
    // What it ended with is shown again.
    assert!(shows(&ui, "› second old question"));
    assert!(shows(&ui, "second old answer"));
    // Up recalls its messages.
    press(&mut ui, KeyCode::Up);
    assert_eq!(ui.app().editor().text(), "second old question");
    ctrl(&mut ui, 'u');
    send(&mut ui, "go on");
    settle(&mut ui).await;
    let sent = user_messages(&provider);
    assert_eq!(sent[0], "old question");
    // The model is told the mode, which the resumed conversation may not say.
    let last = sent.last().unwrap();
    assert!(
        last.contains("[harness] The approval mode is now auto") && last.contains("go on"),
        "{sent:?}"
    );
    assert!(!sent.iter().any(|m| m.contains("the current session")));
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn resume_with_an_id_continues_that_session() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let old = saved(&sessions, dir.path(), &[("old question", "old answer")]);
    let provider = MockProvider::new(vec![Script::text("Resumed.")]);
    let (mut ui, log) = open(provider.clone(), &sessions, dir.path());
    send(&mut ui, &format!("/resume {old}"));
    settle(&mut ui).await;
    assert!(log.lock().unwrap().is_empty());
    assert!(shows(&ui, &format!("resumed session {old}")));
    send(&mut ui, "go on");
    settle(&mut ui).await;
    assert_eq!(user_messages(&provider)[0], "old question");
    send(&mut ui, "/resume nope");
    settle(&mut ui).await;
    assert!(shows(
        &ui,
        "error: could not resume the session: there is no session nope in this project"
    ));
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn resume_without_another_session_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let (mut ui, log) = open(MockProvider::new(Vec::new()), &sessions, dir.path());
    send(&mut ui, "/resume");
    assert!(ui.app().picker().is_none());
    assert!(shows(&ui, "there is no other session in this project yet"));
    assert!(log.lock().unwrap().is_empty());
    ui.finish().await.unwrap();
}

// Review Focus: the chosen session is open in another harness process. Resuming it is refused,
// and the session in use goes on as it was. The error names the session's path, so where the
// screen wraps it depends on how long that is (on the CI runners it fell inside the sentence):
// the message is found whatever the width of the path.
#[tokio::test]
async fn a_session_another_process_holds_is_not_resumed() {
    for pad in (0..60).step_by(5) {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("d".repeat(pad + 1));
        std::fs::create_dir_all(&dir).unwrap();
        let sessions = dir.join("sessions");
        let old = saved(&sessions, &dir, &[("old question", "old answer")]);
        // Another process has it open: the lock is held.
        let (_held, _) = Session::open(&sessions.join(format!("{old}.jsonl"))).unwrap();
        let provider = MockProvider::new(vec![Script::text("One."), Script::text("Two.")]);
        let (mut ui, _log) = open(provider.clone(), &sessions, &dir);
        send(&mut ui, "the current session");
        settle(&mut ui).await;
        send(&mut ui, "/resume");
        until_armed(&ui).await;
        press(&mut ui, KeyCode::Enter);
        settle(&mut ui).await;
        assert!(
            shows_wrapped(&ui, "error: could not resume the session:")
                && shows_wrapped(&ui, "is open in another harness process"),
            "{pad}: {:#?}",
            everything(&ui)
        );
        assert!(!ui.app().busy());
        send(&mut ui, "still here");
        settle(&mut ui).await;
        assert_eq!(
            user_messages(&provider),
            ["the current session", "still here"]
        );
        ui.finish().await.unwrap();
    }
}

// The session picker takes keys once the user has paused, as an approval does: an Enter typed
// ahead cannot resume a session.
#[tokio::test]
async fn keys_typed_ahead_of_the_session_picker_do_not_resume() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    saved(&sessions, dir.path(), &[("old question", "old answer")]);
    let (mut ui, log) = open(MockProvider::new(Vec::new()), &sessions, dir.path());
    send(&mut ui, "/resume");
    assert!(ui.app().picker().is_some());
    press(&mut ui, KeyCode::Enter);
    press(&mut ui, KeyCode::Enter);
    assert!(ui.app().picker().is_some());
    assert!(!shows(&ui, "resumed session"));
    until_armed(&ui).await;
    press(&mut ui, KeyCode::Esc);
    assert!(ui.app().picker().is_none());
    assert_eq!(*log.lock().unwrap(), ["enter", "leave"]);
    ui.finish().await.unwrap();
}

// `harness --resume` alone: the session starts with the session picker open.
#[tokio::test]
async fn the_session_picker_can_open_as_the_session_starts() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let old = saved(&sessions, dir.path(), &[("old question", "old answer")]);
    let provider = MockProvider::new(vec![Script::text("Resumed.")]);
    let (mut ui, log) = open(provider.clone(), &sessions, dir.path());
    ui.open_session_picker().unwrap();
    assert_eq!(screen(&ui)[0], "Resume which session?");
    until_armed(&ui).await;
    press(&mut ui, KeyCode::Enter);
    settle(&mut ui).await;
    assert_eq!(*log.lock().unwrap(), ["enter", "leave"]);
    assert!(shows(&ui, &format!("resumed session {old}")));
    send(&mut ui, "go on");
    settle(&mut ui).await;
    assert_eq!(user_messages(&provider)[0], "old question");
    ui.finish().await.unwrap();
}

// Review B M4: opening a session (reading its file, opening its checkpoints) is not done on the
// UI's loop: the session shows it is loading, and Esc stops it.
#[tokio::test]
async fn a_session_that_is_slow_to_open_shows_loading_and_esc_stops_it() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let old = saved(&sessions, dir.path(), &[("old question", "old answer")]);
    let gate = Arc::new(tokio::sync::Notify::new());
    let provider = MockProvider::new(vec![Script::text("One."), Script::text("Two.")]);
    let (mut ui, _log) = open_with(provider.clone(), &sessions, dir.path(), Some(gate));
    send(&mut ui, "the current session");
    settle(&mut ui).await;
    send(&mut ui, &format!("/resume {old}"));
    assert!(ui.app().busy());
    assert!(
        screen(&ui)
            .iter()
            .any(|r| r.contains("loading the session… (Esc to stop)")),
        "{:#?}",
        screen(&ui)
    );
    press(&mut ui, KeyCode::Esc);
    settle(&mut ui).await;
    assert!(
        shows_wrapped(&ui, "error: could not resume the session: stopped"),
        "{:#?}",
        everything(&ui)
    );
    assert!(!ui.app().busy());
    // The session in use goes on.
    send(&mut ui, "still here");
    settle(&mut ui).await;
    assert_eq!(
        user_messages(&provider),
        ["the current session", "still here"]
    );
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn a_session_that_opens_later_is_continued_when_it_does() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let old = saved(&sessions, dir.path(), &[("old question", "old answer")]);
    let gate = Arc::new(tokio::sync::Notify::new());
    let provider = MockProvider::new(vec![Script::text("Resumed.")]);
    let (mut ui, _log) = open_with(provider.clone(), &sessions, dir.path(), Some(gate.clone()));
    send(&mut ui, &format!("/resume {old}"));
    assert!(ui.app().busy());
    gate.notify_one();
    settle(&mut ui).await;
    assert!(shows(&ui, "resumed session"));
    assert!(shows(&ui, "a note about the file"));
    send(&mut ui, "carry on");
    settle(&mut ui).await;
    // The first message after the switch carries the mode note.
    let sent = user_messages(&provider);
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert_eq!(sent[0], "old question");
    assert!(sent[1].ends_with("carry on"), "{sent:?}");
    ui.finish().await.unwrap();
}
