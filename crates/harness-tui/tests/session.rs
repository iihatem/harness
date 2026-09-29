//! The interactive session end to end: scripted keys and pastes go in, the agent runs against
//! the mock provider, and what reaches the screen and the scrollback is checked on ratatui's
//! `TestBackend`.

mod support;

use std::{
    cell::RefCell,
    path::Path,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use harness_core::{
    agent::{Agent, AgentConfig, NonInteractive},
    engine::{EngineConfig, PermissionEngine, RuleSet},
    message::{ChatRequest, Message},
    permission::Mode,
    provider::{FinishReason, Provider, ProviderEvent, ProviderStream},
    testing::{MockProvider, Script},
    tool::{ToolContext, ToolRegistry},
    turn::{InputPart, TurnInput},
};
use harness_tui::{
    app::{App, Host, Options, Prepared},
    approval::ChannelApprover,
    inline::InlineTerminal,
    style::Theme,
    terminal::{Modes, RawMode},
    ui::{Flow, Ui},
};
use ratatui::{
    backend::TestBackend,
    buffer::Buffer,
    crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers},
};

/// Expands `/greet <name>` into a greeting, as harness-cli expands custom commands.
struct TestHost;

impl Host for TestHost {
    fn is_command(&self, name: &str) -> bool {
        name == "greet"
    }

    fn prepare(&mut self, typed: &str) -> Prepared {
        let name = typed.trim_start_matches("/greet").trim();
        Prepared {
            input: TurnInput {
                parts: vec![InputPart::Text(format!("Say hello to {name}"))],
                display: Some(typed.to_string()),
                ..TurnInput::default()
            },
            notes: vec!["/greet runs as its command file asks".into()],
            warnings: Vec::new(),
        }
    }
}

fn agent(provider: Arc<dyn Provider>, dir: &Path) -> Agent {
    let policy = Arc::new(PermissionEngine::new(EngineConfig {
        mode: Mode::Auto,
        workspace: dir.to_path_buf(),
        read_dirs: Vec::new(),
        rules: RuleSet::default(),
        sandbox_available: false,
        writes_need_approval: false,
    }));
    Agent::new(
        provider,
        ToolRegistry::new(Vec::new()),
        policy,
        Arc::new(NonInteractive),
        AgentConfig::new("mock/m", "m", "system", dir.join("out")),
        ToolContext::new(dir),
    )
}

fn options(dir: &Path) -> Options {
    Options {
        theme: Theme::monochrome(),
        model: "mock/m".into(),
        mode: Mode::Auto,
        commands: vec![
            ("help".into(), "List commands".into()),
            ("rewind".into(), "Rewind code, conversation, or both".into()),
            ("greet".into(), "Say hello".into()),
        ],
        workspace: dir.to_path_buf(),
        history: Vec::new(),
        instruction_files: Vec::new(),
        window_note: None,
        default_mode: Mode::Auto,
        text_editor: None,
        notifier: None,
    }
}

