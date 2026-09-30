//! The inline terminal and the transcript, on ratatui's `TestBackend`: finished lines go into the
//! terminal's scrollback, and only the live region is redrawn.

mod support;

use std::sync::{Arc, Mutex};

use harness_core::event::{AgentEvent, TurnEndReason};
use harness_tui::{inline::InlineTerminal, markdown, style::Theme, transcript::Transcript};
use ratatui::{
    backend::{Backend, ClearType, TestBackend, WindowSize},
    buffer::{Buffer, Cell},
    layout::{Position, Size},
    style::Color,
    text::Line,
    widgets::{Paragraph, Widget},
};
use support::vt::{Vt, VtBackend};

/// Each row of `buffer` as text, without trailing spaces.
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

fn text_rows(lines: &[Line<'_>]) -> Vec<String> {
    lines
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect()
}

/// Draws `text` as the live region, `height` rows tall, with the cursor after it.
fn draw_live(
    term: &mut InlineTerminal<impl Backend<Error: Send + Sync + 'static>>,
    text: &str,
    height: u16,
) {
    term.draw(height, |area, buf| {
        Paragraph::new(text.to_string()).render(area, buf);
        Some(Position::new(text.len() as u16, area.y))
    })
    .unwrap();
}

#[test]
fn earlier_lines_stay_reachable_in_the_terminals_scrollback() {
    let mut term = InlineTerminal::new(TestBackend::new(20, 5), 0).unwrap();
    for i in 1..=8 {
        term.insert(&[Line::from(format!("line {i}"))]).unwrap();
        draw_live(&mut term, "> ", 1);
    }
    let backend = term.backend();
    assert_eq!(
        rows(backend.scrollback()),
        ["line 1", "line 2", "line 3", "line 4"]
    );
    assert_eq!(
        rows(backend.buffer()),
        ["line 5", "line 6", "line 7", "line 8", ">"]
    );
    assert_eq!(backend.cursor_position(), Position::new(2, 4));
}

#[test]
fn the_live_region_grows_and_shrinks_below_the_last_line() {
    let mut term = InlineTerminal::new(TestBackend::new(10, 5), 0).unwrap();
    term.insert(&[Line::from("one"), Line::from("two")])
        .unwrap();
    draw_live(&mut term, "a", 1);
    term.draw(3, |area, buf| {
        Paragraph::new("a\nb\nc").render(area, buf);
        None
    })
    .unwrap();
    assert_eq!(rows(term.backend().buffer()), ["one", "two", "a", "b", "c"]);
    draw_live(&mut term, "a", 1);
    assert_eq!(rows(term.backend().buffer()), ["one", "two", "a", "", ""]);
    term.backend().assert_scrollback_empty();
    // Taller than the room left: the lines above scroll into scrollback.
    term.draw(5, |area, buf| {
        Paragraph::new("1\n2\n3\n4\n5").render(area, buf);
        None
    })
    .unwrap();
    assert_eq!(rows(term.backend().scrollback()), ["one", "two"]);
    assert_eq!(rows(term.backend().buffer()), ["1", "2", "3", "4", "5"]);
    assert_eq!(term.top(), 0);
}

#[test]
fn clearing_leaves_the_finished_lines_and_the_cursor_below_them() {
    let mut term = InlineTerminal::new(TestBackend::new(10, 5), 1).unwrap();
    term.insert(&[Line::from("done")]).unwrap();
    draw_live(&mut term, "> typing", 2);
    term.clear().unwrap();
    assert_eq!(rows(term.backend().buffer()), ["", "done", "", "", ""]);
    assert_eq!(term.backend().cursor_position(), Position::new(0, 2));
}

#[test]
fn after_a_resize_the_live_region_stays_on_screen() {
    let mut term = InlineTerminal::new(TestBackend::new(20, 10), 8).unwrap();
    draw_live(&mut term, "> ", 2);
    term.backend_mut().resize(20, 5);
    term.resized().unwrap();
    draw_live(&mut term, "> ", 2);
    assert_eq!(term.top(), 3);
    assert_eq!(term.backend().cursor_position(), Position::new(2, 3));
}

// Review A's C1: the column hidden under a wide character was written too, which pushed the rest
// of the row one column right per wide character, off its end, and out of the scrollback.
#[test]
fn wide_characters_keep_their_text_and_width_in_the_scrollback() {
    let lines = [
        "日本語のテキストです",
        "ok 🙂 fine 中文 end",
        "return '日本語'",
        "a line after them",
    ];
    let mut term = InlineTerminal::new(VtBackend::new(Vt::new(20, 4)), 0).unwrap();
    for line in lines {
        term.insert(&[Line::from(line)]).unwrap();
        draw_live(&mut term, "> ", 1);
    }
    let vt = term.backend().vt();
    assert_eq!(vt.scrollback(), &lines[..1]);
    assert_eq!(vt.screen(), [&lines[1..], &[">"]].concat());
    assert_eq!(vt.widths(), [20, 19, 15, 17, 1]);
}

/// Draws a live region as tall as the screen, as a long reply streams, then inserts the reply's
/// `lines`.
fn a_screen_tall_reply_finishes(lines: usize) -> InlineTerminal<VtBackend> {
    let mut term = InlineTerminal::new(VtBackend::new(Vt::new(20, 8)), 0).unwrap();
    term.draw(8, |area, buf| {
        let tail: Vec<String> = (1..=8).map(|i| format!("tail {i}")).collect();
        Paragraph::new(tail.join("\n")).render(area, buf);
        None
    })
    .unwrap();
    let reply: Vec<Line> = (1..=lines)
        .map(|i| Line::from(format!("reply {i}")))
        .collect();
    term.insert(&reply).unwrap();
    term
}

// Review A's C2: inserting reserved the old live region's height, so after a reply that filled
// the screen, the whole reply scrolled off and the screen was left blank.
#[test]
fn after_a_screen_tall_live_region_the_end_of_the_reply_stays_on_screen() {
    let mut term = a_screen_tall_reply_finishes(12);
    draw_live(&mut term, "> ", 2);
    {
        let vt = term.backend().vt();
        let replies = |range: std::ops::RangeInclusive<usize>| -> Vec<String> {
            range.map(|i| format!("reply {i}")).collect()
        };
        assert_eq!(vt.scrollback(), replies(1..=6));
        assert_eq!(
            vt.screen(),
            [replies(7..=12), vec![">".into(), "".into()]].concat()
        );
    }
    // Leaving keeps the end of the reply too, with the cursor on the row under it.
    term.clear().unwrap();
    let vt = term.backend().vt();
    assert_eq!(vt.screen()[5], "reply 12");
    assert_eq!(vt.screen()[6], "");
    assert_eq!(vt.cursor(), Position::new(0, 6));
    // And so does leaving right after the reply, before the input is drawn again, with more
    // lines inserted first.
    drop(vt);
    let mut term = a_screen_tall_reply_finishes(12);
    term.insert(&[Line::from("after")]).unwrap();
    term.clear().unwrap();
    let vt = term.backend().vt();
    let everything = vt.everything();
    assert_eq!(
        everything.iter().filter(|r| r.starts_with("reply")).count(),
        12
    );
    assert!(
        !everything.iter().any(|r| r.starts_with("tail")),
        "{everything:#?}"
    );
    assert_eq!(vt.screen()[5..], ["reply 12", "after", ""]);
    assert_eq!(vt.cursor(), Position::new(0, 7));
}

/// A terminal 20 wide and `rows` tall, full of a shell's lines, with harness below them.
fn under_a_full_screen(rows: u16) -> InlineTerminal<VtBackend> {
    let mut vt = Vt::new(20, rows);
    for i in 0..rows + 3 {
        vt.print(&format!("shell {i}\n"));
    }
    let top = vt.cursor().y;
    let mut term = InlineTerminal::new(VtBackend::new(vt), top).unwrap();
    term.insert(&[Line::from("done 1"), Line::from("done 2")])
        .unwrap();
    draw_live(&mut term, "> ", 2);
    term
}

/// What harness drew is on screen once, and the live region is right below it.
fn in_place(term: &InlineTerminal<VtBackend>, rows: u16) {
    let vt = term.backend().vt();
    let everything = vt.everything();
    for line in (0..rows + 3)
        .map(|i| format!("shell {i}"))
        .chain(["done 1".into(), "done 2".into()])
    {
        let count = everything.iter().filter(|r| **r == line).count();
        assert_eq!(count, 1, "{line}: {everything:#?}");
    }
    let screen = vt.screen();
    let input = screen
        .iter()
        .position(|r| r == ">")
        .expect("the live region");
    assert_eq!(screen[input - 1], "done 2", "{screen:#?}");
    assert_eq!(term.top() as usize, input);
}

#[test]
fn after_a_resize_the_live_region_is_found_where_the_terminal_moved_it() {
    // Shorter, as xterm does it: the rows above the cursor go into scrollback.
    let mut term = under_a_full_screen(10);
    term.backend().vt().resize(20, 6);
    term.resized().unwrap();
    draw_live(&mut term, "> ", 2);
    in_place(&term, 10);
    // Taller, with rows coming back from scrollback: the terminal says where its cursor went.
    term.backend().vt().grow_from_scrollback(12);
    term.resized().unwrap();
    draw_live(&mut term, "> ", 2);
    in_place(&term, 10);
    // A terminal that cannot say where its cursor is is taken to do what xterm does.
    let mut term = under_a_full_screen(10).without_cursor_reports();
    term.backend().vt().resize(20, 6);
    term.resized().unwrap();
    draw_live(&mut term, "> ", 2);
    in_place(&term, 10);
    term.backend().vt().resize(30, 9);
    term.resized().unwrap();
    draw_live(&mut term, "> ", 2);
    in_place(&term, 10);
}

// Review A's M3: harness starts on the row after the cursor. When the shell had left the cursor
// mid-line on the bottom row, that row was clamped back onto the cursor's, and its text was
// cleared; when the terminal did not say where its cursor was, harness started at the top of the
// screen and cleared all of it.
#[test]
fn starting_never_draws_over_the_users_rows() {
    let mut vt = Vt::new(20, 4);
    vt.print("a\nb\nc\n$ partial");
    let row = vt.cursor().y;
    let mut term = InlineTerminal::new(VtBackend::new(vt), row + 1).unwrap();
    draw_live(&mut term, "> ", 2);
    assert_eq!(
        term.backend().vt().everything(),
        ["a", "b", "c", "$ partial", ">", ""]
    );
    // Where the cursor is is not known: harness starts below the bottom row.
    let mut vt = Vt::new(20, 4);
    vt.print("a\nb\nc\nd");
    let mut term = InlineTerminal::new(VtBackend::new(vt), u16::MAX)
        .unwrap()
        .without_cursor_reports();
    draw_live(&mut term, "> ", 2);
    assert_eq!(
        term.backend().vt().everything(),
        ["a", "b", "c", "d", ">", ""]
    );
}

/// A `TestBackend` that records the rows of every cell it is asked to draw.
struct Recording {
    inner: TestBackend,
    drawn: Arc<Mutex<Vec<(u16, u16)>>>,
}

impl Backend for Recording {
    type Error = core::convert::Infallible;

    fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        let cells: Vec<_> = content.collect();
        self.drawn
            .lock()
            .unwrap()
            .extend(cells.iter().map(|(x, y, _)| (*x, *y)));
        self.inner.draw(cells.into_iter())
    }
    fn append_lines(&mut self, n: u16) -> Result<(), Self::Error> {
        self.inner.append_lines(n)
    }
    fn hide_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.hide_cursor()
    }
    fn show_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.show_cursor()
    }
    fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
        self.inner.get_cursor_position()
    }
    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> Result<(), Self::Error> {
        self.inner.set_cursor_position(position)
    }
    fn clear(&mut self) -> Result<(), Self::Error> {
        self.inner.clear()
    }
    fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
        self.inner.clear_region(clear_type)
    }
    fn size(&self) -> Result<Size, Self::Error> {
        self.inner.size()
    }
    fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
        self.inner.window_size()
    }
    fn flush(&mut self) -> Result<(), Self::Error> {
        self.inner.flush()
    }
}

