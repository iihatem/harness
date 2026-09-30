//! Approvals at the terminal, with scripted keys: the diff of a file change, approving once or
//! for the session, denying with a reason, and the offer to run a command outside the sandbox.

use std::{path::Path, sync::Arc, time::Duration};

use async_trait::async_trait;
use harness_core::{
    agent::{Agent, AgentConfig, ApprovalDecision, ApprovalKind, ApprovalRequest},
    engine::{EngineConfig, PermissionEngine, RuleSet},
    message::{Message, ToolSpec},
    permission::{Action, Mode},
    testing::{MockProvider, Script},
    tool::{Tool, ToolContext, ToolOutput, ToolRegistry},
};
use harness_tui::{
    app::{Host, Options, Prepared},
    approval::{ARMING_DELAY, ChannelApprover, Requests},
    inline::InlineTerminal,
    input::Timed,
    style::Theme,
    ui::Ui,
};
use ratatui::{
    backend::TestBackend,
    buffer::Buffer,
    crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers},
};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

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
    start_with(provider, dir, mode, None)
}

/// Like [`start`], with the session's approvals coming from `approvals` instead of the agent.
fn start_with(
    provider: Arc<MockProvider>,
    dir: &Path,
    mode: Mode,
    approvals: Option<Requests>,
) -> Ui<TestBackend> {
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
    let (approver, agent_approvals) = ChannelApprover::new();
    let approvals = approvals.unwrap_or(agent_approvals);
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

/// Waits until an approval is shown and takes keys.
async fn until_asked(ui: &mut Ui<TestBackend>) {
    until_shown(ui).await;
    tokio::time::sleep(ARMING_DELAY).await;
}

/// Waits until the approval shown takes keys.
async fn until_armed(ui: &Ui<TestBackend>) {
    let armed = ui.app().armed_at().expect("the prompt was drawn");
    tokio::time::sleep_until(tokio::time::Instant::from_std(armed)).await;
}

/// Waits until an approval is shown.
async fn until_shown(ui: &mut Ui<TestBackend>) {
    tokio::time::timeout(
        Duration::from_secs(10),
        ui.until(|app| app.prompt().is_some()),
    )
    .await
    .expect("an approval is asked for")
    .unwrap();
}

fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
    Event::Key(KeyEvent::new(code, modifiers))
}

fn is_approval(answer: Result<ApprovalDecision, oneshot::error::TryRecvError>) -> bool {
    matches!(
        answer,
        Ok(ApprovalDecision::Approve | ApprovalDecision::ApproveForSession)
    )
}