fn start(provider: Arc<MockProvider>, dir: &Path) -> Ui<TestBackend> {
    let term = InlineTerminal::new(TestBackend::new(60, 16), 0).unwrap();
    let mut ui = Ui::start(
        agent(provider, dir),
        Box::new(TestHost),
        term,
        options(dir),
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

/// The scrollback, then the screen.
fn everything(ui: &Ui<TestBackend>) -> Vec<String> {
    let backend = ui.terminal().backend();
    let mut out = rows(backend.scrollback());
    out.extend(rows(backend.buffer()));
    out
}

fn shows(ui: &Ui<TestBackend>, text: &str) -> bool {
    everything(ui).iter().any(|row| row.contains(text))
}

fn press(ui: &mut Ui<TestBackend>, code: KeyCode) -> Flow {
    ui.handle(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
        .unwrap()
}

fn ctrl(ui: &mut Ui<TestBackend>, c: char) -> Flow {
    ui.handle(Event::Key(KeyEvent::new(
        KeyCode::Char(c),
        KeyModifiers::CONTROL,
    )))
    .unwrap()
}

fn type_text(ui: &mut Ui<TestBackend>, text: &str) {
    for c in text.chars() {
        press(ui, KeyCode::Char(c));
    }
}

async fn settle(ui: &mut Ui<TestBackend>) {
    tokio::time::timeout(Duration::from_secs(10), ui.settle())
        .await
        .expect("the turn ends")
        .unwrap();
}

fn last_user_message(provider: &MockProvider) -> String {
    let requests = provider.requests();
    requests
        .last()
        .and_then(|r| {
            r.messages.iter().rev().find_map(|m| match m {
                Message::User { content } => Some(content.clone()),
                _ => None,
            })
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn a_prompt_runs_a_turn_and_the_reply_goes_into_the_scrollback() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![Script::text("Hello **there**")]);
    let mut ui = start(provider.clone(), dir.path());
    assert!(shows(&ui, "mock/m · auto"));
    type_text(&mut ui, "hi");
    press(&mut ui, KeyCode::Enter);
    settle(&mut ui).await;
    assert_eq!(last_user_message(&provider), "hi");
    let screen = everything(&ui);
    let user = screen.iter().position(|r| r == "› hi").expect("the prompt");
    let reply = screen
        .iter()
        .position(|r| r == "Hello there")
        .expect("the reply");
    assert!(user < reply, "{screen:#?}");
    // The input is empty again, under the reply.
    assert!(screen[reply..].iter().any(|r| r == "›"), "{screen:#?}");
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn esc_interrupts_a_running_turn_and_the_session_goes_on() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::Hang(vec![harness_core::provider::ProviderEvent::TextDelta(
            "partial answer".into(),
        )]),
        Script::text("second answer"),
    ]);
    let mut ui = start(provider.clone(), dir.path());
    type_text(&mut ui, "go");
    press(&mut ui, KeyCode::Enter);
    while !shows(&ui, "partial answer") {
        ui.next().await.unwrap();
    }
    // Enter while a turn runs queues the input; the interruption puts it back in the editor.
    type_text(&mut ui, "later");
    press(&mut ui, KeyCode::Enter);
    assert_eq!(ui.app().editor().text(), "");
    assert!(shows(&ui, "queued: later"));
    press(&mut ui, KeyCode::Esc);
    settle(&mut ui).await;
    assert!(shows(&ui, "interrupted"));
    assert!(!ui.app().busy());
    assert_eq!(ui.app().editor().text(), "later");
    press(&mut ui, KeyCode::Enter);
    settle(&mut ui).await;
    assert!(shows(&ui, "second answer"));
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn ctrl_c_clears_the_input_and_exits_when_pressed_twice() {
    let dir = tempfile::tempdir().unwrap();
    let mut ui = start(MockProvider::new(Vec::new()), dir.path());
    type_text(&mut ui, "abc");
    assert_eq!(ctrl(&mut ui, 'c'), Flow::Continue);
    assert_eq!(ui.app().editor().text(), "");
    assert!(shows(&ui, "press Ctrl+C again to exit"));
    assert_eq!(ctrl(&mut ui, 'c'), Flow::Quit);
    ui.finish().await.unwrap();
    // Ctrl+D on empty input exits too.
    let mut ui = start(MockProvider::new(Vec::new()), dir.path());
    assert_eq!(ctrl(&mut ui, 'd'), Flow::Quit);
    ui.finish().await.unwrap();
}

#[test]
fn a_second_ctrl_c_after_two_seconds_does_not_exit() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = App::new(options(dir.path()), Box::new(TestHost), 60);
    let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    let start = Instant::now();
    assert!(app.on_key(key, start).is_none());
    assert!(app.on_key(key, start + Duration::from_secs(3)).is_none());
    assert!(matches!(
        app.on_key(key, start + Duration::from_millis(4500)),
        Some(harness_tui::app::Action::Quit)
    ));
}

// Review A's re-review of wave 2, nit: the hint is wrapped, not cut at the row's end, so its
// advice to mention a file with @ is there on a narrow terminal too.
#[test]
fn a_paste_too_large_to_send_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = App::new(options(dir.path()), Box::new(TestHost), 60);
    app.on_paste(&"x".repeat(harness_tui::editor::PASTE_MAX_BYTES + 1));
    assert!(app.editor().is_empty());
    let (lines, _) = app.live(10);
    let shown: Vec<String> = lines.iter().map(harness_tui::text::plain).collect();
    assert!(lines.iter().all(|l| l.width() <= 60), "{shown:#?}");
    let joined = shown.join(" ");
    assert!(joined.contains("the paste is too large"), "{shown:#?}");
    assert!(joined.contains("mention it with @"), "{shown:#?}");
}

#[tokio::test]
async fn help_lists_the_commands_and_the_keys() {
    let dir = tempfile::tempdir().unwrap();
    let mut ui = start(MockProvider::new(Vec::new()), dir.path());
    type_text(&mut ui, "/help");
    press(&mut ui, KeyCode::Enter);
    assert!(shows(&ui, "/greet  Say hello"));
    assert!(shows(&ui, "Esc  interrupt the running turn"));
    assert!(!ui.app().busy());
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn later_built_ins_say_so_and_unknown_commands_are_explained() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(Vec::new());
    let mut ui = start(provider.clone(), dir.path());
    type_text(&mut ui, "/login");
    press(&mut ui, KeyCode::Esc);
    press(&mut ui, KeyCode::Enter);
    assert!(shows(&ui, "/login is not available yet"));
    type_text(&mut ui, "/nope");
    press(&mut ui, KeyCode::Enter);
    assert!(shows(&ui, "unknown command /nope"));
    assert_eq!(ui.app().editor().text(), "/nope");
    assert!(provider.requests().is_empty());
    ui.finish().await.unwrap();
}

// Review C, minor 8: `/login` and `/model` said they come "with provider sign-in (P4)" and
// "with model profiles (P4)"; P4 shipped, and a phase name means nothing to a user. They should
// say what to do now instead.
#[tokio::test]
async fn login_and_model_say_what_to_do_now_not_a_phase_name() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(Vec::new());
    let mut ui = start(provider, dir.path());
    type_text(&mut ui, "/login");
    press(&mut ui, KeyCode::Enter);
    assert!(shows(&ui, "harness login") || shows(&ui, "harness auth add"));
    assert!(!shows(&ui, "P4"));
    type_text(&mut ui, "/model");
    press(&mut ui, KeyCode::Enter);
    assert!(shows(&ui, "--model"));
    assert!(!shows(&ui, "P4"));
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn a_custom_command_runs_as_the_host_expands_it() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![Script::text("Hello, Ann!")]);
    let mut ui = start(provider.clone(), dir.path());
    type_text(&mut ui, "/gr");
    // Tab completes the command.
    press(&mut ui, KeyCode::Tab);
    assert_eq!(ui.app().editor().text(), "/greet ");
    type_text(&mut ui, "Ann");
    press(&mut ui, KeyCode::Enter);
    settle(&mut ui).await;
    assert_eq!(last_user_message(&provider), "Say hello to Ann");
    assert!(shows(&ui, "› /greet Ann"));
    assert!(shows(&ui, "/greet runs as its command file asks"));
    assert!(shows(&ui, "Hello, Ann!"));
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn a_long_paste_is_shown_collapsed_and_sent_in_full() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![Script::text("Looks like a null pointer.")]);
    let mut ui = start(provider.clone(), dir.path());
    let trace: String = (1..=200).map(|i| format!("at frame {i}\n")).collect();
    type_text(&mut ui, "why? ");
    ui.handle(Event::Paste(trace.clone())).unwrap();
    press(&mut ui, KeyCode::Enter);
    settle(&mut ui).await;
    assert!(shows(&ui, "› why? [Pasted text #1, 200 lines]"));
    assert!(!shows(&ui, "at frame 200"));
    assert_eq!(last_user_message(&provider), format!("why? {trace}"));
    ui.finish().await.unwrap();
}