#[test]
fn a_redraw_writes_only_the_live_cells_that_changed() {
    let drawn = Arc::new(Mutex::new(Vec::new()));
    let backend = Recording {
        inner: TestBackend::new(20, 6),
        drawn: drawn.clone(),
    };
    let mut term = InlineTerminal::new(backend, 0).unwrap();
    term.insert(&[Line::from("history")]).unwrap();
    draw_live(&mut term, "> hello", 1);
    drawn.lock().unwrap().clear();
    draw_live(&mut term, "> hellO", 1);
    assert_eq!(*drawn.lock().unwrap(), [(6, 1)]);
    // Nothing above the live region is touched by redraws.
    drawn.lock().unwrap().clear();
    draw_live(&mut term, "> other text", 1);
    assert!(drawn.lock().unwrap().iter().all(|(_, y)| *y == 1));
}

fn event_lines(events: &[AgentEvent], width: usize) -> (Transcript, Vec<String>) {
    let mut transcript = Transcript::new(Theme::monochrome());
    for event in events {
        transcript.on_event(event, width);
    }
    let finished = transcript.take_finished();
    let text = text_rows(&finished);
    (transcript, text)
}

#[test]
fn a_tool_using_turn_becomes_lines_for_the_scrollback() {
    let mut transcript = Transcript::new(Theme::monochrome());
    transcript.push_user("run the tests", 40);
    let events = [
        AgentEvent::TurnStarted,
        AgentEvent::ToolCallRequested {
            id: "c1".into(),
            name: "bash".into(),
            arguments: r#"{"command":"cargo test"}"#.into(),
        },
        AgentEvent::ToolCallFinished {
            id: "c1".into(),
            output: "exit code 0\nok\n".into(),
            is_error: false,
        },
        AgentEvent::TextDelta {
            text: "All **tests**".into(),
        },
    ];
    for event in &events {
        transcript.on_event(event, 40);
    }
    assert!(transcript.busy());
    // The reply streams in the live region until it is complete.
    assert_eq!(text_rows(&transcript.live(40, 5)), ["All tests"]);
    for event in [
        AgentEvent::AssistantMessage {
            content: "All **tests** pass.".into(),
            model: "mock/m".into(),
        },
        AgentEvent::TurnFinished {
            reason: TurnEndReason::Completed,
        },
    ] {
        transcript.on_event(&event, 40);
    }
    assert!(!transcript.busy());
    assert!(transcript.live(40, 5).is_empty());
    assert_eq!(
        text_rows(&transcript.take_finished()),
        [
            "› run the tests",
            "",
            "● $ cargo test",
            "  exit code 0",
            "  ok",
            "",
            "All tests pass.",
        ]
    );
}