fn request(command: &str) -> ApprovalRequest {
    ApprovalRequest {
        call_id: "c1".into(),
        tool: "bash".into(),
        arguments: serde_json::Value::Null,
        action: Action::Bash(command.into()),
        reason: format!("run `{command}`"),
        kind: ApprovalKind::Action,
        kept_for_session: true,
    }
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

// Review D C1, probe 3: a key the user typed before the approval was asked for is theirs, not the
// prompt's answer, however the key and the request reach the session.
#[tokio::test]
async fn a_key_queued_before_the_request_leaves_the_prompt_pending() {
    for _ in 0..20 {
        let dir = tempfile::tempdir().unwrap();
        let (requests, approvals) = mpsc::unbounded_channel();
        let mut ui = start_with(
            MockProvider::new(vec![]),
            dir.path(),
            Mode::Ask,
            Some(approvals),
        );
        let (keys, input) = futures::channel::mpsc::unbounded();
        keys.unbounded_send(Ok(key(KeyCode::Char('y'), KeyModifiers::NONE)))
            .unwrap();
        let (reply, mut answer) = oneshot::channel();
        requests.send((request("cargo test"), reply)).unwrap();
        // Long enough for both to be taken in.
        let run = ui.run(input, std::future::pending());
        let _ = tokio::time::timeout(Duration::from_millis(100), run).await;
        assert!(
            !is_approval(answer.try_recv()),
            "a typed-ahead key approved"
        );
        assert!(ui.app().prompt().is_some());
        assert_eq!(ui.app().editor().text(), "y");
        drop(keys);
        ui.finish().await.unwrap();
    }
}

// Review D C1 residual: a key is timed by when the terminal's reader read it, not by when the
// session got to it. A key read before the prompt took keys goes to the input, even when the
// session takes it in only after (while it built a prompt's diff, say).
#[tokio::test]
async fn a_key_is_timed_by_when_it_was_read_not_when_it_is_handled() {
    let dir = tempfile::tempdir().unwrap();
    let (requests, approvals) = mpsc::unbounded_channel();
    let mut ui = start_with(
        MockProvider::new(vec![]),
        dir.path(),
        Mode::Ask,
        Some(approvals),
    );
    let (keys, mut input) = futures::channel::mpsc::unbounded::<std::io::Result<Timed>>();
    let (reply, mut answer) = oneshot::channel();
    requests.send((request("cargo test"), reply)).unwrap();
    let _ = tokio::time::timeout(
        Duration::from_millis(100),
        ui.run(&mut input, std::future::pending()),
    )
    .await;
    let armed = ui.app().armed_at().expect("the prompt was drawn");
    // Read just before the prompt took keys; taken in well after.
    keys.unbounded_send(Ok(Timed {
        event: key(KeyCode::Char('y'), KeyModifiers::NONE),
        at: armed - Duration::from_millis(1),
    }))
    .unwrap();
    tokio::time::sleep_until(tokio::time::Instant::from_std(armed + ARMING_DELAY)).await;
    let _ = tokio::time::timeout(
        Duration::from_millis(100),
        ui.run(&mut input, std::future::pending()),
    )
    .await;
    assert!(!is_approval(answer.try_recv()), "a key read early approved");
    assert!(ui.app().prompt().is_some());
    assert_eq!(ui.app().editor().text(), "y");
    drop(keys);
    ui.finish().await.unwrap();
}

// Review D C1, probe 2: only y, a, n and Esc answer, and not with Ctrl or Alt held; Enter, the
// new-line keys and readline's Ctrl+A were typed for the input.
#[tokio::test]
async fn enter_and_modified_keys_do_not_answer() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call("b1", "bash", json!({"command": "echo one"})),
        Script::tool_call("b2", "bash", json!({"command": "echo two"})),
        Script::text("Done."),
    ]);
    let mut ui = start(provider.clone(), dir.path(), Mode::Ask);
    send(&mut ui, "go");
    until_asked(&mut ui).await;
    for (code, modifiers) in [
        (KeyCode::Enter, KeyModifiers::NONE),
        (KeyCode::Enter, KeyModifiers::ALT),
        (KeyCode::Enter, KeyModifiers::SHIFT),
        (KeyCode::Char('a'), KeyModifiers::CONTROL),
        (KeyCode::Char('y'), KeyModifiers::ALT),
        (KeyCode::Char('y'), KeyModifiers::CONTROL),
        (KeyCode::Char('n'), KeyModifiers::ALT),
        (KeyCode::Char('Y'), KeyModifiers::SHIFT),
    ] {
        ui.handle(key(code, modifiers)).unwrap();
        assert!(
            ui.app().prompt().is_some(),
            "{modifiers:?}+{code:?} answered the prompt"
        );
    }
    assert!(
        !screen(&ui)
            .iter()
            .any(|r| r.starts_with("tell the model why"))
    );
    press(&mut ui, KeyCode::Char('y'));
    until_asked(&mut ui).await;
    // The second command is asked about: nothing was approved for the session.
    assert!(screen(&ui).iter().any(|r| r.trim_start() == "$ echo two"));
    press(&mut ui, KeyCode::Char('y'));
    settle(&mut ui).await;
    assert!(tool_result(&provider, "b1").contains("one"));
    assert!(tool_result(&provider, "b2").contains("two"));
    ui.finish().await.unwrap();
}

// Review D C1: a prompt takes keys only a moment after it is drawn; a key before that was typed
// for the input, which it goes to, and the prompt then waits for a pause after it.
#[tokio::test]
async fn a_key_within_the_grace_period_does_not_answer() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call("b1", "bash", json!({"command": "echo hi"})),
        Script::text("Done."),
    ]);
    let mut ui = start(provider.clone(), dir.path(), Mode::Ask);
    send(&mut ui, "go");
    until_shown(&mut ui).await;
    let armed = ui.app().armed_at().expect("the prompt was drawn");
    let early = armed - Duration::from_millis(1);
    ui.handle_at(key(KeyCode::Char('y'), KeyModifiers::NONE), early)
        .unwrap();
    assert!(ui.app().prompt().is_some());
    assert_eq!(ui.app().editor().text(), "y");
    let armed = ui.app().armed_at().unwrap();
    assert_eq!(armed, early + ARMING_DELAY);
    ui.handle_at(key(KeyCode::Char('y'), KeyModifiers::NONE), armed)
        .unwrap();
    assert!(ui.app().prompt().is_none());
    settle(&mut ui).await;
    assert!(tool_result(&provider, "b1").contains("hi"));
    assert_eq!(ui.app().editor().text(), "y");
    ui.finish().await.unwrap();
}