/// Records whether raw mode is on.
struct FakeRaw(Rc<RefCell<Vec<&'static str>>>);

impl RawMode for FakeRaw {
    fn enable(&mut self) -> std::io::Result<()> {
        self.0.borrow_mut().push("raw on");
        Ok(())
    }
    fn disable(&mut self) -> std::io::Result<()> {
        self.0.borrow_mut().push("raw off");
        Ok(())
    }
}

#[test]
fn the_terminal_modes_are_set_and_undone_in_reverse_order() {
    let log = Rc::new(RefCell::new(Vec::new()));
    let mut out = Vec::new();
    {
        let mut modes = Modes::enter(&mut out, FakeRaw(log.clone()), true).unwrap();
        assert_eq!(*log.borrow(), ["raw on"]);
        assert_eq!(modes.out().as_slice(), b"\x1b[?2004h\x1b[>1u");
        modes.suspend().unwrap();
        modes.resume().unwrap();
    }
    assert_eq!(*log.borrow(), ["raw on", "raw off", "raw on", "raw off"]);
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "\x1b[?2004h\x1b[>1u\x1b[<1u\x1b[?2004l\x1b[?2004h\x1b[>1u\x1b[<1u\x1b[?2004l"
    );
    // Without disambiguated keys, only bracketed paste.
    let mut out = Vec::new();
    drop(Modes::enter(&mut out, FakeRaw(log.clone()), false).unwrap());
    assert_eq!(String::from_utf8(out).unwrap(), "\x1b[?2004h\x1b[?2004l");
}

