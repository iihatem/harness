//! Markdown, code and diffs as they appear on screen, drawn into ratatui's `TestBackend`.

use harness_tui::{diff, markdown, style::Theme, text};
use ratatui::{
    Terminal,
    backend::TestBackend,
    buffer::Buffer,
    style::Color,
    text::{Line, Span},
    widgets::Paragraph,
};

/// `lines` drawn on a screen `width` columns wide and as tall as they are.
fn draw(lines: Vec<Line<'static>>, width: u16) -> Buffer {
    let height = (lines.len() as u16).max(1);
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| frame.render_widget(Paragraph::new(lines), frame.area()))
        .unwrap();
    terminal.backend().buffer().clone()
}

/// Each row of `buffer` as text, without trailing spaces.
fn rows(buffer: &Buffer) -> Vec<String> {
    let width = buffer.area.width as usize;
    buffer
        .content
        .chunks(width)
        .map(|row| {
            let text: String = row.iter().map(|cell| cell.symbol()).collect();
            text.trim_end().to_string()
        })
        .collect()
}

fn colours(buffer: &Buffer) -> Vec<Color> {
    buffer
        .content
        .iter()
        .flat_map(|cell| [cell.fg, cell.bg])
        .filter(|c| *c != Color::Reset)
        .collect()
}

const SAMPLE: &str = "\
# Plan

Add a **rate limiter** to the login handler, so that repeated failures slow down.

- read `src/login.rs`
- change it:
  1. count failures
  2. sleep after three
> Keep the old behaviour behind a flag.

```rust
fn main() {}
```

| name | lines |
|------|-------|
| a.rs | 12 |