/// The line a prompt shows once typing went to the input instead of it.
const TYPED_PAST: &str = "your typing went to your message; the prompt takes keys once you pause";

// Review D C1 residual (its `typing.py`): a user types a follow-up at 12 keys a second, from
// before an approval appears until a second after it. Every key goes to the input, `a`, `n` and
// `y` included, and the prompt says so; it takes only a key pressed after a 500 ms pause.
#[tokio::test]
async fn typing_through_a_prompt_leaves_it_waiting_until_a_pause() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call("b1", "bash", json!({"command": "echo hi"})),
        Script::text("Done."),
    ]);
    let mut ui = start(provider.clone(), dir.path(), Mode::Ask);
    send(&mut ui, "go");
    until_shown(&mut ui).await;
    let shown = ui.app().armed_at().expect("the prompt was drawn") - ARMING_DELAY;
    let pace = Duration::from_millis(83);
    let mut at = shown - Duration::from_millis(500);
    let mut typed = String::new();
    for c in "then also add a test for the empty case and say what you ran; "
        .repeat(3)
        .chars()
    {
        if at > shown + Duration::from_secs(1) {
            break;
        }
        ui.handle_at(key(KeyCode::Char(c), KeyModifiers::NONE), at)
            .unwrap();
        typed.push(c);
        assert!(
            ui.app().prompt().is_some(),
            "{c:?}, {:?} after the prompt showed, answered it",
            at.saturating_duration_since(shown)
        );
        at += pace;
    }
    let last = at - pace;
    assert_eq!(ui.app().editor().text(), typed);
    let shown_rows = screen(&ui);
    assert!(
        !shown_rows
            .iter()
            .any(|r| r.starts_with("tell the model why")),
        "{shown_rows:#?}"
    );
    assert!(
        shown_rows.iter().any(|r| r == TYPED_PAST),
        "{shown_rows:#?}"
    );
    // Not quite a pause: the key is the input's, and the prompt waits for a pause after it.
    let almost = last + ARMING_DELAY - Duration::from_millis(1);
    ui.handle_at(key(KeyCode::Char('y'), KeyModifiers::NONE), almost)
        .unwrap();
    assert!(ui.app().prompt().is_some());
    assert_eq!(ui.app().armed_at(), Some(almost + ARMING_DELAY));
    ui.handle_at(
        key(KeyCode::Char('y'), KeyModifiers::NONE),
        almost + ARMING_DELAY,
    )
    .unwrap();
    assert!(ui.app().prompt().is_none());
    settle(&mut ui).await;
    assert!(tool_result(&provider, "b1").contains("hi"));
    assert!(
        everything(&ui)
            .iter()
            .any(|r| r.starts_with("✓ approved: ")),
        "{:#?}",
        everything(&ui)
    );
    assert_eq!(ui.app().editor().text(), format!("{typed}y"));
    ui.finish().await.unwrap();
}

// Review D C1: `n` typed ahead goes into the input, and the rest of the draft never becomes a
// reason sent to the model.
#[tokio::test]
async fn n_typed_ahead_does_not_turn_the_draft_into_a_denial_reason() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call("b1", "bash", json!({"command": "echo hi"})),
        Script::text("Done."),
        Script::text("Later."),
    ]);
    let mut ui = start(provider.clone(), dir.path(), Mode::Ask);
    send(&mut ui, "go");
    until_shown(&mut ui).await;
    let early = ui.app().armed_at().unwrap() - Duration::from_millis(100);
    for c in "no rush\r".chars() {
        let code = if c == '\r' {
            KeyCode::Enter
        } else {
            KeyCode::Char(c)
        };
        ui.handle_at(key(code, KeyModifiers::NONE), early).unwrap();
    }
    assert!(ui.app().prompt().is_some());
    assert!(
        !screen(&ui)
            .iter()
            .any(|r| r.starts_with("tell the model why"))
    );
    until_armed(&ui).await;
    press(&mut ui, KeyCode::Char('y'));
    settle(&mut ui).await;
    // Typed ahead with Enter: queued, and sent once the turn ended.
    assert!(tool_result(&provider, "b1").contains("hi"));
    assert!(
        everything(&ui).iter().any(|r| r == "› no rush"),
        "{:#?}",
        everything(&ui)
    );
    ui.finish().await.unwrap();
}

