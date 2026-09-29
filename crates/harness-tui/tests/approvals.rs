//! Approvals at the terminal, with scripted keys: the diff of a file change, approving once or
//! for the session, denying with a reason, and the offer to run a command outside the sandbox.

use std::{path::Path, sync::Arc, time::Duration};

use async_trait::async_trait;
use harness_core::{
    agent::{Agent, AgentConfig},
    engine::{EngineConfig, PermissionEngine, RuleSet},
    message::{Message, ToolSpec},
    permission::{Action, Mode},
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

struct NoCommands;

impl Host for NoCommands {
    fn is_command(&self, _name: &str) -> bool {
        false
    }
    fn prepare(&mut self, _typed: &str) -> Prepared {
        unreachable!()
    }
}

/// Fails as if the sandbox blocked it, unless it runs outside the sandbox.
struct Boxed;

#[async_trait]
impl Tool for Boxed {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "fetch".into(),
            description: "fetch".into(),
            parameters: json!({"type": "object"}),
        }
    }
    fn action(&self, _args: &Value, _ctx: &ToolContext) -> Action {
        Action::Bash("curl https://example.com".into())
    }
    async fn run(&self, _args: Value, ctx: &ToolContext) -> ToolOutput {
        if ctx.unsandboxed {
            return ToolOutput::ok("fetched without the sandbox");
        }
        let mut out = ToolOutput::error("exit code 6\ncurl: (6) Could not resolve host");
        out.sandbox_denied = true;
        out
    }
}