// Review Focus: leaving while a turn runs must stop it and let the session end, not hang on the
// turn or leave the agent (and its session file) behind.
#[tokio::test]
async fn quitting_while_a_turn_runs_stops_it_and_ends_the_session() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![Script::Hang(vec![
        harness_core::provider::ProviderEvent::TextDelta("still going".into()),
    ])]);
    let mut ui = start(provider, dir.path());
    type_text(&mut ui, "go");
    press(&mut ui, KeyCode::Enter);
    while !shows(&ui, "still going") {
        ui.next().await.unwrap();
    }
    assert_eq!(ctrl(&mut ui, 'c'), Flow::Continue);
    assert_eq!(ctrl(&mut ui, 'c'), Flow::Quit);
    tokio::time::timeout(Duration::from_secs(10), ui.finish())
        .await
        .expect("the session ends")
        .unwrap();
    // What streamed is kept, and the live region is gone.
    assert!(shows(&ui, "still going"));
    assert!(!shows(&ui, "mock/m · auto"));
}

/// Streams `count` words, one every `every`.
struct Trickle {
    count: usize,
    every: Duration,
}

impl Provider for Trickle {
    fn stream(&self, _request: ChatRequest) -> ProviderStream {
        use futures::StreamExt;
        let every = self.every;
        let words = futures::stream::iter(0..self.count).then(move |i| async move {
            tokio::time::sleep(every).await;
            Ok(ProviderEvent::TextDelta(format!("w{i} ")))
        });
        let end = futures::stream::iter([Ok(ProviderEvent::Finished(FinishReason::Stop))]);
        Box::pin(words.chain(end))
    }
}