// Review D C1: Esc before the prompt takes keys still stops the turn, and so denies.
#[tokio::test]
async fn esc_before_the_prompt_takes_keys_still_denies_and_stops() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call("b1", "bash", json!({"command": "echo hi"})),
        Script::text("never asked"),
    ]);
    let mut ui = start(provider.clone(), dir.path(), Mode::Ask);
    send(&mut ui, "go");
    until_shown(&mut ui).await;
    let early = ui.app().armed_at().unwrap() - Duration::from_millis(100);
    ui.handle_at(key(KeyCode::Esc, KeyModifiers::NONE), early)
        .unwrap();
    settle(&mut ui).await;
    assert!(ui.app().prompt().is_none());
    assert!(
        everything(&ui)
            .iter()
            .any(|r| r.starts_with("✗ denied, and stopped"))
    );
    assert_eq!(provider.requests().len(), 1);
    ui.finish().await.unwrap();
}

// Review E M1: Ctrl+S while an approval waits says why nothing is sent.
#[tokio::test]
async fn ctrl_s_while_an_approval_waits_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call("b1", "bash", json!({"command": "echo hi"})),
        Script::text("Done."),
    ]);
    let mut ui = start(provider.clone(), dir.path(), Mode::Ask);
    send(&mut ui, "go");
    until_asked(&mut ui).await;
    ui.handle(key(KeyCode::Char('s'), KeyModifiers::CONTROL))
        .unwrap();
    assert!(ui.app().prompt().is_some());
    let shown = screen(&ui).join("\n");
    assert!(shown.contains("answer the approval first"), "{shown}");
    press(&mut ui, KeyCode::Char('y'));
    settle(&mut ui).await;
    ui.finish().await.unwrap();
}

// Review D M6: a paste while typing why goes into the reason.
#[tokio::test]
async fn a_paste_while_typing_the_reason_goes_into_it() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
    let provider = MockProvider::new(edit_script("a.txt", "one", "two"));
    let mut ui = start(provider.clone(), dir.path(), Mode::Ask);
    send(&mut ui, "change it");
    until_asked(&mut ui).await;
    press(&mut ui, KeyCode::Char('n'));
    ui.handle(Event::Paste("see the style guide".into()))
        .unwrap();
    press(&mut ui, KeyCode::Enter);
    settle(&mut ui).await;
    assert_eq!(
        tool_result(&provider, "e1"),
        "the user denied this action: see the style guide"
    );
    assert_eq!(ui.app().editor().text(), "");
    ui.finish().await.unwrap();
}

// Review D M5: approving for the session what the policy cannot keep reads "approved once".
#[tokio::test]
async fn a_session_approval_that_cannot_be_kept_reads_approved_once() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call("b1", "bash", json!({"command": "git reset --hard HEAD"})),
        Script::text("Done."),
    ]);
    let mut ui = start(provider.clone(), dir.path(), Mode::Ask);
    send(&mut ui, "go");
    until_asked(&mut ui).await;
    press(&mut ui, KeyCode::Char('a'));
    settle(&mut ui).await;
    let all = everything(&ui);
    assert!(
        all.iter().any(|r| r.starts_with("✓ approved once:")),
        "{all:#?}"
    );
    assert!(
        !all.iter()
            .any(|r| r.starts_with("✓ approved for this session"))
    );
    ui.finish().await.unwrap();
}

/// Pages down in the prompt until `seen` shows or `pages` run out; whether it showed.
fn page_to(ui: &mut Ui<TestBackend>, seen: &str, pages: usize) -> bool {
    for _ in 0..pages {
        if screen(ui).join("\n").contains(seen) {
            return true;
        }
        press(ui, KeyCode::PageDown);
    }
    screen(ui).join("\n").contains(seen)
}

// Review D C2, probe 1: a one-line command longer than the prompt says rows are hidden, and its
// end can be scrolled to.
#[tokio::test]
async fn a_long_one_line_command_scrolls_to_its_end() {
    let dir = tempfile::tempdir().unwrap();
    let command = format!("echo {} ; echo TAIL-MARKER", "x".repeat(3000));
    let provider = MockProvider::new(vec![
        Script::tool_call("b1", "bash", json!({ "command": command })),
        Script::text("Done."),
    ]);
    let mut ui = start(provider.clone(), dir.path(), Mode::Ask);
    send(&mut ui, "go");
    until_asked(&mut ui).await;
    let shown = screen(&ui).join("\n");
    assert!(!shown.contains("TAIL-MARKER"), "{shown}");
    assert!(
        shown.contains("rows 1-") && shown.contains(": Up, Down, PgUp and PgDn scroll"),
        "{shown}"
    );
    assert!(page_to(&mut ui, "TAIL-MARKER", 10), "{:#?}", screen(&ui));
    press(&mut ui, KeyCode::Char('n'));
    press(&mut ui, KeyCode::Enter);
    settle(&mut ui).await;
    ui.finish().await.unwrap();
}