#[test]
fn an_edit_shows_what_it_replaced_and_long_output_is_cut_short() {
    let output: String = (1..=10).map(|i| format!("line {i}\n")).collect();
    let (_, lines) = event_lines(
        &[
            AgentEvent::ToolCallRequested {
                id: "e".into(),
                name: "edit".into(),
                arguments:
                    r#"{"path":"src/a.rs","old_string":"let x = 1;","new_string":"let x = 2;"}"#
                        .into(),
            },
            AgentEvent::ToolCallFinished {
                id: "e".into(),
                output: "edited src/a.rs".into(),
                is_error: false,
            },
            AgentEvent::ToolCallRequested {
                id: "b".into(),
                name: "bash".into(),
                arguments: r#"{"command":"seq 10"}"#.into(),
            },
            AgentEvent::ToolCallFinished {
                id: "b".into(),
                output,
                is_error: false,
            },
        ],
        40,
    );
    assert_eq!(
        lines,
        [
            "● edit src/a.rs",
            "  -let x = 1;",
            "  +let x = 2;",
            "",
            "● $ seq 10",
            "  line 1",
            "  line 2",
            "  line 3",
            "  line 4",
            "  line 5",
            "  line 6",
            "  … 4 more lines",
        ]
    );
}

// Review A's M1: output was cut to six lines before it was wrapped, so one long line (minified
// code, a JSON blob) filled hundreds of rows of the scrollback. It is cut to six rows.
#[test]
fn a_long_line_of_tool_output_is_cut_to_six_rows() {
    let (_, lines) = event_lines(
        &[
            AgentEvent::ToolCallRequested {
                id: "b".into(),
                name: "bash".into(),
                arguments: r#"{"command":"cat min.js"}"#.into(),
            },
            AgentEvent::ToolCallFinished {
                id: "b".into(),
                output: format!("{}\nsecond line", "x".repeat(3_000)),
                is_error: false,
            },
        ],
        40,
    );
    assert_eq!(lines.len(), 8, "{lines:#?}");
    assert_eq!(lines[0], "● $ cat min.js");
    assert!(
        lines[1..7]
            .iter()
            .all(|l| l == &format!("  {}", "x".repeat(38)))
    );
    assert_eq!(lines[7], "  … 74 more lines");
}

