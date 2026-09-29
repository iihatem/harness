//! Input typed while a turn runs: Enter queues it for when the turn ends, Ctrl+S sends it with
//! the next tool results.

use std::{path::Path, sync::Arc, time::Duration};

use async_trait::async_trait;
use futures::StreamExt;
use harness_core::{
    agent::{Agent, AgentConfig, NonInteractive},
    engine::{EngineConfig, PermissionEngine, RuleSet},
    message::{ChatRequest, Message, ToolSpec},
    permission::{Action, Mode},
    provider::{Provider, ProviderStream},
    testing::{MockProvider, Script},
    tool::{Tool, ToolContext, ToolOutput, ToolRegistry},
};
use harness_tui::{
    app::{Host, Options, Prepared},
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
use serde_json::{Value, json};
use tokio::sync::Notify;

struct NoCommands;

impl Host for NoCommands {
    fn is_command(&self, _name: &str) -> bool {
        false
    }
    fn prepare(&mut self, _typed: &str) -> Prepared {
        unreachable!()
    }
}

/// Runs "the tests" until the test lets it finish, or the turn is interrupted.
struct Tests(Arc<Notify>);

#[async_trait]
impl Tool for Tests {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "run_tests".into(),
            description: "runs the tests".into(),
            parameters: json!({"type": "object"}),
        }
    }
    fn action(&self, _args: &Value, ctx: &ToolContext) -> Action {
        Action::Read(ctx.workspace.clone())
    }
    async fn run(&self, _args: Value, ctx: &ToolContext) -> ToolOutput {
        tokio::select! {
            _ = self.0.notified() => ToolOutput::ok("3 tests failed"),
            _ = ctx.cancel.cancelled() => ToolOutput::error("interrupted"),
        }
    }
}

/// Answers as `inner` does, the first request only once the test opens `gate`.
struct Gated {
    inner: Arc<MockProvider>,
    gate: Arc<Notify>,
    first: std::sync::atomic::AtomicBool,
}

impl Provider for Gated {
    fn stream(&self, request: ChatRequest) -> ProviderStream {
        let inner = self.inner.stream(request);
        if !self.first.swap(false, std::sync::atomic::Ordering::SeqCst) {
            return inner;
        }
        let gate = self.gate.clone();
        Box::pin(
            futures::stream::once(async move {
                gate.notified().await;
                inner
            })
            .flatten(),
        )
    }
}

fn start(provider: Arc<MockProvider>, dir: &Path, gate: &Arc<Notify>) -> Ui<TestBackend> {
    start_with(provider, dir, gate)
}

fn start_with(provider: Arc<dyn Provider>, dir: &Path, gate: &Arc<Notify>) -> Ui<TestBackend> {
    let policy = Arc::new(PermissionEngine::new(EngineConfig {
        mode: Mode::Auto,
        workspace: dir.to_path_buf(),
        read_dirs: Vec::new(),
        rules: RuleSet::default(),
        sandbox_available: false,
        writes_need_approval: false,
    }));
    let agent = Agent::new(
        provider,
        ToolRegistry::new(vec![Arc::new(Tests(gate.clone()))]),
        policy,
        Arc::new(NonInteractive),
        AgentConfig::new("mock/m", "m", "system", dir.join("out")),
        ToolContext::new(dir),
    );
    let options = Options {
        theme: Theme::monochrome(),
        model: "mock/m".into(),
        mode: Mode::Auto,
        commands: Vec::new(),
        workspace: dir.to_path_buf(),
        history: Vec::new(),
        instruction_files: Vec::new(),
        window_note: None,
        default_mode: Mode::Auto,
        text_editor: None,
        notifier: None,
    };
    let term = InlineTerminal::new(TestBackend::new(80, 24), 0).unwrap();
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

fn everything(ui: &Ui<TestBackend>) -> Vec<String> {
    let backend = ui.terminal().backend();
    let mut out = rows(backend.scrollback());
    out.extend(rows(backend.buffer()));
    out
}

fn position(ui: &Ui<TestBackend>, row: &str) -> usize {
    everything(ui)
        .iter()
        .position(|r| r == row)
        .unwrap_or_else(|| panic!("no {row:?} in {:#?}", everything(ui)))
}

fn type_text(ui: &mut Ui<TestBackend>, text: &str) {
    for c in text.chars() {
        ui.handle(Event::Key(KeyEvent::new(
            KeyCode::Char(c),
            KeyModifiers::NONE,
        )))
        .unwrap();
    }
}

fn enter(ui: &mut Ui<TestBackend>) {
    ui.handle(Event::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )))
    .unwrap();
}

fn ctrl_s(ui: &mut Ui<TestBackend>) {
    ui.handle(Event::Key(KeyEvent::new(
        KeyCode::Char('s'),
        KeyModifiers::CONTROL,
    )))
    .unwrap();
}

async fn while_the_tests_run(ui: &mut Ui<TestBackend>) {
    tokio::time::timeout(
        Duration::from_secs(10),
        ui.until(|app| app.transcript.arguments("t1").is_some()),
    )
    .await
    .expect("the tool starts")
    .unwrap();
}