// Review D C2: so can a long line in a diff.
#[tokio::test]
async fn a_long_diff_line_scrolls_to_its_end() {
    let dir = tempfile::tempdir().unwrap();
    let content = format!("{}TAIL-MARKER\n", "y".repeat(3000));
    let provider = MockProvider::new(vec![
        Script::tool_call(
            "w1",
            "write",
            json!({"path": "bundle.js", "content": content}),
        ),
        Script::text("Written."),
    ]);
    let mut ui = start(provider.clone(), dir.path(), Mode::Ask);
    send(&mut ui, "write it");
    until_asked(&mut ui).await;
    let shown = screen(&ui).join("\n");
    assert!(!shown.contains("TAIL-MARKER"), "{shown}");
    assert!(shown.contains("rows 1-"), "{shown}");
    assert!(page_to(&mut ui, "TAIL-MARKER", 10), "{:#?}", screen(&ui));
    press(&mut ui, KeyCode::Char('y'));
    settle(&mut ui).await;
    ui.finish().await.unwrap();
}

// Review D M3: what is approved shows the characters that draw as nothing, so two commands or
// paths that look the same are the same.
#[tokio::test]
async fn invisible_characters_in_what_is_approved_are_shown() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call(
            "b1",
            "bash",
            json!({"command": "rm -rf ./bu\u{200b}ild\u{2060} x\u{e0041}"}),
        ),
        Script::tool_call(
            "w1",
            "write",
            json!({"path": "a\nb.txt", "content": "ok\u{feff}\n"}),
        ),
        Script::text("Done."),
    ]);
    let mut ui = start(provider.clone(), dir.path(), Mode::Ask);
    send(&mut ui, "go");
    until_asked(&mut ui).await;
    let shown = screen(&ui).join("\n");
    assert!(
        shown.contains("$ rm -rf ./bu\\u{200b}ild\\u{2060} x\\u{e0041}"),
        "{shown}"
    );
    press(&mut ui, KeyCode::Char('n'));
    press(&mut ui, KeyCode::Enter);
    until_asked(&mut ui).await;
    let shown = screen(&ui).join("\n");
    assert!(shown.contains("write a\\nb.txt (+1 -0)"), "{shown}");
    assert!(shown.contains("+ok\\u{feff}"), "{shown}");
    press(&mut ui, KeyCode::Char('n'));
    press(&mut ui, KeyCode::Enter);
    settle(&mut ui).await;
    ui.finish().await.unwrap();
}

// Review D M4: a file harness shows no diff of still shows what would be written.
#[tokio::test]
async fn a_prompt_without_a_diff_still_shows_the_new_content() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("logo.bin"), [0xff, 0xfe, 0x00, 0x01]).unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call(
            "w1",
            "write",
            json!({"path": "logo.bin", "content": "now text\n"}),
        ),
        Script::text("Written."),
    ]);
    let mut ui = start(provider.clone(), dir.path(), Mode::Ask);
    send(&mut ui, "write it");
    until_asked(&mut ui).await;
    let shown = screen(&ui).join("\n");
    assert!(
        shown.contains("logo.bin: not text, so no diff is shown"),
        "{shown}"
    );
    assert!(shown.contains("+now text"), "{shown}");
    press(&mut ui, KeyCode::Char('n'));
    press(&mut ui, KeyCode::Enter);
    settle(&mut ui).await;
    ui.finish().await.unwrap();
}

// Review D I1: a write to a FIFO is asked about at once, with the new content and a note.
#[tokio::test]
async fn a_write_to_a_fifo_is_asked_about_at_once() {
    let dir = tempfile::tempdir().unwrap();
    assert!(
        std::process::Command::new("mkfifo")
            .arg(dir.path().join("pipe"))
            .status()
            .unwrap()
            .success()
    );
    let provider = MockProvider::new(vec![
        Script::tool_call("w1", "write", json!({"path": "pipe", "content": "data\n"})),
        Script::text("Left it."),
    ]);
    let mut ui = start(provider.clone(), dir.path(), Mode::Ask);
    send(&mut ui, "write it");
    until_asked(&mut ui).await;
    let shown = screen(&ui).join("\n");
    assert!(
        shown.contains("pipe: not a regular file, so no diff is shown"),
        "{shown}"
    );
    assert!(shown.contains("+data"), "{shown}");
    press(&mut ui, KeyCode::Char('n'));
    press(&mut ui, KeyCode::Enter);
    settle(&mut ui).await;
    ui.finish().await.unwrap();
}