#[test]
fn the_live_region_shows_the_block_still_streaming_and_the_running_tool() {
    let mut transcript = Transcript::new(Theme::monochrome());
    transcript.on_event(
        &AgentEvent::TextDelta {
            text: "one\n\ntwo\n\nthree\n\nfour and".into(),
        },
        20,
    );
    // The paragraphs that are complete are finished lines already.
    assert_eq!(
        text_rows(&transcript.take_finished()),
        ["one", "", "two", "", "three"]
    );
    transcript.on_event(
        &AgentEvent::TextDelta {
            text: " more\nwords to wrap".into(),
        },
        20,
    );
    assert_eq!(
        text_rows(&transcript.live(20, 2)),
        ["four and more words", "to wrap"]
    );
    assert_eq!(text_rows(&transcript.live(20, 1)), ["to wrap"]);
    transcript.on_event(
        &AgentEvent::ToolCallRequested {
            id: "c".into(),
            name: "read".into(),
            arguments: r#"{"path":"README.md"}"#.into(),
        },
        20,
    );
    assert_eq!(
        text_rows(&transcript.live(20, 2)),
        ["to wrap", "● read README.md"]
    );
}

#[test]
fn interrupted_and_failed_turns_say_so_and_keep_partial_output() {
    let (_, lines) = event_lines(
        &[
            AgentEvent::TurnStarted,
            AgentEvent::TextDelta {
                text: "partial".into(),
            },
            AgentEvent::TurnFinished {
                reason: TurnEndReason::Interrupted,
            },
        ],
        30,
    );
    assert_eq!(lines, ["partial", "interrupted"]);
    let (_, lines) = event_lines(
        &[
            AgentEvent::Error {
                kind: harness_core::event::ErrorKind::Provider,
                message: "HTTP 500: \u{1b}[31mboom".into(),
            },
            AgentEvent::TurnFinished {
                reason: TurnEndReason::StepLimit,
            },
        ],
        60,
    );
    assert_eq!(
        lines,
        [
            "error: HTTP 500: \\u{1b}[31mboom",
            "error: stopped after reaching the step limit",
        ]
    );
}