// Review A's I1: the session redrew after every batch of the agent's events, so a reply streamed
// in small chunks cost a redraw per chunk. While events stream, it redraws at most once every
// 30 ms.
#[tokio::test(start_paused = true)]
async fn while_a_reply_streams_the_screen_is_redrawn_at_most_every_30_ms() {
    let dir = tempfile::tempdir().unwrap();
    let (backend, draws) = support::count::Counted::new(TestBackend::new(60, 16));
    let provider = Arc::new(Trickle {
        count: 200,
        every: Duration::from_millis(3),
    });
    let mut ui = Ui::start(
        agent(provider, dir.path()),
        Box::new(TestHost),
        InlineTerminal::new(backend, 0).unwrap(),
        options(dir.path()),
        ChannelApprover::new().1,
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
    let before = draws.load(std::sync::atomic::Ordering::SeqCst);
    let started = tokio::time::Instant::now();
    tokio::time::timeout(Duration::from_secs(60), ui.settle())
        .await
        .expect("the turn ends")
        .unwrap();
    let took = started.elapsed().as_millis() as usize;
    let drawn = draws.load(std::sync::atomic::Ordering::SeqCst) - before;
    assert!(took >= 600, "the reply took {took} ms");
    assert!(
        drawn <= took / 30 + 3,
        "{drawn} draws in {took} ms of streaming"
    );
    let screen = rows(ui.terminal().backend().inner.buffer());
    assert!(screen.iter().any(|r| r.contains("w199")), "{screen:#?}");
    ui.finish().await.unwrap();
}

use support::vt::{Vt, VtBackend};

/// A session on a terminal `cols` by `rows` whose screen is full of a shell's lines.
fn start_on_vt(
    provider: Arc<dyn Provider>,
    dir: &Path,
    cols: u16,
    rows: u16,
) -> (Ui<VtBackend>, Arc<std::sync::Mutex<Vt>>) {
    let mut vt = Vt::new(cols, rows);
    for i in 0..rows + 5 {
        vt.print(&format!("shell line {i}\n"));
    }
    let top = vt.cursor().y;
    let backend = VtBackend::new(vt);
    let shared = backend.shared();
    let mut ui = Ui::start(
        agent(provider, dir),
        Box::new(TestHost),
        InlineTerminal::new(backend, top).unwrap(),
        options(dir),
        ChannelApprover::new().1,
    );
    ui.draw().unwrap();
    (ui, shared)
}

/// The window changes size, then harness hears of it.
fn resize(ui: &mut Ui<VtBackend>, vt: &std::sync::Mutex<Vt>, cols: u16, rows: u16) {
    vt.lock().unwrap().resize(cols, rows);
    ui.handle(Event::Resize(cols, rows)).unwrap();
}

fn send(ui: &mut Ui<VtBackend>, text: &str) {
    for c in text.chars() {
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
}

/// Each of `lines` is shown exactly once, in scrollback or on screen, in this order, and so is
/// the stats line of each of `turns`.
fn once_in_order(vt: &std::sync::Mutex<Vt>, lines: &[String], turns: usize) {
    let everything = vt.lock().unwrap().everything();
    let stats = everything
        .iter()
        .filter(|r| r.starts_with("mock/m · first token"))
        .count();
    assert_eq!(stats, turns, "{everything:#?}");
    let mut at = 0;
    for line in lines {
        let found: Vec<usize> = everything
            .iter()
            .enumerate()
            .filter(|(_, row)| *row == line)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(
            found.len(),
            1,
            "{line:?} is shown {} times: {everything:#?}",
            found.len()
        );
        assert!(found[0] >= at, "{line:?} is out of order: {everything:#?}");
        at = found[0];
    }
    // The live region is on screen once, at the bottom of what harness drew.
    let status = everything
        .iter()
        .filter(|r| r.starts_with("mock/m · auto"))
        .count();
    assert_eq!(status, 1, "{everything:#?}");
}

// Review A's I2: after a resize, harness assumed the rows had stayed where they were. A terminal
// that keeps its cursor on screen as the window gets shorter pushes the rows above it into
// scrollback, so harness cleared transcript rows that had moved into its old place.
#[tokio::test]
async fn resizing_at_an_idle_prompt_keeps_every_line_once() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::text("first answer"),
        Script::text("second answer"),
    ]);
    let (mut ui, vt) = start_on_vt(provider, dir.path(), 60, 20);
    send(&mut ui, "one");
    tokio::time::timeout(Duration::from_secs(10), ui.settle())
        .await
        .unwrap()
        .unwrap();
    let mut lines: Vec<String> = (0..25).map(|i| format!("shell line {i}")).collect();
    lines.extend(["› one".to_string(), "first answer".to_string()]);
    let mut turns = 1;
    once_in_order(&vt, &lines, turns);
    resize(&mut ui, &vt, 60, 10);
    once_in_order(&vt, &lines, turns);
    send(&mut ui, "two");
    tokio::time::timeout(Duration::from_secs(10), ui.settle())
        .await
        .unwrap()
        .unwrap();
    lines.extend(["› two".to_string(), "second answer".to_string()]);
    turns += 1;
    once_in_order(&vt, &lines, turns);
    // Wider and taller, then narrower.
    resize(&mut ui, &vt, 80, 24);
    once_in_order(&vt, &lines, turns);
    resize(&mut ui, &vt, 40, 24);
    once_in_order(&vt, &lines, turns);
    let screen = vt.lock().unwrap().screen();
    let input = screen.iter().position(|r| r == "›").expect("the input");
    assert!(
        screen[input - 1].starts_with("mock/m · first token"),
        "{screen:#?}"
    );
    assert!(
        screen[..input].iter().any(|r| r == "second answer"),
        "{screen:#?}"
    );
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn resizing_while_a_reply_streams_keeps_every_line_once() {
    let numbered = |from: usize, to: usize| -> String {
        (from..=to)
            .map(|i| format!("NUM-{i:03}"))
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::Hang(vec![ProviderEvent::TextDelta(numbered(1, 30))]),
        Script::Hang(vec![ProviderEvent::TextDelta(numbered(31, 60))]),
    ]);
    let (mut ui, vt) = start_on_vt(provider, dir.path(), 60, 20);
    let mut lines: Vec<String> = (0..25).map(|i| format!("shell line {i}")).collect();
    for (turn, (from, to), (cols, rows)) in [(1, (1, 30), (60, 10)), (2, (31, 60), (80, 24))] {
        send(&mut ui, &format!("turn {turn}"));
        let last = format!("NUM-{to:03}");
        while !vt.lock().unwrap().everything().contains(&last) {
            ui.next().await.unwrap();
        }
        resize(&mut ui, &vt, cols, rows);
        ui.handle(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)))
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), ui.settle())
            .await
            .unwrap()
            .unwrap();
        lines.push(format!("› turn {turn}"));
        lines.extend((from..=to).map(|i| format!("NUM-{i:03}")));
        let turns = turn;
        once_in_order(&vt, &lines, turns);
    }
    ui.finish().await.unwrap();
}

