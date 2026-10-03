//! The picker: a list to choose from, filtered as the user types, drawn in a full-screen view
//! on the terminal's alternate screen, which gives the inline screen back as it was.

use std::time::{Duration, Instant};

use harness_tui::{
    inline::InlineTerminal,
    input::Timed,
    picker::{Item, Picked, Picker, choose},
    style::Theme,
    terminal::{AltScreen, CrosstermAltScreen},
    testing::TestAltScreen,
};
use ratatui::{
    backend::{CrosstermBackend, TestBackend},
    buffer::Buffer,
    crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers},
    layout::Rect,
    text::Line,
};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
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

/// `picker` drawn on a `width` by `height` screen.
fn drawn(picker: &Picker, width: u16, height: u16) -> Vec<String> {
    let area = Rect::new(0, 0, width, height);
    let mut buffer = Buffer::empty(area);
    picker.render(area, &mut buffer, &Theme::monochrome());
    rows(&buffer)
}

fn models() -> Picker {
    let items = [
        ("ollama/qwen3-coder:30b", "local"),
        ("openai/gpt-5", ""),
        ("anthropic/claude-sonnet-4-5", ""),
        ("chatgpt/gpt-5-codex", "signed in"),
    ]
    .map(|(label, detail)| Item::new(label, detail));
    Picker::new("Choose a model", items.to_vec())
}

#[test]
fn a_picker_lists_its_items_and_chooses_with_enter() {
    let mut picker = models();
    let screen = drawn(&picker, 60, 12);
    assert_eq!(screen[0], "Choose a model");
    assert!(
        screen
            .iter()
            .any(|r| r.contains("ollama/qwen3-coder:30b") && r.contains("local")),
        "{screen:#?}"
    );
    assert!(screen.last().unwrap().contains("Enter"), "{screen:#?}");
    assert_eq!(picker.key(key(KeyCode::Down)), None);
    assert_eq!(picker.key(key(KeyCode::Enter)), Some(Picked::Chosen(1)));
}

#[test]
fn typing_filters_the_list() {
    let mut picker = models();
    for c in "sonnet".chars() {
        assert_eq!(picker.key(key(KeyCode::Char(c))), None);
    }
    let screen = drawn(&picker, 60, 12);
    assert!(screen.iter().any(|r| r.contains("sonnet")), "{screen:#?}");
    assert!(!screen.iter().any(|r| r.contains("gpt-5")), "{screen:#?}");
    assert_eq!(picker.key(key(KeyCode::Enter)), Some(Picked::Chosen(2)));
    // Nothing matches: Enter chooses nothing.
    let mut picker = models();
    for c in "zzz".chars() {
        picker.key(key(KeyCode::Char(c)));
    }
    assert!(
        drawn(&picker, 60, 12)
            .iter()
            .any(|r| r.contains("nothing matches")),
    );
    assert_eq!(picker.key(key(KeyCode::Enter)), None);
    // Backspace widens it again.
    for _ in 0..3 {
        picker.key(key(KeyCode::Backspace));
    }
    assert_eq!(picker.key(key(KeyCode::Enter)), Some(Picked::Chosen(0)));
}

#[test]
fn esc_cancels() {
    let mut picker = models();
    picker.key(key(KeyCode::Down));
    assert_eq!(picker.key(key(KeyCode::Esc)), Some(Picked::Cancelled));
}

#[test]
fn a_long_list_scrolls_with_the_selection() {
    let items: Vec<Item> = (0..50)
        .map(|i| Item::new(&format!("item {i:02}"), ""))
        .collect();
    let mut picker = Picker::new("Many", items);
    picker.key(key(KeyCode::PageDown));
    picker.key(key(KeyCode::PageDown));
    let selected = picker.selected().unwrap();
    assert!(selected > 5, "{selected}");
    let screen = drawn(&picker, 40, 12);
    let label = format!("item {selected:02}");
    assert!(screen.iter().any(|r| r.contains(&label)), "{screen:#?}");
    picker.key(key(KeyCode::End));
    assert_eq!(picker.selected(), Some(49));
    assert!(drawn(&picker, 40, 12).iter().any(|r| r.contains("item 49")));
    picker.key(key(KeyCode::Home));
    assert_eq!(picker.selected(), Some(0));
    // Up from the top stays there.
    picker.key(key(KeyCode::Up));
    assert_eq!(picker.selected(), Some(0));
}

#[test]
fn the_footer_and_the_starting_item_are_shown() {
    let picker = models()
        .with_footer(vec![
            "Rewinding restores files in the workspace only.".into(),
        ])
        .with_selected(3);
    assert_eq!(picker.selected(), Some(3));
    let screen = drawn(&picker, 60, 12);
    assert!(
        screen
            .iter()
            .any(|r| r.contains("Rewinding restores files in the workspace only.")),
        "{screen:#?}"
    );
}

#[test]
fn a_picker_can_wait_for_its_items() {
    let mut picker = Picker::loading("Choose a model", "looking for models…");
    assert!(
        drawn(&picker, 60, 12)
            .iter()
            .any(|r| r.contains("looking for models…"))
    );
    assert_eq!(picker.key(key(KeyCode::Enter)), None);
    picker.set_items(vec![Item::new("ollama/llama3", "")]);
    assert_eq!(picker.key(key(KeyCode::Enter)), Some(Picked::Chosen(0)));
}

