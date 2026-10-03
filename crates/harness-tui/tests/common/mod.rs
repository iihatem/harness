//! What the tests of the commands and pickers share: an agent on the mock provider, the session
//! on ratatui's `TestBackend` with the terminal's alternate screen emulated, and scripted keys.

#![allow(dead_code)]

use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use harness_core::{
    agent::{Agent, AgentConfig, NonInteractive},
    engine::{EngineConfig, PermissionEngine, RuleSet},
    permission::Mode,
    provider::Provider,
    tool::{Tool, ToolContext, ToolRegistry},
};
use harness_tui::{
    app::{App, Host, Options},
    approval::ChannelApprover,
    inline::InlineTerminal,
    style::Theme,
    testing::TestAltScreen,
    ui::Ui,
};
use ratatui::{
    backend::TestBackend,
    buffer::Buffer,
    crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers},
};

/// What the emulated alternate screen was asked to do: `"enter"` and `"leave"`.
pub type Log = Arc<Mutex<Vec<&'static str>>>;

/// An agent on `provider` in `dir`, whose `write` and `bash` run without approval in `mode`, as if
/// sandboxed.
pub fn agent(provider: Arc<dyn Provider>, dir: &Path, mode: Mode) -> Agent {
    let policy = Arc::new(PermissionEngine::new(EngineConfig {
        mode,
        workspace: dir.to_path_buf(),
        read_dirs: Vec::new(),
        rules: RuleSet::default(),
        sandbox_available: true,
        writes_need_approval: false,
    }));
    let tools: Vec<Arc<dyn Tool>> = ["read", "write", "bash"]
        .into_iter()
        .map(|name| harness_tools::builtin().get(name).unwrap())
        .collect();
    Agent::new(
        provider,
        ToolRegistry::new(tools),
        policy,
        Arc::new(NonInteractive),
        AgentConfig::new("mock/m", "m", "system", dir.join("out")),
        ToolContext::new(dir),
    )
}

pub fn options(dir: &Path, mode: Mode) -> Options {
    Options {
        theme: Theme::monochrome(),
        model: "mock/m".into(),
        mode,
        commands: Vec::new(),
        workspace: dir.to_path_buf(),
        history: Vec::new(),
        instruction_files: Vec::new(),
        window_note: None,
        default_mode: Mode::Auto,
        text_editor: None,
        notifier: None,
    }
}

/// The session for `agent` on a 80 by 24 terminal, and the alternate screen's log.
pub fn start(agent: Agent, host: Box<dyn Host>, options: Options) -> (Ui<TestBackend>, Log) {
    let (alt, log) = TestAltScreen::new();
    let term = InlineTerminal::new(TestBackend::new(80, 24), 0)
        .unwrap()
        .with_alt_screen(Box::new(alt));
    let mut ui = Ui::start(agent, host, term, options, ChannelApprover::new().1);
    ui.draw().unwrap();
    (ui, log)
}

pub fn rows(buffer: &Buffer) -> Vec<String> {
    let width = buffer.area.width as usize;
    buffer
        .content
        .chunks(width.max(1))
        .map(|row| {
            let text: String = row.iter().map(|cell| cell.symbol()).collect();
            text.trim_end().to_string()
        })
        .collect()
}

/// What the screen shows now.
pub fn screen(ui: &Ui<TestBackend>) -> Vec<String> {
    rows(ui.terminal().backend().buffer())
}

/// The scrollback, then the screen.
pub fn everything(ui: &Ui<TestBackend>) -> Vec<String> {
    let backend = ui.terminal().backend();
    let mut out = rows(backend.scrollback());
    out.extend(rows(backend.buffer()));
    out
}

pub fn shows(ui: &Ui<TestBackend>, text: &str) -> bool {
    everything(ui).iter().any(|row| row.contains(text))
}

/// Whether the screen and scrollback show `text`, whatever way they wrapped it: the rows are
/// joined and runs of whitespace collapsed (a wrap at a space leaves none, or one).
pub fn shows_wrapped(ui: &Ui<TestBackend>, text: &str) -> bool {
    let joined = everything(ui).join(" ");
    let flat = joined.split_whitespace().collect::<Vec<_>>().join(" ");
    flat.contains(text)
}

pub fn press(ui: &mut Ui<TestBackend>, code: KeyCode) {
    ui.handle(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
        .unwrap();
}

pub fn ctrl(ui: &mut Ui<TestBackend>, c: char) {
    ui.handle(Event::Key(KeyEvent::new(
        KeyCode::Char(c),
        KeyModifiers::CONTROL,
    )))
    .unwrap();
}

pub fn type_text(ui: &mut Ui<TestBackend>, text: &str) {
    for c in text.chars() {
        press(ui, KeyCode::Char(c));
    }
}

pub fn send(ui: &mut Ui<TestBackend>, text: &str) {
    type_text(ui, text);
    press(ui, KeyCode::Enter);
}

/// Waits until what the user is asked (an approval, a plan choice, a picker) takes keys: pickers
/// arm as prompts do, so keys typed ahead never choose an item.
pub async fn until_armed(ui: &Ui<TestBackend>) {
    let armed = ui.app().armed_at().expect("what asks was drawn");
    tokio::time::sleep_until(tokio::time::Instant::from_std(armed)).await;
}

/// Takes in the agent's events until nothing runs and the agent has said where things stand.
pub async fn settle(ui: &mut Ui<TestBackend>) {
    tokio::time::timeout(Duration::from_secs(10), ui.settle())
        .await
        .expect("the work ends")
        .unwrap();
}

/// Takes in the agent's events until `done` holds.
pub async fn until(ui: &mut Ui<TestBackend>, done: impl Fn(&App) -> bool) {
    tokio::time::timeout(Duration::from_secs(10), ui.until(done))
        .await
        .expect("it happens")
        .unwrap();
}