// A redraw put off while the agent's events stream can come just after the terminal went away,
// before its reader has told the session: that is the terminal going away, not a failure.
#[tokio::test(start_paused = true)]
async fn a_put_off_redraw_after_the_terminal_went_away_ends_the_session_as_a_hangup() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![Script::Hang(vec![ProviderEvent::TextDelta(
        "streaming".into(),
    )])]);
    let (backend, gone) = support::gone::Breakable::new(TestBackend::new(60, 16));
    let mut ui = Ui::start(
        agent(provider, dir.path()),
        Box::new(TestHost),
        InlineTerminal::new(backend, 0).unwrap(),
        options(dir.path()),
        ChannelApprover::new().1,
    );
    let (keys, input) = futures::channel::mpsc::unbounded();
    for code in [KeyCode::Char('g'), KeyCode::Char('o'), KeyCode::Enter] {
        keys.unbounded_send(Ok(Event::Key(KeyEvent::new(code, KeyModifiers::NONE))))
            .unwrap();
    }
    let terminal_goes = async move {
        // The keys and the reply's first events are taken in, and a redraw is put off.
        tokio::time::sleep(Duration::from_millis(1)).await;
        gone.store(true, std::sync::atomic::Ordering::SeqCst);
        // The terminal's reader tells a moment later.
        tokio::time::sleep(Duration::from_millis(100)).await;
        drop(keys);
    };
    let (ending, ()) = tokio::join!(ui.run(input, std::future::pending()), terminal_goes);
    assert_eq!(ending.unwrap(), harness_tui::ui::Ending::Hangup);
}