const REPLY: &str = "# Title

First paragraph with **bold** text.

- one
- two

  still two

Second paragraph.
```rust
fn main() {}

let x = 1;
```
After the code.

| a | b |
|---|---|
| 1 | 2 |

> quote

Last line.";

fn coloured(lines: &[Line<'_>]) -> bool {
    lines
        .iter()
        .flat_map(|l| &l.spans)
        .any(|s| matches!(s.style.fg, Some(Color::Rgb(..) | Color::Indexed(_))))
}

// Review A's I1: every redraw rendered and highlighted the whole reply so far, so each chunk of a
// long reply cost more than the last. Blocks now go into the scrollback as they complete,
// rendered once, and the live region shows only the block still growing.
#[test]
fn a_streaming_reply_goes_into_the_scrollback_block_by_block() {
    let theme = Theme::colored();
    let mut transcript = Transcript::new(theme);
    transcript.on_event(&AgentEvent::TurnStarted, 40);
    let mut streamed = Vec::new();
    for chunk in REPLY.as_bytes().chunks(5) {
        let text = std::str::from_utf8(chunk).unwrap().to_string();
        transcript.on_event(&AgentEvent::TextDelta { text }, 40);
        streamed.extend(transcript.take_finished());
    }
    let before_the_end = streamed.len();
    transcript.on_event(
        &AgentEvent::AssistantMessage {
            content: REPLY.into(),
            model: "mock/m".into(),
        },
        40,
    );
    streamed.extend(transcript.take_finished());
    // All but the last paragraph was in the scrollback before the reply ended...
    assert_eq!(text_rows(&streamed[before_the_end..]), ["", "Last line."]);
    // ...and it is the reply rendered whole, code highlighted.
    assert_eq!(streamed, markdown::render(REPLY, 40, &theme));
    assert!(coloured(&streamed));
}

#[test]
fn an_open_code_block_streams_plain_and_is_highlighted_once_it_closes() {
    let mut transcript = Transcript::new(Theme::colored());
    transcript.on_event(
        &AgentEvent::TextDelta {
            text: "```rust\nlet x = 1;\nlet y = 2;\n".into(),
        },
        40,
    );
    let live = transcript.live(40, 10);
    assert_eq!(text_rows(&live), ["  let x = 1;", "  let y = 2;"]);
    assert!(!coloured(&live));
    assert!(transcript.take_finished().is_empty());
    // A long block shows its last lines only.
    let more: String = (1..=1_000).map(|i| format!("let v{i} = {i};\n")).collect();
    transcript.on_event(&AgentEvent::TextDelta { text: more }, 40);
    assert_eq!(
        text_rows(&transcript.live(40, 2)),
        ["  let v999 = 999;", "  let v1000 = 1000;"]
    );
    transcript.on_event(
        &AgentEvent::TextDelta {
            text: "```\nafter".into(),
        },
        40,
    );
    let finished = transcript.take_finished();
    assert_eq!(finished.len(), 1_002);
    assert_eq!(text_rows(&finished[..2]), ["  let x = 1;", "  let y = 2;"]);
    assert!(coloured(&finished));
    assert_eq!(text_rows(&transcript.live(40, 10)), ["after"]);
}
