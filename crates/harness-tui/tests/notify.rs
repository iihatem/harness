//! Notifications: the bytes written to the terminal, and when they are sent.

use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use harness_core::{
    agent::{Agent, AgentConfig},
    engine::{EngineConfig, PermissionEngine, RuleSet},
    event::{AgentEvent, TurnEndReason},
    permission::Mode,
    testing::{MockProvider, Script},
    tool::{Tool, ToolContext, ToolRegistry},
};
use harness_tui::{
    app::{App, Host, Options, Prepared},
    approval::ChannelApprover,
    inline::InlineTerminal,
    notify::{self, Notify, TerminalNotifier},
    style::Theme,
    ui::Ui,
};
use ratatui::{
    backend::TestBackend,
    crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers},
};
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

/// Keeps what it was asked to send.
#[derive(Clone, Default)]
struct Recording(Arc<Mutex<Vec<String>>>);

impl Notify for Recording {
    fn notify(&mut self, text: &str) -> std::io::Result<()> {
        self.0.lock().unwrap().push(text.to_string());
        Ok(())
    }
}

fn options(dir: &Path, notifier: Option<Box<dyn Notify>>) -> Options {
    Options {
        theme: Theme::monochrome(),
        model: "mock/m".into(),
        mode: Mode::Ask,
        commands: Vec::new(),
        workspace: dir.to_path_buf(),
        history: Vec::new(),
        instruction_files: Vec::new(),
        window_note: None,
        default_mode: Mode::Auto,
        text_editor: None,
        notifier,
    }
}

fn sent(notifier: bool, bell: bool, text: &str) -> String {
    let mut out = TerminalNotifier::new(Vec::new(), notifier, bell);
    out.notify(text).unwrap();
    String::from_utf8(out.out().clone()).unwrap()
}

#[test]
fn a_notification_is_an_osc_9_sequence_and_a_bell_each_when_enabled() {
    assert_eq!(
        sent(true, true, "the turn finished after 12s"),
        "\x1b]9;harness: the turn finished after 12s\x07\x07"
    );
    assert_eq!(sent(true, false, "x"), "\x1b]9;harness: x\x07");
    assert_eq!(sent(false, true, "x"), "\x07");
    assert_eq!(sent(false, false, "x"), "");
    // Text from the model cannot end the sequence early or start another.
    assert_eq!(
        sent(true, false, "run `x\x1b]0;evil\x07`\n"),
        "\x1b]9;harness: run `x]0;evil`\x07"
    );
    let long = "y".repeat(500);
    assert_eq!(
        sent(true, false, &long).len(),
        "\x1b]9;harness: \x07".len() + 200
    );
}

#[test]
fn durations_read_as_people_say_them() {
    assert_eq!(notify::duration(Duration::from_secs(12)), "12s");
    assert_eq!(notify::duration(Duration::from_secs(182)), "3m 2s");
    assert_eq!(notify::duration(Duration::from_secs(3_900)), "1h 5m");
}

#[test]
fn a_turn_that_ran_long_notifies_when_it_ends_unless_interrupted() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = App::new(options(dir.path(), None), Box::new(NoCommands), 80);
    let start = Instant::now();
    let turn = |app: &mut App, took: u64, reason: TurnEndReason| {
        app.on_event_at(&AgentEvent::TurnStarted, start);
        app.on_event_at(
            &AgentEvent::TurnFinished { reason },
            start + Duration::from_secs(took),
        );
        app.take_notifications()
    };
    assert_eq!(
        turn(&mut app, 180, TurnEndReason::Completed),
        ["the turn finished after 3m 0s"]
    );
    assert!(turn(&mut app, 9, TurnEndReason::Completed).is_empty());
    assert!(turn(&mut app, 60, TurnEndReason::Interrupted).is_empty());
    assert_eq!(
        turn(&mut app, 11, TurnEndReason::Error),
        ["the turn stopped with an error after 11s"]
    );
}

#[tokio::test]
async fn an_approval_notifies() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call("b1", "bash", json!({"command": "echo hi"})),
        Script::text("done"),
    ]);
    let policy = Arc::new(PermissionEngine::new(EngineConfig {
        mode: Mode::Ask,
        workspace: dir.path().to_path_buf(),
        read_dirs: Vec::new(),
        rules: RuleSet::default(),
        sandbox_available: true,
        writes_need_approval: false,
    }));
    let bash: Arc<dyn Tool> = harness_tools::builtin().get("bash").unwrap();
    let (approver, approvals) = ChannelApprover::new();
    let agent = Agent::new(
        provider,
        ToolRegistry::new(vec![bash]),
        policy,
        approver,
        AgentConfig::new("mock/m", "m", "system", dir.path().join("out")),
        ToolContext::new(dir.path()),
    );
    let recording = Recording::default();
    let term = InlineTerminal::new(TestBackend::new(80, 20), 0).unwrap();
    let mut ui = Ui::start(
        agent,
        Box::new(NoCommands),
        term,
        options(dir.path(), Some(Box::new(recording.clone()))),
        approvals,
    );
    for c in "go".chars() {
        ui.handle(Event::Key(KeyEvent::new(
            KeyCode::Char(c),
            KeyModifiers::NONE,
        )))
        .unwrap();
    }
    ui.handle(Event::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )))
    .unwrap();
    tokio::time::timeout(
        Duration::from_secs(10),
        ui.until(|app| app.prompt().is_some()),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        *recording.0.lock().unwrap(),
        ["approval needed: run `echo hi`"]
    );
    ui.handle(Event::Key(KeyEvent::new(
        KeyCode::Char('y'),
        KeyModifiers::NONE,
    )))
    .unwrap();
    tokio::time::timeout(Duration::from_secs(10), ui.settle())
        .await
        .unwrap()
        .unwrap();
    // A short turn does not notify when it ends.
    assert_eq!(recording.0.lock().unwrap().len(), 1);
    ui.finish().await.unwrap();
}
