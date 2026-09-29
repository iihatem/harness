//! The status line, the stats after each turn, `/context` and `/usage`.

use std::{path::Path, sync::Arc, time::Duration};

use harness_core::{
    agent::{Agent, AgentConfig, ContextUsage, NonInteractive},
    engine::{EngineConfig, PermissionEngine, RuleSet},
    message::Usage,
    permission::Mode,
    provider::{FinishReason, ProviderEvent},
    testing::{MockProvider, Script},
    tool::{ToolContext, ToolRegistry},
};
use harness_tui::{
    app::{Host, Options, Prepared},
    inline::InlineTerminal,
    status::{self, Totals},
    style::Theme,
    text::plain,
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

fn start(provider: Arc<MockProvider>, dir: &Path, system: &str) -> Ui<TestBackend> {
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
        ToolRegistry::new(Vec::new()),
        policy,
        Arc::new(NonInteractive),
        AgentConfig::new("mock/m", "m", system, dir.join("out")),
        ToolContext::new(dir),
    );
    let options = Options {
        theme: Theme::monochrome(),
        model: "mock/m".into(),
        mode: Mode::Auto,
        commands: Vec::new(),
        workspace: dir.to_path_buf(),
        history: Vec::new(),
        instruction_files: vec![("AGENTS.md".into(), 2_000)],
        window_note: Some("assumed".into()),
    };
    let term = InlineTerminal::new(TestBackend::new(70, 20), 0).unwrap();
    let mut ui = Ui::start(agent, Box::new(NoCommands), term, options);
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

fn row_with(ui: &Ui<TestBackend>, text: &str) -> String {
    everything(ui)
        .into_iter()
        .find(|r| r.contains(text))
        .unwrap_or_else(|| panic!("no row with {text:?}: {:#?}", everything(ui)))
}

fn send(ui: &mut Ui<TestBackend>, text: &str) {
    for c in text.chars() {
        ui.handle(Event::Key(KeyEvent::new(
            KeyCode::Char(c),
            KeyModifiers::NONE,
        )))
        .unwrap();
    }
    ui.handle(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)))
        .unwrap();
    ui.handle(Event::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )))
    .unwrap();
}

async fn settle(ui: &mut Ui<TestBackend>) {
    tokio::time::timeout(Duration::from_secs(10), ui.settle())
        .await
        .expect("the turn ends")
        .unwrap();
}

fn reply_with_usage(text: &str, input: u64, output: u64, cached: u64) -> Script {
    Script::Reply(vec![
        Ok(ProviderEvent::TextDelta(text.into())),
        Ok(ProviderEvent::Usage(Usage {
            input_tokens: input,
            output_tokens: output,
            cached_tokens: cached,
        })),
        Ok(ProviderEvent::Finished(FinishReason::Stop)),
    ])
}

#[test]
fn token_counts_are_short_in_the_status_line_and_exact_in_reports() {
    assert_eq!(status::tokens(950), "950");
    assert_eq!(status::tokens(1_234), "1.2k");
    assert_eq!(status::tokens(34_567), "34k");
    assert_eq!(status::tokens(1_500_000), "1.5M");
    assert_eq!(status::thousands(24_353), "24,353");
    assert_eq!(status::thousands(999), "999");
}

#[test]
fn the_status_line_shows_the_model_mode_context_and_tokens() {
    let theme = Theme::monochrome();
    let context = ContextUsage {
        window: 32_768,
        system: 1_000,
        tools: 1_000,
        messages: 1_000,
        total: 3_277,
    };
    let mut totals = Totals::default();
    totals.add(
        "mock/m",
        &Usage {
            input_tokens: 12_000,
            output_tokens: 1_100,
            cached_tokens: 0,
        },
    );
    let line = status::status_line("mock/m", Mode::Auto, &context, &totals, &theme);
    assert_eq!(
        plain(&line),
        "mock/m · auto · 11% of context · 12k in, 1.1k out"
    );
    let full = status::status_line("mock/m", Mode::FullAccess, &context, &totals, &theme);
    assert!(plain(&full).ends_with("· full-access: no sandbox, no approvals"));
}

#[test]
fn the_stats_line_shows_what_the_provider_reported() {
    let theme = Theme::monochrome();
    let line = status::stats_line("ollama/qwen", Some(400), 1_000, 1_000, 50, 800, &theme);
    assert_eq!(
        plain(&line),
        "ollama/qwen · first token 0.4s · 50 tok/s · cache 80.0%"
    );
    // No cached tokens reported: no cache rate.
    let line = status::stats_line("openai/x", Some(1_250), 2_000, 900, 100, 0, &theme);
    assert_eq!(plain(&line), "openai/x · first token 1.2s · 50 tok/s");
}

#[tokio::test]
async fn a_turn_on_a_local_model_ends_with_its_stats_and_updates_the_status_line() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![reply_with_usage("Done.", 1_000, 50, 800)]);
    let mut ui = start(provider, dir.path(), "system");
    assert!(row_with(&ui, "mock/m · auto").contains("0 in, 0 out"));
    send(&mut ui, "go");
    settle(&mut ui).await;
    let stats = row_with(&ui, "first token");
    assert!(stats.starts_with("mock/m · first token"), "{stats}");
    assert!(stats.ends_with("· cache 80.0%"), "{stats}");
    let status = row_with(&ui, "mock/m · auto");
    assert!(status.contains("1.0k in, 50 out"), "{status}");
    // The provider reported 1,000 input tokens: about 3% of the 32,768-token window.
    assert!(status.contains(" · 4% of context"), "{status}");
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn context_lists_each_instruction_file_and_the_free_space() {
    let dir = tempfile::tempdir().unwrap();
    // A system prompt holding a 2,000-token AGENTS.md.
    let system = format!("base prompt\n{}", "a".repeat(8_000));
    let mut ui = start(MockProvider::new(Vec::new()), dir.path(), &system);
    send(&mut ui, "/context");
    assert!(row_with(&ui, "Context window").contains("32,768 tokens (assumed)"));
    let agents = row_with(&ui, "AGENTS.md");
    assert!(agents.contains("2,000"), "{agents}");
    assert!(agents.contains("6.1%"), "{agents}");
    let prompt = row_with(&ui, "system prompt");
    assert!(prompt.contains(" 3 "), "{prompt}");
    assert!(row_with(&ui, "tool definitions").contains(" 1 "));
    let free = row_with(&ui, "free");
    assert!(free.contains("30,764"), "{free}");
    assert!(everything(&ui).iter().any(|r| r.contains("conversation")));
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn usage_shows_tokens_per_model() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        reply_with_usage("one", 1_000, 50, 800),
        reply_with_usage("two", 1_200, 70, 1_000),
    ]);
    let mut ui = start(provider, dir.path(), "system");
    send(&mut ui, "/usage");
    assert!(row_with(&ui, "No tokens used yet").contains("session"));
    send(&mut ui, "first");
    settle(&mut ui).await;
    send(&mut ui, "second");
    settle(&mut ui).await;
    send(&mut ui, "/usage");
    let header = row_with(&ui, "model");
    assert!(
        header.contains("input") && header.contains("cached"),
        "{header}"
    );
    let row = row_with(&ui, "2,200");
    let cells: Vec<&str> = row.split_whitespace().collect();
    assert_eq!(cells, ["mock/m", "2,200", "120", "1,800"]);
    ui.finish().await.unwrap();
}
