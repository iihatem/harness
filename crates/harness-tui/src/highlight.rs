//! Syntax highlighting for code blocks, with syntect's bundled syntaxes and theme. They are
//! loaded the first time a code block needs them.

use std::sync::OnceLock;

use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use syntect::{
    easy::HighlightLines,
    highlighting::{FontStyle, Theme as SyntaxTheme, ThemeSet},
    parsing::SyntaxSet,
    util::LinesWithEndings,
};

use crate::{style::Theme, text::sanitize};

/// Lines longer than this are not highlighted: syntect's regular expressions can take a long
/// time on them.
const MAX_LINE: usize = 2_000;
/// Code blocks with more lines than this are not highlighted: at about 0.15 ms a line, it would
/// hold the UI for longer than a third of a second.
const MAX_LINES: usize = 2_000;

struct Assets {
    syntaxes: SyntaxSet,
    theme: SyntaxTheme,
}

fn assets() -> &'static Assets {
    static ASSETS: OnceLock<Assets> = OnceLock::new();
    ASSETS.get_or_init(|| {
        let mut themes = ThemeSet::load_defaults();
        Assets {
            syntaxes: SyntaxSet::load_defaults_newlines(),
            theme: themes
                .themes
                .remove("base16-ocean.dark")
                .unwrap_or_default(),
        }
    })
}

/// `code` highlighted as `language`, the word after a code fence's backticks (`rust`, `py`,
/// `sh`, ...), one line per line of code. `None` when the theme has no colour, the language is
/// unknown, or the code has too many lines, or one too long, to highlight.
pub fn highlight(code: &str, language: &str, theme: &Theme) -> Option<Vec<Line<'static>>> {
    if !theme.color
        || language.is_empty()
        || code.lines().nth(MAX_LINES).is_some()
        || code.lines().any(|l| l.len() > MAX_LINE)
    {
        return None;
    }
    let assets = assets();
    let syntax = assets.syntaxes.find_syntax_by_token(language)?;
    let mut highlighter = HighlightLines::new(syntax, &assets.theme);
    let mut out = Vec::new();
    for line in LinesWithEndings::from(code) {
        let ranges = highlighter.highlight_line(line, &assets.syntaxes).ok()?;
        let spans = ranges
            .into_iter()
            .map(|(style, text)| {
                let text = sanitize(text.trim_end_matches(['\n', '\r']));
                let mut out = Style::default();
                let fg = style.foreground;
                if let Some(color) = theme.rgb(fg.r, fg.g, fg.b) {
                    out = out.fg(color);
                }
                if style.font_style.contains(FontStyle::BOLD) {
                    out = out.add_modifier(Modifier::BOLD);
                }
                if style.font_style.contains(FontStyle::ITALIC) {
                    out = out.add_modifier(Modifier::ITALIC);
                }
                Span::styled(text, out)
            })
            .filter(|span| !span.content.is_empty())
            .collect::<Vec<_>>();
        out.push(Line::from(spans));
    }
    Some(out)
}