fn start(provider: Arc<MockProvider>, dir: &Path, mode: Mode) -> Ui<TestBackend> {
    let policy = Arc::new(PermissionEngine::new(EngineConfig {
        mode,
        workspace: dir.to_path_buf(),
        read_dirs: Vec::new(),
        rules: RuleSet::default(),
        // Shell commands run directly in these tests, as if sandboxed.
        sandbox_available: true,
        writes_need_approval: false,
    }));
    let mut tools: Vec<Arc<dyn Tool>> = vec![Arc::new(Boxed)];
    for name in ["read", "write", "edit", "bash"] {
        tools.push(harness_tools::builtin().get(name).unwrap());
    }
    let (approver, approvals) = ChannelApprover::new();
    let agent = Agent::new(
        provider,
        ToolRegistry::new(tools),
        policy,
        approver,
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
    };
    let term = InlineTerminal::new(TestBackend::new(80, 24), 0).unwrap();
    let mut ui = Ui::start(agent, Box::new(NoCommands), term, options, approvals);
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

fn screen(ui: &Ui<TestBackend>) -> Vec<String> {
    rows(ui.terminal().backend().buffer())
}

fn everything(ui: &Ui<TestBackend>) -> Vec<String> {
    let backend = ui.terminal().backend();
    let mut out = rows(backend.scrollback());
    out.extend(rows(backend.buffer()));
    out
}

fn press(ui: &mut Ui<TestBackend>, code: KeyCode) {
    ui.handle(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
        .unwrap();
}

fn send(ui: &mut Ui<TestBackend>, text: &str) {
    for c in text.chars() {
        press(ui, KeyCode::Char(c));
    }
    press(ui, KeyCode::Enter);
}

async fn until_asked(ui: &mut Ui<TestBackend>) {
    tokio::time::timeout(
        Duration::from_secs(10),
        ui.until(|app| app.prompt().is_some()),
    )
    .await
    .expect("an approval is asked for")
    .unwrap();
}

async fn settle(ui: &mut Ui<TestBackend>) {
    tokio::time::timeout(Duration::from_secs(10), ui.settle())
        .await
        .expect("the turn ends")
        .unwrap();
}

/// What the model was told about tool call `id`.
fn tool_result(provider: &MockProvider, id: &str) -> String {
    provider
        .requests()
        .iter()
        .flat_map(|r| r.messages.clone())
        .find_map(|m| match m {
            Message::Tool {
                call_id, content, ..
            } if call_id == id => Some(content),
            _ => None,
        })
        .unwrap_or_default()
}

fn edit_script(file: &str, from: &str, to: &str) -> Vec<Script> {
    vec![
        Script::tool_call("r1", "read", json!({"path": file})),
        Script::tool_call(
            "e1",
            "edit",
            json!({"path": file, "old_string": from, "new_string": to}),
        ),
        Script::text("Edited."),
    ]
}

#[tokio::test]
async fn an_edit_in_ask_mode_shows_its_diff_and_runs_once_approved() {
    let dir = tempfile::tempdir().unwrap();
    let text: String = (1..=9).map(|i| format!("line {i}\n")).collect();
    std::fs::write(dir.path().join("notes.txt"), &text).unwrap();
    let provider = MockProvider::new(edit_script("notes.txt", "line 5\n", "line five\n"));
    let mut ui = start(provider.clone(), dir.path(), Mode::Ask);
    send(&mut ui, "fix line 5");
    until_asked(&mut ui).await;
    let shown = screen(&ui);
    let at = |text: &str| {
        shown
            .iter()
            .position(|r| r.trim_start() == text)
            .unwrap_or_else(|| panic!("no {text:?} in {shown:#?}"))
    };
    assert!(
        shown.iter().any(|r| r.starts_with("approve? write ")),
        "{shown:#?}"
    );
    assert!(shown.iter().any(|r| r.contains("edit notes.txt (+1 -1)")));
    assert!(at("@@ -2,7 +2,7 @@") < at("-line 5"));
    assert_eq!(at("-line 5") + 1, at("+line five"));
    assert!(
        shown
            .iter()
            .any(|r| r.contains("[a] yes, for this session"))
    );
    // Nothing changed yet.
    assert_eq!(
        std::fs::read_to_string(dir.path().join("notes.txt")).unwrap(),
        text
    );
    press(&mut ui, KeyCode::Char('y'));
    settle(&mut ui).await;
    assert!(
        std::fs::read_to_string(dir.path().join("notes.txt"))
            .unwrap()
            .contains("line five")
    );
    assert!(
        everything(&ui)
            .iter()
            .any(|r| r.starts_with("✓ approved: write"))
    );
    assert!(everything(&ui).iter().any(|r| r == "Edited."));
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn denying_with_a_reason_tells_the_model_and_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
    let provider = MockProvider::new(edit_script("a.txt", "one", "two"));
    let mut ui = start(provider.clone(), dir.path(), Mode::Ask);
    send(&mut ui, "change it");
    until_asked(&mut ui).await;
    press(&mut ui, KeyCode::Char('n'));
    assert!(
        screen(&ui)
            .iter()
            .any(|r| r.starts_with("tell the model why"))
    );
    send(&mut ui, "keep it as it is");
    settle(&mut ui).await;
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "one\n"
    );
    assert_eq!(
        tool_result(&provider, "e1"),
        "the user denied this action: keep it as it is"
    );
    assert!(
        everything(&ui)
            .iter()
            .any(|r| r.contains("denied: write") && r.ends_with("(keep it as it is)"))
    );
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn a_command_approved_for_the_session_is_not_asked_about_again() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call("b1", "bash", json!({"command": "echo one"})),
        Script::tool_call("b2", "bash", json!({"command": "echo two"})),
        Script::text("Both ran."),
    ]);
    let mut ui = start(provider.clone(), dir.path(), Mode::Ask);
    send(&mut ui, "run them");
    until_asked(&mut ui).await;
    assert!(screen(&ui).iter().any(|r| r.trim_start() == "$ echo one"));
    press(&mut ui, KeyCode::Char('a'));
    settle(&mut ui).await;
    assert!(tool_result(&provider, "b2").contains("two"));
    let approvals = everything(&ui)
        .iter()
        .filter(|r| r.starts_with("✓ approved"))
        .count();
    assert_eq!(approvals, 1);
    assert!(everything(&ui).iter().any(|r| r == "Both ran."));
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn a_blocked_command_can_run_outside_the_sandbox_once_but_never_for_the_session() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call("f1", "fetch", json!({})),
        Script::text("Fetched."),
    ]);
    let mut ui = start(provider.clone(), dir.path(), Mode::Auto);
    send(&mut ui, "fetch it");
    until_asked(&mut ui).await;
    let shown = screen(&ui).join("\n");
    assert!(
        shown.contains("the sandbox may have blocked this command"),
        "{shown}"
    );
    assert!(!shown.contains("for this session"), "{shown}");
    // `a` does nothing here.
    press(&mut ui, KeyCode::Char('a'));
    assert!(ui.app().prompt().is_some());
    press(&mut ui, KeyCode::Char('y'));
    settle(&mut ui).await;
    assert_eq!(tool_result(&provider, "f1"), "fetched without the sandbox");
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn a_long_diff_scrolls_inside_the_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let content: String = (1..=100).map(|i| format!("row {i}\n")).collect();
    let provider = MockProvider::new(vec![
        Script::tool_call(
            "w1",
            "write",
            json!({"path": "big.txt", "content": content}),
        ),
        Script::text("Written."),
    ]);
    let mut ui = start(provider.clone(), dir.path(), Mode::Ask);
    send(&mut ui, "write it");
    until_asked(&mut ui).await;
    let shown = screen(&ui);
    assert!(shown.iter().any(|r| r.contains("write big.txt (+100 -0)")));
    assert!(shown.iter().any(|r| r.trim_start() == "+row 1"));
    assert!(!shown.iter().any(|r| r.trim_start() == "+row 60"));
    assert!(
        shown
            .iter()
            .any(|r| r.contains("of 102: Up, Down, PgUp and PgDn scroll")),
        "{shown:#?}"
    );
    for _ in 0..6 {
        press(&mut ui, KeyCode::PageDown);
    }
    let shown = screen(&ui);
    assert!(
        shown.iter().any(|r| r.trim_start() == "+row 100"),
        "{shown:#?}"
    );
    assert!(!shown.iter().any(|r| r.trim_start() == "+row 1"));
    press(&mut ui, KeyCode::Char('y'));
    settle(&mut ui).await;
    assert!(dir.path().join("big.txt").exists());
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn esc_at_a_prompt_denies_and_stops_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call("b1", "bash", json!({"command": "echo hi"})),
        Script::text("never asked"),
    ]);
    let mut ui = start(provider.clone(), dir.path(), Mode::Ask);
    send(&mut ui, "go");
    until_asked(&mut ui).await;
    press(&mut ui, KeyCode::Esc);
    settle(&mut ui).await;
    assert!(ui.app().prompt().is_none());
    assert!(
        everything(&ui)
            .iter()
            .any(|r| r.starts_with("✗ denied, and stopped"))
    );
    assert!(everything(&ui).iter().any(|r| r == "interrupted"));
    assert_eq!(provider.requests().len(), 1);
    ui.finish().await.unwrap();
}

