//! What the terminal shows has the secrets harness knows replaced by `[redacted]`: the reply,
//! even when a secret arrives in pieces, tool calls and their output, an approval's diff of a
//! file, and the warnings the host reports.

use std::{path::Path, sync::Arc, sync::Mutex, time::Duration};

use harness_core::{
    agent::{Agent, AgentConfig},
    engine::{EngineConfig, PermissionEngine, RuleSet},
    permission::Mode,
    provider::{FinishReason, ProviderEvent},
    redact::Redactor,
    testing::{MockProvider, Script},
    tool::{Tool, ToolContext, ToolRegistry},
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
use serde_json::json;

const SECRET: &str = "sk-canary-0123456789abcdef";

/// Reports each warning it is given once, as the credential store does.
#[derive(Default)]
struct Warns(Arc<Mutex<Vec<String>>>);

impl Host for Warns {
    fn is_command(&self, _name: &str) -> bool {
        false
    }
    fn prepare(&mut self, _typed: &str) -> Prepared {
        unreachable!()
    }
    fn take_warnings(&self) -> Vec<String> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

fn start(provider: Arc<MockProvider>, dir: &Path, mode: Mode, host: Warns) -> Ui<TestBackend> {
    let policy = Arc::new(PermissionEngine::new(EngineConfig {
        mode,
        workspace: dir.to_path_buf(),
        read_dirs: Vec::new(),
        rules: RuleSet::default(),
        // Shell commands run directly in these tests, as if sandboxed.
        sandbox_available: true,
        writes_need_approval: false,
    }));
    let tools: Vec<Arc<dyn Tool>> = ["read", "edit", "bash"]
        .into_iter()
        .map(|name| harness_tools::builtin().get(name).unwrap())
        .collect();
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
        default_mode: Mode::Auto,
        text_editor: None,
        notifier: None,
    };
    let redactor = Arc::new(Redactor::default());
    redactor.add(SECRET);
    let term = InlineTerminal::new(TestBackend::new(80, 24), 0).unwrap();
    let mut ui = Ui::start(agent, Box::new(host), term, options, approvals).with_redactor(redactor);
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

fn everything(ui: &Ui<TestBackend>) -> String {
    let backend = ui.terminal().backend();
    let mut out = rows(backend.scrollback());
    out.extend(rows(backend.buffer()));
    out.join("\n")
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

async fn settle(ui: &mut Ui<TestBackend>) {
    tokio::time::timeout(Duration::from_secs(10), ui.settle())
        .await
        .expect("the turn ends")
        .unwrap();
}

/// No part of the secret longer than a few characters is shown.
fn assert_hidden(shown: &str) {
    assert!(!shown.contains(&SECRET[..12]), "{shown}");
    assert!(!shown.contains(&SECRET[8..]), "{shown}");
}

#[tokio::test]
async fn a_secret_streamed_in_pieces_is_shown_redacted() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![Script::Reply(vec![
        Ok(ProviderEvent::TextDelta(format!(
            "the key is {}",
            &SECRET[..14]
        ))),
        Ok(ProviderEvent::TextDelta(format!(
            "{}, keep it",
            &SECRET[14..]
        ))),
        Ok(ProviderEvent::Finished(FinishReason::Stop)),
    ])]);
    let mut ui = start(provider, dir.path(), Mode::Auto, Warns::default());
    send(&mut ui, "what is the key?");
    settle(&mut ui).await;
    let shown = everything(&ui);
    assert_hidden(&shown);
    assert!(shown.contains("the key is [redacted], keep it"), "{shown}");
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn a_command_and_its_output_are_shown_redacted() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call("c1", "bash", json!({"command": format!("echo {SECRET}")})),
        Script::text("Done."),
    ]);
    let mut ui = start(provider, dir.path(), Mode::Auto, Warns::default());
    send(&mut ui, "print it");
    settle(&mut ui).await;
    let shown = everything(&ui);
    assert_hidden(&shown);
    assert!(shown.contains("$ echo [redacted]"), "{shown}");
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn an_approval_shows_a_file_diff_without_the_secret() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".env"), format!("KEY={SECRET}\nMODE=dev\n")).unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call("r1", "read", json!({"path": ".env"})),
        Script::tool_call(
            "e1",
            "edit",
            json!({"path": ".env", "old_string": "MODE=dev", "new_string": "MODE=prod"}),
        ),
        Script::text("Left it."),
    ]);
    let mut ui = start(provider, dir.path(), Mode::Ask, Warns::default());
    send(&mut ui, "switch to prod");
    tokio::time::timeout(
        Duration::from_secs(10),
        ui.until(|app| app.prompt().is_some()),
    )
    .await
    .expect("an approval is asked for")
    .unwrap();
    let shown = everything(&ui);
    assert_hidden(&shown);
    assert!(shown.contains("KEY=[redacted]"), "{shown}");
    assert!(shown.contains("MODE=prod"), "{shown}");
    tokio::time::sleep(harness_tui::approval::ARMING_DELAY).await;
    press(&mut ui, KeyCode::Char('n'));
    press(&mut ui, KeyCode::Enter);
    settle(&mut ui).await;
    ui.finish().await.unwrap();
}