#[test]
fn the_full_screen_view_gives_the_inline_screen_back_as_it_was() {
    let (alt, log) = TestAltScreen::new();
    let mut term = InlineTerminal::new(TestBackend::new(40, 10), 0)
        .unwrap()
        .with_alt_screen(Box::new(alt));
    term.insert(&[Line::from("earlier output"), Line::from("more output")])
        .unwrap();
    term.draw(2, |area, buf| {
        buf.set_line(area.x, area.y, &Line::from("› typing"), area.width);
        buf.set_line(area.x, area.y + 1, &Line::from("status"), area.width);
        None
    })
    .unwrap();
    let screen = term.backend().buffer().clone();
    let scrollback = term.backend().scrollback().clone();

    let picker = models();
    term.draw_full(|area, buf| {
        picker.render(area, buf, &Theme::monochrome());
        None
    })
    .unwrap();
    assert!(term.in_full_screen());
    let full = rows(term.backend().buffer());
    assert_eq!(full[0], "Choose a model");
    assert!(
        !full.iter().any(|r| r.contains("earlier output")),
        "{full:#?}"
    );
    // Drawing again writes only what changed, and stays on the alternate screen.
    term.draw_full(|area, buf| {
        picker.render(area, buf, &Theme::monochrome());
        None
    })
    .unwrap();

    term.leave_full().unwrap();
    assert!(!term.in_full_screen());
    assert_eq!(term.backend().buffer(), &screen);
    assert_eq!(term.backend().scrollback(), &scrollback);
    assert_eq!(*log.lock().unwrap(), ["enter", "leave"]);
    // The live region is drawn again where it was.
    term.draw(2, |area, buf| {
        buf.set_line(area.x, area.y, &Line::from("› typing more"), area.width);
        buf.set_line(area.x, area.y + 1, &Line::from("status"), area.width);
        None
    })
    .unwrap();
    let after = rows(term.backend().buffer());
    assert_eq!(after[0], "earlier output");
    assert_eq!(after[2], "› typing more");
}

#[test]
fn without_an_alternate_screen_the_live_region_starts_again_at_the_top() {
    let mut term = InlineTerminal::new(TestBackend::new(40, 10), 0).unwrap();
    term.insert(&[Line::from("earlier output")]).unwrap();
    let picker = models();
    term.draw_full(|area, buf| {
        picker.render(area, buf, &Theme::monochrome());
        None
    })
    .unwrap();
    term.leave_full().unwrap();
    term.draw(1, |area, buf| {
        buf.set_line(area.x, area.y, &Line::from("› back"), area.width);
        None
    })
    .unwrap();
    let after = rows(term.backend().buffer());
    assert_eq!(after[0], "› back");
    assert!(after[1..].iter().all(|r| r.is_empty()), "{after:#?}");
}