// Review Focus: Ctrl+C at a prompt must answer it (no) and stop the turn, as Esc does; a turn
// waiting on an unanswered approval could not stop.
#[tokio::test]
async fn ctrl_c_at_a_prompt_denies_and_stops_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call("b1", "bash", json!({"command": "echo hi"})),
        Script::text("never asked"),
    ]);
    let mut ui = start(provider.clone(), dir.path(), Mode::Ask);
    send(&mut ui, "go");
    until_asked(&mut ui).await;
    ui.handle(Event::Key(KeyEvent::new(
        KeyCode::Char('c'),
        KeyModifiers::CONTROL,
    )))
    .unwrap();
    settle(&mut ui).await;
    assert!(ui.app().prompt().is_none());
    assert!(
        everything(&ui)
            .iter()
            .any(|r| r.starts_with("✗ denied, and stopped"))
    );
    assert!(everything(&ui).iter().any(|r| r == "interrupted"));
    assert_eq!(provider.requests().len(), 1);
    ui.finish().await.unwrap();
}

// Review Focus: an approval that arrives while the user is typing replaces the input only while
// it waits; the draft is neither lost nor sent.
#[tokio::test]
async fn a_draft_typed_before_an_approval_is_kept() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call("b1", "bash", json!({"command": "echo hi"})),
        Script::text("Done."),
    ]);
    let mut ui = start(provider.clone(), dir.path(), Mode::Ask);
    send(&mut ui, "go");
    for c in "next idea".chars() {
        press(&mut ui, KeyCode::Char(c));
    }
    until_asked(&mut ui).await;
    press(&mut ui, KeyCode::Char('y'));
    settle(&mut ui).await;
    assert_eq!(ui.app().editor().text(), "next idea");
    assert_eq!(provider.requests().len(), 2);
    ui.finish().await.unwrap();
}

// Review Focus: a terminal made narrow and short still shows the prompt, wrapped, with its keys,
// and takes the answer.
#[tokio::test]
async fn a_narrowed_terminal_wraps_the_prompt_and_still_takes_the_answer() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call(
            "b1",
            "bash",
            json!({"command": "echo a-long-argument-that-does-not-fit-in-the-width"}),
        ),
        Script::text("ok"),
    ]);
    let mut ui = start(provider.clone(), dir.path(), Mode::Ask);
    ui.terminal_mut().backend_mut().resize(16, 12);
    ui.handle(Event::Resize(16, 12)).unwrap();
    send(&mut ui, "go");
    until_asked(&mut ui).await;
    let shown = screen(&ui).join("\n");
    assert!(shown.contains("approve?"), "{shown}");
    assert!(shown.contains("[y] yes"), "{shown}");
    press(&mut ui, KeyCode::Char('y'));
    settle(&mut ui).await;
    assert!(tool_result(&provider, "b1").contains("a-long-argument"));
    ui.finish().await.unwrap();
}