See [the docs](https://example.com/docs).
";

#[test]
fn markdown_renders_headings_lists_code_quotes_and_tables_at_a_width() {
    let buffer = draw(markdown::render(SAMPLE, 40, &Theme::colored()), 40);
    assert_eq!(
        rows(&buffer),
        [
            "Plan",
            "",
            "Add a rate limiter to the login handler,",
            "so that repeated failures slow down.",
            "",
            "- read src/login.rs",
            "- change it:",
            "  1. count failures",
            "  2. sleep after three",
            "",
            "│ Keep the old behaviour behind a flag.",
            "",
            "  fn main() {}",
            "",
            "name │ lines",
            "─────┼──────",
            "a.rs │ 12",
            "",
            "See the docs (https://example.com/docs).",
        ]
    );
}

#[test]
fn long_words_and_list_items_wrap_under_their_text() {
    let lines = markdown::render(
        "- one two three four five six seven\n- abcdefghijklmnopqrstuvwxyz",
        16,
        &Theme::colored(),
    );
    assert_eq!(
        rows(&draw(lines, 16)),
        [
            "- one two three",
            "  four five six",
            "  seven",
            "- abcdefghijklmn",
            "  opqrstuvwxyz",
        ]
    );
}

#[test]
fn code_blocks_are_highlighted_only_with_colour_and_a_known_language() {
    let code = "```rust\nlet x = \"hi\";\n```\n";
    let coloured = draw(markdown::render(code, 30, &Theme::colored()), 30);
    let distinct: std::collections::HashSet<_> = colours(&coloured).into_iter().collect();
    assert!(distinct.len() >= 2, "{distinct:?}");
    assert!(distinct.iter().all(|c| matches!(c, Color::Rgb(..))));
    // Without 24-bit colour, the nearest of the 256 standard colours.
    let theme = Theme::from_vars(None, None);
    let indexed = draw(markdown::render(code, 30, &theme), 30);
    assert!(
        colours(&indexed)
            .iter()
            .all(|c| matches!(c, Color::Indexed(_)))
    );
    // An unknown language is shown as it is.
    let unknown = draw(
        markdown::render("```nosuchlang\nlet x = 1;\n```\n", 30, &Theme::colored()),
        30,
    );
    assert!(colours(&unknown).is_empty());
    assert_eq!(rows(&unknown), ["  let x = 1;"]);
}

#[test]
fn no_color_draws_no_colour_and_keeps_code_and_diff_markers() {
    let theme = Theme::from_vars(Some("1"), Some("truecolor"));
    assert!(!theme.color);
    let buffer = draw(markdown::render(SAMPLE, 40, &theme), 40);
    assert!(colours(&buffer).is_empty(), "{:?}", colours(&buffer));
    assert!(rows(&buffer).contains(&"- read `src/login.rs`".to_string()));
    let diff = draw(diff::unified("a\nb\n", "a\nc\n", 1, &theme), 20);
    assert!(colours(&diff).is_empty());
    assert_eq!(rows(&diff), ["@@ -1,2 +1,2 @@", " a", "-b", "+c"]);
    // An empty NO_COLOR does not count.
    assert!(Theme::from_vars(Some(""), None).color);
}

#[test]
fn a_diff_shows_hunks_with_coloured_marked_lines() {
    let theme = Theme::colored();
    let old = "one\ntwo\nthree\nfour\nfive\nsix\nseven\n";
    let new = "one\n2\nthree\nfour\nfive\nsix\nseven\neight\n";
    let buffer = draw(diff::unified(old, new, 1, &theme), 20);
    assert_eq!(
        rows(&buffer),
        [
            "@@ -1,3 +1,3 @@",
            " one",
            "-two",
            "+2",
            " three",
            "@@ -7 +7,2 @@",
            " seven",
            "+eight",
        ]
    );
    let row = |y: u16| &buffer[(0, y)];
    assert_eq!(row(2).fg, Color::Red);
    assert_eq!(row(3).fg, Color::Green);
    assert_eq!(diff::counts(old, new), (2, 1));
    assert!(diff::unified("same\n", "same\n", 3, &theme).is_empty());
}

#[test]
fn control_characters_from_the_model_are_shown_escaped() {
    let buffer = draw(
        markdown::render(
            "evil \u{1b}[2J text \u{202e}reversed and a \u{7} bell\n\n```\n\u{1b}]0;title\u{7}\n```",
            60,
            &Theme::colored(),
        ),
        60,
    );
    let screen = rows(&buffer).join("\n");
    assert!(
        !screen.chars().any(|c| c.is_control() && c != '\n'),
        "{screen:?}"
    );
    assert!(
        screen.contains("evil \\u{1b}[2J text \\u{202e}reversed"),
        "{screen}"
    );
    assert!(screen.contains("\\u{1b}]0;title\\u{7}"), "{screen}");
    assert_eq!(text::sanitize("a\tb\r\nc"), "a    b\nc");
}

#[test]
fn wrapping_keeps_styles_and_prefixes() {
    let line = Line::from(vec![
        Span::styled(
            "red words ",
            ratatui::style::Style::default().fg(Color::Red),
        ),
        Span::raw("plain words here"),
    ]);
    let wrapped = text::wrap(&line, 12, &[Span::raw("> ")], &[Span::raw("  ")]);
    let texts: Vec<String> = wrapped.iter().map(text::plain).collect();
    assert_eq!(texts, ["> red words", "  plain", "  words here"]);
    assert_eq!(wrapped[0].spans[1].style.fg, Some(Color::Red));
    assert_eq!(text::width("日本"), 4);
}

// Review A's I3: ratatui draws a grapheme cluster (an emoji with VS16, a ZWJ sequence, a flag) as
// one character two columns wide. Measured by code point, such emoji counted one column or none,
// so wrapped rows came out wider than the screen and their ends were cut off.
#[test]
fn emoji_sequences_are_measured_and_wrapped_as_the_screen_draws_them() {
    assert_eq!(text::width("⚠️"), 2);
    assert_eq!(text::width("👨‍👩‍👧‍👦"), 2);
    assert_eq!(text::width("🇯🇵"), 2);
    assert_eq!(text::width("e\u{301}"), 1);
    assert_eq!(text::width("日本"), 4);
    let words: Vec<String> = (1..=40).map(|i| format!("w{i:02}")).collect();
    let source = format!("⚠️ ⚠️ ⚠️ ⚠️ ⚠️ ✔️ ❤️ {}", words.join(" "));
    let wrapped = text::wrap(&Line::from(source.clone()), 40, &[], &[]);
    for line in &wrapped {
        assert!(
            line.width() <= 40,
            "{:?} is {} wide",
            text::plain(line),
            line.width()
        );
    }
    // Nothing is cut when the rows are drawn.
    let shown = rows(&draw(wrapped, 40)).join(" ");
    assert_eq!(
        shown.split_whitespace().collect::<Vec<_>>(),
        source.split_whitespace().collect::<Vec<_>>()
    );
    // A cluster is never split between rows.
    let family = "👨‍👩‍👧‍👦".repeat(3);
    let wrapped = text::wrap(&Line::from(family), 4, &[], &[]);
    let texts: Vec<String> = wrapped.iter().map(text::plain).collect();
    assert_eq!(texts, ["👨‍👩‍👧‍👦👨‍👩‍👧‍👦", "👨‍👩‍👧‍👦"]);
    // Table columns line up.
    let table = markdown::render(
        "| sign | word |\n|---|---|\n| ⚠️⚠️ | warn |\n| 👍🏽 | fine |\n| ok | x |\n",
        40,
        &Theme::monochrome(),
    );
    let bars: Vec<usize> = table
        .iter()
        .map(|line| {
            let plain = text::plain(line);
            let before = plain.split(['│', '┼']).next().unwrap_or_default();
            text::width(before)
        })
        .collect();
    assert!(bars.windows(2).all(|w| w[0] == w[1]), "{bars:?}");
}

// Review D's M3: invisible format characters (zero-width spaces and joiners, the soft hyphen, the
// byte-order mark, tag characters, line separators) are not drawn, so a command, a path or a
// diff line could differ invisibly from what is shown. They are shown escaped, as control and
// bidirectional characters are, except where a script or an emoji needs them.
#[test]
fn invisible_format_characters_are_shown_escaped() {
    for (text, shown) in [
        ("rm\u{200b} -rf", "rm\\u{200b} -rf"),
        ("a\u{200c}b a\u{200d}b", "a\\u{200c}b a\\u{200d}b"),
        ("soft\u{ad}hyphen", "soft\\u{ad}hyphen"),
        ("\u{feff}bom \u{2060}wj", "\\u{feff}bom \\u{2060}wj"),
        ("tag\u{e0041}\u{e007f}", "tag\\u{e0041}\\u{e007f}"),
        ("line\u{2028}para\u{2029}", "line\\u{2028}para\\u{2029}"),
        ("\u{202e}rtl", "\\u{202e}rtl"),
    ] {
        assert_eq!(text::sanitize(text), shown);
    }
    // Emoji sequences, subdivision flags, and joiners in scripts that write with them are kept.
    for text in [
        "👨\u{200d}👩\u{200d}👧\u{200d}👦",
        "🏳\u{fe0f}\u{200d}🌈",
        "🏴\u{e0067}\u{e0062}\u{e0065}\u{e006e}\u{e0067}\u{e007f}",
        "می\u{200c}خواهم",
        "क्\u{200d}ष",
        "\u{0600}١٢",
    ] {
        assert_eq!(text::sanitize(text), text);
    }
}

// Review A's M6: highlighting costs about 0.15 ms a line and only a line's length was bounded,
// so a block of thousands of lines held the UI for a second when it was rendered. A block of more
// than 2,000 lines is shown plain.
#[test]
fn a_very_long_code_block_is_shown_plain() {
    let block = |lines: usize| {
        let code: String = (0..lines).map(|i| format!("let x{i} = {i};\n")).collect();
        markdown::render(&format!("```rust\n{code}```\n"), 40, &Theme::colored())
    };
    let long = block(2_001);
    assert_eq!(long.len(), 2_001);
    assert!(colours(&draw(long[..5].to_vec(), 40)).is_empty());
    let short = block(2_000);
    assert!(!colours(&draw(short[..5].to_vec(), 40)).is_empty());
}

// Review A's M4: each line of an HTML block was pushed onto the line before it, so a block of
// several lines ran together and was wrapped as one, and nothing separated it from what followed.
#[test]
fn html_blocks_keep_their_lines_and_the_space_around_them() {
    let lines = markdown::render(
        "before\n\n<details>\n<summary>x</summary>\n</details>\n\nafter para\n\n<br>\nmore\n\n<!-- a\n\nb -->",
        40,
        &Theme::monochrome(),
    );
    assert_eq!(
        lines.iter().map(text::plain).collect::<Vec<_>>(),
        [
            "before",
            "",
            "<details>",
            "<summary>x</summary>",
            "</details>",
            "",
            "after para",
            "",
            "<br>",
            "more",
            "",
            "<!-- a",
            "",
            "b -->",
        ]
    );
}

// Review A's M5: when the prefixes of nested quotes and lists took the whole width, every row still
// got one character after them and ran past the screen's edge, where it was cut. A prefix is cut
// to half the width.
#[test]
fn deep_nesting_never_makes_rows_wider_than_the_screen() {
    let lines = markdown::render("> > > > > > hello world again", 10, &Theme::monochrome());
    for line in &lines {
        assert!(line.width() <= 10, "{:?}", text::plain(line));
    }
    let text: String = lines
        .iter()
        .map(|l| text::plain(l).replace('│', ""))
        .collect::<Vec<_>>()
        .join(" ");
    assert_eq!(
        text.split_whitespace().collect::<Vec<_>>(),
        ["hello", "world", "again"]
    );
    assert!(text::plain(&lines[0]).starts_with("│ │ │"));
}