async fn settle(ui: &mut Ui<TestBackend>) {
    tokio::time::timeout(Duration::from_secs(10), ui.settle())
        .await
        .expect("the turns end")
        .unwrap();
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

#[tokio::test]
async fn ctrl_s_sends_with_the_next_tool_result_and_enter_queues_a_new_turn() {
    let dir = tempfile::tempdir().unwrap();
    let gate = Arc::new(Notify::new());
    let provider = MockProvider::new(vec![
        Script::tool_call("t1", "run_tests", json!({})),
        Script::text("Switched to the v2 API."),
        Script::text("README updated."),
    ]);
    let mut ui = start(provider.clone(), dir.path(), &gate);
    type_text(&mut ui, "fix the tests");
    enter(&mut ui);
    while_the_tests_run(&mut ui).await;
    type_text(&mut ui, "use the v2 API instead");
    ctrl_s(&mut ui);
    type_text(&mut ui, "also update the README");
    enter(&mut ui);
    let waiting = rows(ui.terminal().backend().buffer());
    assert!(
        waiting.contains(&"sending with the next tool results: use the v2 API instead".into()),
        "{waiting:#?}"
    );
    assert!(waiting.contains(&"queued: also update the README".into()));
    gate.notify_one();
    settle(&mut ui).await;
    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    // The steering message follows the tool result in the same turn.
    let second = &requests[1].messages;
    assert!(matches!(&second[second.len() - 2], Message::Tool { .. }));
    assert_eq!(
        user_messages(second).last().unwrap(),
        "use the v2 API instead"
    );
    // The queued message is a turn of its own, after the first finished.
    assert_eq!(
        user_messages(&requests[2].messages).last().unwrap(),
        "also update the README"
    );
    assert!(position(&ui, "● run_tests {}") < position(&ui, "› use the v2 API instead"));
    assert!(position(&ui, "› use the v2 API instead") < position(&ui, "Switched to the v2 API."));
    assert!(position(&ui, "Switched to the v2 API.") < position(&ui, "› also update the README"));
    assert!(position(&ui, "› also update the README") < position(&ui, "README updated."));
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn send_now_input_after_the_last_tool_result_starts_the_next_turn() {
    let dir = tempfile::tempdir().unwrap();
    let gate = Arc::new(Notify::new());
    let provider = Arc::new(Gated {
        inner: MockProvider::new(vec![
            Script::text("No tools needed."),
            Script::text("Next turn."),
        ]),
        gate: gate.clone(),
        first: true.into(),
    });
    let mut ui = start_with(provider.clone(), dir.path(), &gate);
    type_text(&mut ui, "hello");
    enter(&mut ui);
    tokio::time::timeout(
        Duration::from_secs(10),
        ui.until(|app| app.transcript.busy()),
    )
    .await
    .unwrap()
    .unwrap();
    // This turn runs no tool, so no tool result will take it.
    type_text(&mut ui, "and then this");
    ctrl_s(&mut ui);
    gate.notify_one();
    settle(&mut ui).await;
    let requests = provider.inner.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        user_messages(&requests[1].messages).last().unwrap(),
        "and then this"
    );
    assert!(position(&ui, "No tools needed.") < position(&ui, "› and then this"));
    assert!(position(&ui, "› and then this") < position(&ui, "Next turn."));
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn interrupting_puts_waiting_input_back_in_the_editor() {
    let dir = tempfile::tempdir().unwrap();
    let gate = Arc::new(Notify::new());
    let provider = MockProvider::new(vec![
        Script::tool_call("t1", "run_tests", json!({})),
        Script::text("Resumed."),
    ]);
    let mut ui = start(provider.clone(), dir.path(), &gate);
    type_text(&mut ui, "go");
    enter(&mut ui);
    while_the_tests_run(&mut ui).await;
    type_text(&mut ui, "send this now");
    ctrl_s(&mut ui);
    type_text(&mut ui, "and this later");
    enter(&mut ui);
    type_text(&mut ui, "still typing");
    ui.handle(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)))
        .unwrap();
    settle(&mut ui).await;
    assert_eq!(
        ui.app().editor().text(),
        "send this now\n\nand this later\n\nstill typing"
    );
    assert_eq!(provider.requests().len(), 1);
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn a_command_cannot_be_sent_during_a_turn_but_can_be_queued() {
    let dir = tempfile::tempdir().unwrap();
    let gate = Arc::new(Notify::new());
    let provider = MockProvider::new(vec![
        Script::tool_call("t1", "run_tests", json!({})),
        Script::text("Done."),
    ]);
    let mut ui = start(provider.clone(), dir.path(), &gate);
    type_text(&mut ui, "go");
    enter(&mut ui);
    while_the_tests_run(&mut ui).await;
    type_text(&mut ui, "/usage");
    ctrl_s(&mut ui);
    assert_eq!(ui.app().editor().text(), "/usage");
    assert!(
        everything(&ui)
            .iter()
            .any(|r| r == "a command cannot be sent during a turn: press Enter to queue it")
    );
    // Built-in commands that need no turn run at once, even now.
    enter(&mut ui);
    assert!(
        everything(&ui)
            .iter()
            .any(|r| r.starts_with("No tokens used yet"))
    );
    gate.notify_one();
    settle(&mut ui).await;
    ui.finish().await.unwrap();
}
