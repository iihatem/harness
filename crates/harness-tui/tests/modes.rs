//! Shift+Tab cycles the approval mode through plan, ask and auto.

use std::{path::Path, sync::Arc, time::Duration};

use harness_core::{
    agent::{Agent, AgentConfig, NonInteractive},
    engine::{EngineConfig, PermissionEngine, RuleSet},
    message::Message,
    permission::Mode,
    provider::ProviderEvent,
    testing::{MockProvider, Script},
    tool::{ToolContext, ToolRegistry},
};
use harness_tui::{
    app::{Host, Options, Prepared, next_mode},
    approval::ChannelApprover,
    inline::InlineTerminal,
    style::Theme,
    ui::Ui,
};
use ratatui::{
    backend::TestBackend,
    buffer::Buffer,
    crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers},
};

struct NoCommands;

impl Host for NoCommands {
    fn is_command(&self, _name: &str) -> bool {
        false
    }
    fn prepare(&mut self, _typed: &str) -> Prepared {
        unreachable!()
    }
}

fn start(provider: Arc<MockProvider>, dir: &Path, mode: Mode) -> Ui<TestBackend> {
    let policy = Arc::new(PermissionEngine::new(EngineConfig {
        mode,
        workspace: dir.to_path_buf(),
        read_dirs: Vec::new(),
        rules: RuleSet::default(),
        sandbox_available: false,
        writes_need_approval: false,
    }));
    let agent = Agent::new(
        provider,
        ToolRegistry::new(Vec::new()),
        policy,
        Arc::new(NonInteractive),
        AgentConfig::new("mock/m", "m", "system", dir.join("out")),
        ToolContext::new(dir),
    );
    let options = Options {
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
    };
    let term = InlineTerminal::new(TestBackend::new(100, 20), 0).unwrap();
    let mut ui = Ui::start(
        agent,
        Box::new(NoCommands),
        term,
        options,
        ChannelApprover::new().1,
    );
    ui.draw().unwrap();
    ui
}

fn rows(buffer: &Buffer) -> Vec<String> {
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

fn status(ui: &Ui<TestBackend>) -> String {
    // The status line is the last row that starts with the model.
    rows(ui.terminal().backend().buffer())
        .into_iter()
        .rfind(|r| r.starts_with("mock/m · "))
        .unwrap()
}

fn press(ui: &mut Ui<TestBackend>, code: KeyCode, modifiers: KeyModifiers) {
    ui.handle(Event::Key(KeyEvent::new(code, modifiers)))
        .unwrap();
}

fn shift_tab(ui: &mut Ui<TestBackend>) {
    press(ui, KeyCode::BackTab, KeyModifiers::SHIFT);
}

async fn settle(ui: &mut Ui<TestBackend>) {
    tokio::time::timeout(Duration::from_secs(10), ui.settle())
        .await
        .expect("settles")
        .unwrap();
}

fn notes(provider: &MockProvider) -> Vec<String> {
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

#[test]
fn shift_tab_never_reaches_full_access_or_read_only() {
    assert_eq!(next_mode(Mode::Plan), Mode::Ask);
    assert_eq!(next_mode(Mode::Ask), Mode::Auto);
    assert_eq!(next_mode(Mode::Auto), Mode::Plan);
    assert_eq!(next_mode(Mode::ReadOnly), Mode::Ask);
    assert_eq!(next_mode(Mode::FullAccess), Mode::Plan);
}

#[tokio::test]
async fn shift_tab_switches_the_mode_and_the_agent_is_told() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![Script::text("planning")]);
    let mut ui = start(provider.clone(), dir.path(), Mode::Auto);
    assert!(status(&ui).starts_with("mock/m · auto ·"));
    shift_tab(&mut ui);
    settle(&mut ui).await;
    assert!(
        status(&ui).starts_with("mock/m · plan ·"),
        "{}",
        status(&ui)
    );
    for c in "look around".chars() {
        press(&mut ui, KeyCode::Char(c), KeyModifiers::NONE);
    }
    press(&mut ui, KeyCode::Enter, KeyModifiers::NONE);
    settle(&mut ui).await;
    let sent = notes(&provider).join("\n");
    assert!(
        sent.contains("[harness] The approval mode is now plan"),
        "{sent}"
    );
    assert!(
        rows(ui.terminal().backend().scrollback())
            .iter()
            .chain(rows(ui.terminal().backend().buffer()).iter())
            .any(|r| r == "switched to plan mode")
    );
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn a_mode_chosen_during_a_turn_applies_when_it_ends() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::Hang(vec![ProviderEvent::TextDelta("working".into())]),
        Script::text("next"),
    ]);
    let mut ui = start(provider.clone(), dir.path(), Mode::Auto);
    press(&mut ui, KeyCode::Char('x'), KeyModifiers::NONE);
    press(&mut ui, KeyCode::Enter, KeyModifiers::NONE);
    tokio::time::timeout(
        Duration::from_secs(10),
        ui.until(|app| app.transcript.busy()),
    )
    .await
    .unwrap()
    .unwrap();
    shift_tab(&mut ui);
    shift_tab(&mut ui);
    let now = status(&ui);
    assert!(now.starts_with("mock/m · auto ·"), "{now}");
    assert!(now.ends_with("· ask mode after this turn"), "{now}");
    // Back to where it started: nothing to switch.
    shift_tab(&mut ui);
    assert!(!status(&ui).contains("after this turn"));
    shift_tab(&mut ui);
    press(&mut ui, KeyCode::Esc, KeyModifiers::NONE);
    settle(&mut ui).await;
    assert!(
        status(&ui).starts_with("mock/m · plan ·"),
        "{}",
        status(&ui)
    );
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn leaving_full_access_goes_to_plan_and_drops_its_warning() {
    let dir = tempfile::tempdir().unwrap();
    let mut ui = start(MockProvider::new(Vec::new()), dir.path(), Mode::FullAccess);
    assert!(status(&ui).ends_with("full-access: no sandbox, no approvals"));
    shift_tab(&mut ui);
    settle(&mut ui).await;
    let now = status(&ui);
    assert!(now.starts_with("mock/m · plan ·"), "{now}");
    assert!(!now.contains("full-access"), "{now}");
    ui.finish().await.unwrap();
}