/// What a terminal is sent.
#[derive(Clone, Default)]
struct Sent(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Sent {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn the_terminal_switches_screens_with_1049() {
    let sent = Sent::default();
    let mut backend = CrosstermBackend::new(sent.clone());
    CrosstermAltScreen::default().enter(&mut backend).unwrap();
    CrosstermAltScreen::default().leave(&mut backend).unwrap();
    assert_eq!(
        String::from_utf8_lossy(&sent.0.lock().unwrap()),
        "\u{1b}[?1049h\u{1b}[?1049l"
    );
}

// Review Focus: the terminal is resized while a picker is open. The view is drawn anew at the
// new size, and closing it gives back the inline screen with the live region on it.
#[test]
fn a_resize_while_a_picker_is_open_redraws_it_and_the_inline_screen_after() {
    let (alt, _log) = TestAltScreen::new();
    let mut term = InlineTerminal::new(TestBackend::new(40, 10), 0)
        .unwrap()
        .with_alt_screen(Box::new(alt));
    term.insert(&[Line::from("earlier output")]).unwrap();
    term.draw(1, |area, buf| {
        buf.set_line(area.x, area.y, &Line::from("› typing"), area.width);
        None
    })
    .unwrap();
    let picker = models();
    let draw = |term: &mut InlineTerminal<TestBackend>| {
        term.draw_full(|area, buf| picker.render(area, buf, &Theme::monochrome()))
            .unwrap()
    };
    draw(&mut term);
    term.backend_mut().resize(30, 6);
    term.resized().unwrap();
    draw(&mut term);
    let full = rows(term.backend().buffer());
    assert_eq!(full.len(), 6);
    assert_eq!(full[0], "Choose a model");
    assert!(full[5].contains("Enter"), "{full:#?}");
    term.leave_full().unwrap();
    term.draw(1, |area, buf| {
        buf.set_line(area.x, area.y, &Line::from("› typing"), area.width);
        None
    })
    .unwrap();
    let after = rows(term.backend().buffer());
    assert_eq!(after.len(), 6);
    assert!(after.iter().any(|r| r == "› typing"), "{after:#?}");
}

// Review Focus: a picker on a terminal a few rows high still shows the selected item and
// takes keys.
#[test]
fn a_picker_on_a_tiny_terminal_still_shows_the_selection() {
    let mut picker = models();
    picker.key(key(KeyCode::End));
    for height in 1..=4 {
        let screen = drawn(&picker, 20, height);
        assert_eq!(screen.len(), height as usize);
    }
    let screen = drawn(&picker, 40, 5);
    assert!(
        screen.iter().any(|r| r.contains("chatgpt/gpt-5-codex")),
        "{screen:#?}"
    );
    assert_eq!(picker.key(key(KeyCode::Enter)), Some(Picked::Chosen(3)));
}

/// Keys as the terminal's reader gives them, each read `after` the picker was shown (the first
/// at least a pause after it), so the picker takes them.
fn keys(
    codes: &[(KeyCode, KeyModifiers)],
) -> impl futures::Stream<Item = std::io::Result<Timed>> + Unpin {
    keys_after(Duration::from_secs(1), codes)
}

fn keys_after(
    after: Duration,
    codes: &[(KeyCode, KeyModifiers)],
) -> impl futures::Stream<Item = std::io::Result<Timed>> + Unpin {
    let base = Instant::now() + after;
    futures::stream::iter(
        codes
            .iter()
            .enumerate()
            .map(|(i, (code, modifiers))| {
                Ok(Timed {
                    event: Event::Key(KeyEvent::new(*code, *modifiers)),
                    at: base + Duration::from_millis(i as u64),
                })
            })
            .collect::<Vec<_>>(),
    )
}

// The first-run model choice: a picker on its own, before the session starts.
#[tokio::test]
async fn a_picker_on_its_own_returns_the_choice_and_gives_the_screen_back() {
    let (alt, log) = TestAltScreen::new();
    let mut term = InlineTerminal::new(TestBackend::new(60, 12), 0)
        .unwrap()
        .with_alt_screen(Box::new(alt));
    term.insert(&[Line::from("$ harness")]).unwrap();
    let chosen = choose(
        &mut term,
        keys(&[
            (KeyCode::Char('g'), KeyModifiers::NONE),
            (KeyCode::Char('p'), KeyModifiers::NONE),
            (KeyCode::Char('t'), KeyModifiers::NONE),
            (KeyCode::Down, KeyModifiers::NONE),
            (KeyCode::Enter, KeyModifiers::NONE),
        ]),
        models(),
        &Theme::monochrome(),
    )
    .await
    .unwrap();
    assert_eq!(chosen, Some(3));
    assert_eq!(*log.lock().unwrap(), ["enter", "leave"]);
    assert!(!term.in_full_screen());
    assert_eq!(rows(term.backend().buffer())[0], "$ harness");
}

// Keys read before the picker had been quiet for a pause were typed ahead of it: they choose
// nothing, and the picker waits for a pause after the last of them.
#[tokio::test]
async fn keys_typed_ahead_of_a_picker_on_its_own_choose_nothing() {
    let mut term = InlineTerminal::new(TestBackend::new(60, 12), 0)
        .unwrap()
        .with_alt_screen(Box::new(TestAltScreen::new().0));
    let chosen = choose(
        &mut term,
        keys_after(
            Duration::ZERO,
            &[
                (KeyCode::Enter, KeyModifiers::NONE),
                (KeyCode::Esc, KeyModifiers::NONE),
            ],
        ),
        models(),
        &Theme::monochrome(),
    )
    .await
    .unwrap();
    // Nothing chose or closed it: the events ended.
    assert_eq!(chosen, None);
}

#[tokio::test]
async fn esc_ctrl_c_or_the_end_of_input_choose_nothing() {
    for script in [
        vec![(KeyCode::Esc, KeyModifiers::NONE)],
        vec![(KeyCode::Char('c'), KeyModifiers::CONTROL)],
        vec![(KeyCode::Down, KeyModifiers::NONE)],
    ] {
        let mut term = InlineTerminal::new(TestBackend::new(60, 12), 0)
            .unwrap()
            .with_alt_screen(Box::new(TestAltScreen::new().0));
        let chosen = choose(&mut term, keys(&script), models(), &Theme::monochrome())
            .await
            .unwrap();
        assert_eq!(chosen, None);
        assert!(!term.in_full_screen());
    }
}

// Review B I1: the item for a model says whether it is the current one or one a ChatGPT plan
// includes, in the model picker and the first-run list alike.
#[test]
fn model_items_mark_the_current_model_and_chatgpt_ones() {
    use harness_tui::picker::{Item, model_item};
    assert_eq!(
        model_item("ollama/llama3", false),
        Item::new("ollama/llama3", "")
    );
    assert_eq!(
        model_item("ollama/llama3", true),
        Item::new("ollama/llama3", "(current)")
    );
    assert_eq!(
        model_item("chatgpt/gpt-5", false),
        Item::new("chatgpt/gpt-5", "ChatGPT plan")
    );
    assert_eq!(
        model_item("chatgpt/gpt-5", true),
        Item::new("chatgpt/gpt-5", "(current) ChatGPT plan")
    );
}
