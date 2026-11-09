//! A turn on a delegated model (Claude Code, Codex) at the terminal, with a scripted child: its
//! text and tool rows draw like any turn's, its approval is asked in the same prompt (the same
//! keys), and Esc interrupts it.

use std::{path::Path, sync::Arc, time::Duration};

use async_trait::async_trait;
use harness_core::{
    agent::{Agent, AgentConfig},
    delegated::{ApprovalGate, DelegateTurns, DelegatedOutcome, DelegatedTurn, GateRequest},
    engine::{EngineConfig, PermissionEngine, RuleSet},
    event::{AgentEvent, TurnEndReason},
    meter::AccountKind,
    permission::{Action, Mode},
    provider::{Provider, ProviderStream},
    tool::{ToolContext, ToolRegistry},
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
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

struct NoCommands;
impl Host for NoCommands {
    fn is_command(&self, _name: &str) -> bool {
        false
    }
    fn prepare(&mut self, _typed: &str) -> Prepared {
        unreachable!()
    }
}

/// Asks to write `a.txt`, then shows a tool row and a message; or hangs until interrupted.
struct Child {
    hang: bool,
    /// Set once the child is running, for a test to wait on.
    started: Arc<std::sync::atomic::AtomicBool>,
}

#[async_trait]
impl DelegateTurns for Child {
    fn account(&self) -> AccountKind {
        AccountKind::Delegated
    }
    fn label(&self) -> String {
        "claude-code".into()
    }
    async fn prepare(&self, _gate: &dyn ApprovalGate) -> Result<(), String> {
        Ok(())
    }
    async fn run(
        &self,
        turn: DelegatedTurn,
        gate: &dyn ApprovalGate,
        events: &UnboundedSender<AgentEvent>,
        cancel: CancellationToken,
    ) -> DelegatedOutcome {
        self.started
            .store(true, std::sync::atomic::Ordering::SeqCst);
        if self.hang {
            cancel.cancelled().await;
            return DelegatedOutcome::ended(TurnEndReason::Interrupted);
        }
        let answer = gate
            .authorize(GateRequest {
                call_id: "t1".into(),
                tool: "write".into(),
                arguments: serde_json::json!({"path": "a.txt", "content": "hello\n"}),
                actions: vec![Action::Write(turn.workspace.join("a.txt"))],
                reason: String::new(),
            })
            .await;
        let _ = events.send(AgentEvent::ToolCallRequested {
            id: "t1".into(),
            name: "Write".into(),
            arguments: "{\"file_path\":\"a.txt\"}".into(),
        });
        let _ = events.send(AgentEvent::ToolCallFinished {
            id: "t1".into(),
            output: format!("{answer:?}"),
            is_error: false,
        });
        let _ = events.send(AgentEvent::AssistantMessage {
            content: "All done.".into(),
            model: turn.model_id,
            role: turn.role,
            switch_reason: None,
        });
        DelegatedOutcome {
            texts: vec!["All done.".into()],
            ..DelegatedOutcome::ended(TurnEndReason::Completed)
        }
    }
}

struct Delegated(Arc<Child>);
impl Provider for Delegated {
    fn stream(&self, _r: harness_core::message::ChatRequest) -> ProviderStream {
        unreachable!("a delegated model sends no requests")
    }
    fn delegated(&self) -> Option<Arc<dyn DelegateTurns>> {
        Some(self.0.clone())
    }
}

fn start(dir: &Path, child: Child) -> Ui<TestBackend> {
    let policy = Arc::new(PermissionEngine::new(EngineConfig {
        mode: Mode::Ask,
        workspace: dir.to_path_buf(),
        read_dirs: Vec::new(),
        rules: RuleSet::default(),
        sandbox_available: true,
        writes_need_approval: false,
    }));
    let (approver, approvals) = ChannelApprover::new();
    let agent = Agent::new(
        Arc::new(Delegated(Arc::new(child))),
        ToolRegistry::new(vec![]),
        policy,
        approver,
        AgentConfig::new("claude-code/sonnet", "sonnet", "system", dir.join("out")),
        ToolContext::new(dir),
    );
    let options = Options {
        theme: Theme::monochrome(),
        model: "claude-code/sonnet".into(),
        mode: Mode::Ask,
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
            row.iter()
                .map(|c| c.symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
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

#[tokio::test]
async fn the_childs_approval_is_asked_in_the_normal_prompt_and_its_rows_draw_like_any_turn() {
    let dir = tempfile::tempdir().unwrap();
    let mut ui = start(
        dir.path(),
        Child {
            hang: false,
            started: Arc::default(),
        },
    );
    send(&mut ui, "write it");
    tokio::time::timeout(
        Duration::from_secs(10),
        ui.until(|app| app.prompt().is_some()),
    )
    .await
    .expect("the child's approval is asked")
    .unwrap();
    let shown = everything(&ui);
    assert!(shown.contains("approve?"), "{shown}");
    assert!(shown.contains("write"), "{shown}");
    let armed = ui.app().armed_at().unwrap();
    tokio::time::sleep_until(tokio::time::Instant::from_std(armed)).await;
    press(&mut ui, KeyCode::Char('y'));
    tokio::time::timeout(Duration::from_secs(10), ui.settle())
        .await
        .expect("the turn ends")
        .unwrap();
    let shown = everything(&ui);
    assert!(shown.contains("Write"), "the child's tool row: {shown}");
    assert!(shown.contains("All done."), "{shown}");
    assert!(shown.contains("Allow"), "the child was told yes: {shown}");
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn esc_interrupts_a_delegated_turn() {
    let dir = tempfile::tempdir().unwrap();
    let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut ui = start(
        dir.path(),
        Child {
            hang: true,
            started: started.clone(),
        },
    );
    send(&mut ui, "go");
    // Wait until the child is running, then Esc.
    let begun = tokio::time::Instant::now();
    while !started.load(std::sync::atomic::Ordering::SeqCst) {
        assert!(
            begun.elapsed() < Duration::from_secs(10),
            "the child never started"
        );
        let _ = tokio::time::timeout(Duration::from_millis(20), ui.settle()).await;
    }
    press(&mut ui, KeyCode::Esc);
    tokio::time::timeout(Duration::from_secs(10), ui.settle())
        .await
        .expect("the turn ended after Esc")
        .unwrap();
    ui.finish().await.unwrap();
}
