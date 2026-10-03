//! Ruling P3, end to end: the first edit of a file with a server, in an untrusted workspace, asks
//! "Start language servers here?" in the terminal UI (scripted keys on a `TestBackend`) and then
//! starts the fake server, or does not; the answer is remembered once and the question is not
//! asked again.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use harness_core::{
    agent::{Agent, AgentConfig},
    engine::{EngineConfig, PermissionEngine, RuleSet},
    message::Message,
    permission::Mode,
    testing::{MockProvider, Script},
    tool::ToolContext,
};
use harness_lsp::{AskThroughApprover, LspDiagnostics, Manager, SERVERS_QUESTION, Settings};
use harness_tui::{
    app::{Host, Options, Prepared},
    approval::{ARMING_DELAY, ChannelApprover},
    inline::InlineTerminal,
    style::Theme,
    ui::Ui,
};
use ratatui::{
    backend::TestBackend,
    crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers},
};
use serde_json::json;

const SERVER: &str = env!("CARGO_BIN_EXE_fake-lsp-server");

struct NoCommands;

impl Host for NoCommands {
    fn is_command(&self, _name: &str) -> bool {
        false
    }
    fn prepare(&mut self, _typed: &str) -> Prepared {
        unreachable!()
    }
}

struct Session {
    ui: Ui<TestBackend>,
    provider: Arc<MockProvider>,
    remembered: Arc<Mutex<Vec<bool>>>,
    _dir: tempfile::TempDir,
}

fn session(edits: usize) -> Session {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let ws = base.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(base.join("bin")).unwrap();
    std::os::unix::fs::symlink(SERVER, base.join("bin/gopls")).unwrap();
    std::fs::write(ws.join("main.go"), "package main\n").unwrap();
    let mut scripts = vec![Script::tool_call("r1", "read", json!({"path": "main.go"}))];
    for i in 0..edits {
        let (from, to) = if i == 0 {
            ("package main", "package main // ERROR")
        } else {
            ("// ERROR", "// ERROR again")
        };
        scripts.push(Script::tool_call(
            &format!("e{i}"),
            "edit",
            json!({"path": "main.go", "old_string": from, "new_string": to}),
        ));
    }
    scripts.push(Script::text("Done."));
    let provider = MockProvider::new(scripts);
    let policy = Arc::new(PermissionEngine::new(EngineConfig {
        mode: Mode::FullAccess,
        workspace: ws.clone(),
        read_dirs: Vec::new(),
        rules: RuleSet::default(),
        sandbox_available: true,
        writes_need_approval: false,
    }));
    let (approver, approvals) = ChannelApprover::new();
    let remembered = Arc::new(Mutex::new(Vec::new()));
    let log = remembered.clone();
    let manager = Manager::new(
        Settings {
            enabled: true,
            wait: Duration::from_secs(5),
            first_wait: Duration::from_secs(10),
            servers: BTreeMap::new(),
            trusted: false,
            allowed: None,
            path: vec![base.join("bin")],
            init_timeout: Duration::from_secs(10),
        },
        ws.clone(),
    )
    .with_consent(Arc::new(AskThroughApprover::new(
        approver.clone(),
        ws.clone(),
        move |yes| log.lock().unwrap().push(yes),
    )));
    let agent = Agent::new(
        provider.clone(),
        harness_tools::builtin(),
        policy,
        approver,
        AgentConfig::new("mock/m", "m", "system", base.join("out")),
        ToolContext::new(&ws),
    )
    .with_diagnostics(Arc::new(LspDiagnostics::new(manager, ws.clone())));
    let options = Options {
        theme: Theme::monochrome(),
        model: "mock/m".into(),
        mode: Mode::FullAccess,
        commands: Vec::new(),
        workspace: ws,
        history: Vec::new(),
        instruction_files: Vec::new(),
        window_note: None,
        default_mode: Mode::FullAccess,
        text_editor: None,
        notifier: None,
    };
    let term = InlineTerminal::new(TestBackend::new(100, 24), 0).unwrap();
    let mut ui = Ui::start(agent, Box::new(NoCommands), term, options, approvals);
    ui.draw().unwrap();
    Session {
        ui,
        provider,
        remembered,
        _dir: dir,
    }
}

fn press(ui: &mut Ui<TestBackend>, code: KeyCode) {
    ui.handle(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
        .unwrap();
}

fn screen(ui: &Ui<TestBackend>) -> String {
    let buffer = ui.terminal().backend().buffer();
    let width = buffer.area.width as usize;
    buffer
        .content
        .chunks(width)
        .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

async fn until_asked(ui: &mut Ui<TestBackend>) {
    tokio::time::timeout(
        Duration::from_secs(10),
        ui.until(|app| app.prompt().is_some()),
    )
    .await
    .expect("the question is asked")
    .unwrap();
    tokio::time::sleep(ARMING_DELAY).await;
}

async fn settle(ui: &mut Ui<TestBackend>) {
    tokio::time::timeout(Duration::from_secs(20), ui.settle())
        .await
        .expect("the turn ends")
        .unwrap();
}

fn edit_results(provider: &MockProvider) -> Vec<String> {
    provider
        .requests()
        .last()
        .unwrap()
        .messages
        .iter()
        .filter_map(|m| match m {
            Message::Tool {
                call_id, content, ..
            } if call_id.starts_with('e') => Some(content.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn yes_starts_the_server_and_the_second_edit_does_not_ask_again() {
    let mut s = session(2);
    for c in "go".chars() {
        press(&mut s.ui, KeyCode::Char(c));
    }
    press(&mut s.ui, KeyCode::Enter);
    until_asked(&mut s.ui).await;
    assert!(
        screen(&s.ui).contains(SERVERS_QUESTION),
        "{}",
        screen(&s.ui)
    );
    press(&mut s.ui, KeyCode::Char('y'));
    settle(&mut s.ui).await;
    let results = edit_results(&s.provider);
    assert!(results[0].contains("[diagnostics: 1 error"), "{results:?}");
    assert!(results[1].contains("[diagnostics: 1 error"), "{results:?}");
    assert_eq!(*s.remembered.lock().unwrap(), [true]);
    s.ui.finish().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn enter_says_no_and_no_server_runs() {
    let mut s = session(2);
    for c in "go".chars() {
        press(&mut s.ui, KeyCode::Char(c));
    }
    press(&mut s.ui, KeyCode::Enter);
    until_asked(&mut s.ui).await;
    press(&mut s.ui, KeyCode::Enter);
    settle(&mut s.ui).await;
    // No diagnostics, and one note, once, that they are off and what turns them on.
    let results = edit_results(&s.provider);
    assert!(results[0].contains("harness trust"), "{results:?}");
    assert!(!results[0].contains("[diagnostics:"), "{results:?}");
    for result in &results[1..] {
        assert!(!result.contains("diagnostics"), "{result}");
    }
    assert_eq!(*s.remembered.lock().unwrap(), [false]);
    s.ui.finish().await.unwrap();
}
