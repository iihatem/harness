//! Plan mode's flow: a planning turn ends with the plan and three choices, Build goes back to
//! the mode before plan mode and implements it, Edit opens it in the user's editor, and Keep
//! planning stays.

use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use harness_core::{
    agent::{Agent, AgentConfig},
    engine::{EngineConfig, PermissionEngine, RuleSet},
    message::Message,
    permission::Mode,
    session::Session,
    testing::{MockProvider, Script},
    tool::{Tool, ToolContext, ToolRegistry},
};
use harness_tui::{
    app::{Host, Options, Prepared},
    approval::ChannelApprover,
    inline::InlineTerminal,
    plan::{ExternalEditor, TextEditor},
    style::Theme,
    terminal::{Modes, RawMode},
    ui::Ui,
};
use ratatui::{
    backend::TestBackend,
    buffer::Buffer,
    crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers},
};
use serde_json::json;

const PLAN: &str = "1. Read src/login.rs\n2. Add a limiter\n3. Test it";

struct NoCommands;

impl Host for NoCommands {
    fn is_command(&self, _name: &str) -> bool {
        false
    }
    fn prepare(&mut self, _typed: &str) -> Prepared {
        unreachable!()
    }
}

/// Deletes step 3, as a user would in their editor.
struct DeleteStepThree;

impl TextEditor for DeleteStepThree {
    fn edit(&mut self, text: &str) -> std::io::Result<String> {
        Ok(text
            .lines()
            .filter(|l| !l.starts_with("3."))
            .map(|l| format!("{l}\n"))
            .collect())
    }
}