// Final review M4: the prompt was built from the edit's arguments as the transcript holds them,
// redacted, so an edit whose text to replace holds a secret was said not to match the file,
// although approving it applies it. The prompt is built from the arguments as the model sent
// them, and only what it shows is redacted.
#[tokio::test]
async fn an_edit_of_a_secret_is_described_as_it_would_apply() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".env"), format!("KEY={SECRET}\nMODE=dev\n")).unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call("r1", "read", json!({"path": ".env"})),
        Script::tool_call(
            "e1",
            "edit",
            json!({"path": ".env", "old_string": format!("KEY={SECRET}"), "new_string": "KEY=rotated"}),
        ),
        Script::text("Rotated."),
    ]);
    let mut ui = start(provider, dir.path(), Mode::Ask, Warns::default());
    send(&mut ui, "rotate the key");
    tokio::time::timeout(
        Duration::from_secs(10),
        ui.until(|app| app.prompt().is_some()),
    )
    .await
    .expect("an approval is asked for")
    .unwrap();
    let shown = everything(&ui);
    assert_hidden(&shown);
    assert!(!shown.contains("not in the file"), "{shown}");
    assert!(shown.contains("edit .env (+1 -1)"), "{shown}");
    assert!(shown.contains("-KEY=[redacted]"), "{shown}");
    assert!(shown.contains("+KEY=rotated"), "{shown}");
    tokio::time::sleep(harness_tui::approval::ARMING_DELAY).await;
    press(&mut ui, KeyCode::Char('y'));
    settle(&mut ui).await;
    assert_eq!(
        std::fs::read_to_string(dir.path().join(".env")).unwrap(),
        "KEY=rotated\nMODE=dev\n"
    );
    assert_hidden(&everything(&ui));
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn the_hosts_warnings_are_shown_once() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![Script::text("one"), Script::text("two")]);
    let host = Warns::default();
    host.0.lock().unwrap().push(format!(
        "the renewed sign-in could not be stored ({SECRET})"
    ));
    let mut ui = start(provider, dir.path(), Mode::Auto, host);
    send(&mut ui, "first");
    settle(&mut ui).await;
    send(&mut ui, "second");
    settle(&mut ui).await;
    let shown = everything(&ui);
    assert_hidden(&shown);
    let warning = "warning: the renewed sign-in could not be stored ([redacted])";
    assert_eq!(shown.matches(warning).count(), 1, "{shown}");
    ui.finish().await.unwrap();
}

// As `harness ask` passes them on every 250 ms: a renewal waiting for another process says so
// while the turn waits for it, before any event follows.
#[tokio::test]
async fn the_hosts_warnings_are_shown_while_no_event_comes() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![Script::Hang(vec![])]);
    let host = Warns::default();
    let raised = host.0.clone();
    let mut ui = start(provider, dir.path(), Mode::Auto, host);
    send(&mut ui, "wait");
    // Takes in the events the turn starts with, until it waits for the provider.
    let _ = tokio::time::timeout(Duration::from_millis(500), ui.until(|_| false)).await;
    raised
        .lock()
        .unwrap()
        .push(format!("waiting for another harness process ({SECRET})"));
    let warning = "warning: waiting for another harness process ([redacted])";
    let shown = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            ui.next().await.unwrap();
            let shown = everything(&ui);
            if shown.contains(warning) {
                return shown;
            }
        }
    })
    .await
    .expect("shown while no event comes");
    assert_hidden(&shown);
    press(&mut ui, KeyCode::Esc);
    settle(&mut ui).await;
    ui.finish().await.unwrap();
    assert_eq!(everything(&ui).matches(warning).count(), 1);
}