fn start(
    provider: Arc<MockProvider>,
    dir: &Path,
    mode: Mode,
    default_mode: Mode,
) -> Ui<TestBackend> {
    let policy = Arc::new(PermissionEngine::new(EngineConfig {
        mode,
        workspace: dir.to_path_buf(),
        read_dirs: Vec::new(),
        rules: RuleSet::default(),
        sandbox_available: true,
        writes_need_approval: false,
    }));
    let tools: Vec<Arc<dyn Tool>> = ["read", "write"]
        .iter()
        .map(|name| harness_tools::builtin().get(name).unwrap())
        .collect();
    let (approver, approvals) = ChannelApprover::new();
    let agent = Agent::new(
        provider,
        ToolRegistry::new(tools),
        policy,
        approver,
        AgentConfig::new("mock/m", "m", "system", dir.join("out")),
        ToolContext::new(dir).with_sandbox(None, mode.fs_access()),
    )
    .with_session(Session::create(&dir.join("sessions"), dir));
    let options = Options {
        theme: Theme::monochrome(),
        model: "mock/m".into(),
        mode,
        commands: Vec::new(),
        workspace: dir.to_path_buf(),
        history: Vec::new(),
        instruction_files: Vec::new(),
        window_note: None,
        default_mode,
        text_editor: Some(Box::new(DeleteStepThree)),
    };
    let term = InlineTerminal::new(TestBackend::new(100, 30), 0).unwrap();
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

fn everything(ui: &Ui<TestBackend>) -> Vec<String> {
    let backend = ui.terminal().backend();
    let mut out = rows(backend.scrollback());
    out.extend(rows(backend.buffer()));
    out
}

fn status(ui: &Ui<TestBackend>) -> String {
    rows(ui.terminal().backend().buffer())
        .into_iter()
        .rfind(|r| r.starts_with("mock/m · "))
        .unwrap()
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
        .expect("settles")
        .unwrap();
}

fn last_user(provider: &MockProvider) -> String {
    provider
        .requests()
        .last()
        .unwrap()
        .messages
        .iter()
        .rev()
        .find_map(|m| match m {
            Message::User { content } => Some(content.clone()),
            _ => None,
        })
        .unwrap()
}

/// Plans in `ui` (in plan mode already), with the model first trying to write a file.
async fn plan(ui: &mut Ui<TestBackend>) {
    send(ui, "add a login rate limiter");
    settle(ui).await;
}

fn planning_script(then: Vec<Script>) -> Arc<MockProvider> {
    let mut script = vec![
        Script::tool_call("w1", "write", json!({"path": "limiter.rs", "content": "x"})),
        Script::text(PLAN),
    ];
    script.extend(then);
    MockProvider::new(script)
}

#[tokio::test]
async fn a_planning_turn_changes_nothing_and_ends_with_the_plan_and_three_choices() {
    let dir = tempfile::tempdir().unwrap();
    let provider = planning_script(vec![]);
    let mut ui = start(provider.clone(), dir.path(), Mode::Auto, Mode::Auto);
    ui.handle(Event::Key(KeyEvent::new(
        KeyCode::BackTab,
        KeyModifiers::SHIFT,
    )))
    .unwrap();
    plan(&mut ui).await;
    assert!(!dir.path().join("limiter.rs").exists());
    let asked = provider.requests()[0]
        .messages
        .iter()
        .map(|m| format!("{m:?}"))
        .collect::<String>();
    assert!(
        asked.contains("step-by-step implementation plan"),
        "{asked}"
    );
    let screen = everything(&ui);
    assert!(screen.iter().any(|r| r == "3. Test it"), "{screen:#?}");
    assert!(
        screen.iter().any(|r| r
            == "The plan is ready: [b] build it  [e] edit it in your editor  [k] keep planning"),
        "{screen:#?}"
    );
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn build_goes_back_to_the_mode_before_plan_and_implements_the_plan() {
    let dir = tempfile::tempdir().unwrap();
    let provider = planning_script(vec![Script::text("Implemented.")]);
    let mut ui = start(provider.clone(), dir.path(), Mode::Auto, Mode::Ask);
    ui.handle(Event::Key(KeyEvent::new(
        KeyCode::BackTab,
        KeyModifiers::SHIFT,
    )))
    .unwrap();
    plan(&mut ui).await;
    press(&mut ui, KeyCode::Char('b'));
    settle(&mut ui).await;
    assert!(
        status(&ui).starts_with("mock/m · auto ·"),
        "{}",
        status(&ui)
    );
    // The mode change and the request to build go as one user message.
    let build = last_user(&provider);
    assert!(
        build.starts_with("[harness] The approval mode is now auto"),
        "{build}"
    );
    assert!(build.ends_with("\n\nImplement the plan above."), "{build}");
    assert!(everything(&ui).iter().any(|r| r == "› Build the plan"));
    assert!(everything(&ui).iter().any(|r| r == "Implemented."));
    ui.finish().await.unwrap();
    // The session keeps the approved plan.
    let sessions = dir.path().join("sessions");
    let file = std::fs::read_dir(&sessions)
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    let text = std::fs::read_to_string(file.path()).unwrap();
    assert!(
        text.contains(&format!(
            "\"plan\":{}",
            serde_json::to_string(PLAN).unwrap()
        )),
        "{text}"
    );
}

#[tokio::test]
async fn an_edited_plan_is_shown_again_and_built_as_edited() {
    let dir = tempfile::tempdir().unwrap();
    let provider = planning_script(vec![Script::text("Implemented.")]);
    let mut ui = start(provider.clone(), dir.path(), Mode::Plan, Mode::Auto);
    plan(&mut ui).await;
    press(&mut ui, KeyCode::Char('e'));
    let screen = everything(&ui);
    let edited = screen
        .iter()
        .position(|r| r == "the edited plan:")
        .expect("the edited plan");
    assert_eq!(screen[edited + 1], "1. Read src/login.rs");
    assert_eq!(screen[edited + 2], "2. Add a limiter");
    assert!(
        !screen[edited..].iter().any(|r| r == "3. Test it"),
        "{screen:#?}"
    );
    assert!(
        screen
            .iter()
            .any(|r| r.starts_with("The edited plan is ready: [b] build it"))
    );
    press(&mut ui, KeyCode::Enter);
    settle(&mut ui).await;
    let build = last_user(&provider);
    assert!(
        build.ends_with("\n\nImplement this plan:\n\n1. Read src/login.rs\n2. Add a limiter\n"),
        "{build}"
    );
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn keep_planning_stays_in_plan_mode_and_returns_to_the_input() {
    let dir = tempfile::tempdir().unwrap();
    let provider = planning_script(vec![Script::text("A better plan.")]);
    let mut ui = start(provider.clone(), dir.path(), Mode::Plan, Mode::Auto);
    plan(&mut ui).await;
    press(&mut ui, KeyCode::Char('k'));
    assert!(ui.app().plan_choice().is_none());
    assert!(
        everything(&ui)
            .iter()
            .any(|r| r == "still planning: say what to change")
    );
    assert!(status(&ui).starts_with("mock/m · plan ·"));
    send(&mut ui, "also cover the API");
    settle(&mut ui).await;
    assert!(ui.app().plan_choice().is_some());
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn a_session_started_in_plan_mode_is_told_to_plan_and_builds_in_the_default_mode() {
    let dir = tempfile::tempdir().unwrap();
    let provider = planning_script(vec![Script::text("Implemented.")]);
    let mut ui = start(provider.clone(), dir.path(), Mode::Plan, Mode::Ask);
    plan(&mut ui).await;
    let first = format!("{:?}", provider.requests()[0].messages);
    assert!(first.contains("The approval mode is now plan"), "{first}");
    press(&mut ui, KeyCode::Char('b'));
    settle(&mut ui).await;
    assert!(status(&ui).starts_with("mock/m · ask ·"), "{}", status(&ui));
    ui.finish().await.unwrap();
}

/// Records whether raw mode is on.
struct FakeRaw(Arc<Mutex<Vec<&'static str>>>);

impl RawMode for FakeRaw {
    fn enable(&mut self) -> std::io::Result<()> {
        self.0.lock().unwrap().push("raw on");
        Ok(())
    }
    fn disable(&mut self) -> std::io::Result<()> {
        self.0.lock().unwrap().push("raw off");
        Ok(())
    }
}

#[test]
fn the_editor_edits_the_plan_with_the_terminal_given_back_meanwhile() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut out = Vec::new();
    {
        let modes = Modes::enter(&mut out, FakeRaw(log.clone()), false).unwrap();
        let mut editor = ExternalEditor::new(
            r#"f() { grep -v '^3\.' "$1" > "$1.new"; mv "$1.new" "$1"; }; f"#.into(),
            modes,
        );
        let edited = editor.edit("1. a\n2. b\n3. c\n").unwrap();
        assert_eq!(edited, "1. a\n2. b\n");
        assert_eq!(*log.lock().unwrap(), ["raw on", "raw off", "raw on"]);
        // An editor that fails leaves the plan as it was.
        let mut failing = ExternalEditor::new(
            "false".into(),
            Modes::enter(Vec::new(), FakeRaw(log.clone()), false).unwrap(),
        );
        let error = failing.edit("plan").unwrap_err();
        assert!(error.to_string().contains("exited with"), "{error}");
    }
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "\x1b[?2004h\x1b[?2004l\x1b[?2004h\x1b[?2004l"
    );
    // The plan's file is gone.
    let left = std::fs::read_dir(std::env::temp_dir())
        .unwrap()
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with(&format!("harness-plan-{}-", std::process::id()))
        })
        .count();
    assert_eq!(left, 0);
}
