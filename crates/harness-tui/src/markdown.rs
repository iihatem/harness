//! Markdown, as the model writes it, rendered as styled lines of a given width: paragraphs,
//! headings, emphasis, inline code, fenced code blocks with syntax highlighting, lists, block
//! quotes, links, tables and rules. Everything drawn is sanitized first. A reply that is still
//! streaming is split into the blocks that are complete, rendered once, and the block still
//! growing ([`Stream`]).

use std::borrow::Cow;

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};

use crate::{
    highlight::highlight,
    style::Theme,
    text::{sanitize, width as text_width, wrap},
};

/// `markdown` as lines at most `width` columns wide.
pub fn render(markdown: &str, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    render_with(markdown, width, theme, true)
}

/// `markdown` as lines at most `width` columns wide, with code blocks not highlighted: for text
/// that is drawn again and again as it grows.
pub fn render_plain(markdown: &str, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    render_with(markdown, width, theme, false)
}

fn render_with(markdown: &str, width: usize, theme: &Theme, highlight: bool) -> Vec<Line<'static>> {
    let options =
        Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS;
    let mut renderer = Renderer {
        theme,
        highlight,
        width: width.max(8),
        out: Vec::new(),
        line: Vec::new(),
        styles: Vec::new(),
        containers: Vec::new(),
        code: None,
        table: None,
        links: Vec::new(),
        needs_blank: false,
    };
    for event in Parser::new_ext(markdown, options) {
        renderer.event(event);
    }
    renderer.flush();
    renderer.out
}

enum Container {
    Quote,
    List { next: Option<u64> },
    Item { marker: String, first: bool },
}

struct Code {
    language: String,
    text: String,
}

#[derive(Default)]
struct Table {
    rows: Vec<Vec<String>>,
    row: Vec<String>,
    cell: String,
    head_rows: usize,
}

struct Renderer<'t> {
    theme: &'t Theme,
    /// Code blocks are highlighted.
    highlight: bool,
    width: usize,
    out: Vec<Line<'static>>,
    /// The line being built.
    line: Vec<Span<'static>>,
    /// Inline styles that apply now, innermost last.
    styles: Vec<Style>,
    containers: Vec<Container>,
    code: Option<Code>,
    table: Option<Table>,
    /// For each open link: its address, and where its text starts in `line`.
    links: Vec<(String, usize)>,
    /// A blank line goes before the next block.
    needs_blank: bool,
}

impl Renderer<'_> {
    fn style(&self) -> Style {
        self.styles
            .iter()
            .fold(Style::default(), |acc, s| acc.patch(*s))
    }

    fn push(&mut self, text: &str, style: Style) {
        let text = sanitize(text).replace('\n', " ");
        if text.is_empty() {
            return;
        }
        if let Some(table) = &mut self.table {
            table.cell.push_str(&text);
            return;
        }
        self.line.push(Span::styled(text, style));
    }

    /// The prefixes of the first line and of later lines, from the enclosing quotes and lists.
    fn prefixes(&self) -> (Vec<Span<'static>>, Vec<Span<'static>>) {
        let mut first = Vec::new();
        let mut rest = Vec::new();
        for container in &self.containers {
            match container {
                Container::Quote => {
                    first.push(Span::styled("│ ", self.theme.quote()));
                    rest.push(Span::styled("│ ", self.theme.quote()));
                }
                Container::List { .. } => {}
                Container::Item {
                    marker,
                    first: at_start,
                } => {
                    let blank = " ".repeat(text_width(marker));
                    if *at_start {
                        first.push(Span::styled(marker.clone(), self.theme.accent()));
                    } else {
                        first.push(Span::raw(blank.clone()));
                    }
                    rest.push(Span::raw(blank));
                }
            }
        }
        (first, rest)
    }

    fn items_started(&mut self) {
        for container in &mut self.containers {
            if let Container::Item { first, .. } = container {
                *first = false;
            }
        }
    }

    /// Wraps and emits the line being built, if any.
    fn flush(&mut self) {
        if self.line.is_empty() {
            return;
        }
        let spans = std::mem::take(&mut self.line);
        let (first, rest) = self.prefixes();
        self.out
            .extend(wrap(&Line::from(spans), self.width, &first, &rest));
        self.items_started();
    }

    /// Starts a block: a blank line after the previous one.
    fn block(&mut self) {
        self.flush();
        if self.needs_blank && !self.out.is_empty() {
            let (_, rest) = self.prefixes();
            let quotes: Vec<Span<'static>> = rest
                .into_iter()
                .filter(|s| s.content.trim() == "│")
                .collect();
            self.out.push(Line::from(quotes));
        }
        self.needs_blank = false;
    }

    fn in_item(&self) -> bool {
        self.containers
            .iter()
            .any(|c| matches!(c, Container::Item { .. }))
    }

    fn event(&mut self, event: Event<'_>) {
        if let Some(code) = &mut self.code {
            match event {
                Event::Text(text) => code.text.push_str(&text),
                Event::End(TagEnd::CodeBlock) => self.end_code(),
                _ => {}
            }
            return;
        }
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.push(&text, self.style()),
            Event::Code(code) | Event::InlineMath(code) | Event::DisplayMath(code) => {
                let style = self.style().patch(self.theme.code());
                if self.theme.color {
                    self.push(&code, style);
                } else {
                    self.push(&format!("`{code}`"), style);
                }
            }
            // An HTML block comes a line at a time, each with its line break.
            Event::Html(html) => {
                for (i, line) in html.split('\n').enumerate() {
                    if i > 0 {
                        self.flush();
                    }
                    self.push(line, self.theme.dim());
                }
            }
            Event::InlineHtml(html) => self.push(&html, self.theme.dim()),
            Event::FootnoteReference(label) => self.push(&format!("[^{label}]"), self.style()),
            Event::SoftBreak => self.push(" ", self.style()),
            Event::HardBreak => self.flush(),
            Event::Rule => {
                self.block();
                let (first, _) = self.prefixes();
                let used: usize = first.iter().map(|s| text_width(&s.content)).sum();
                let mut spans = first;
                spans.push(Span::styled(
                    "─".repeat(self.width.saturating_sub(used).min(40)),
                    self.theme.dim(),
                ));
                self.out.push(Line::from(spans));
                self.needs_blank = true;
            }
            Event::TaskListMarker(done) => {
                self.push(if done { "[x] " } else { "[ ] " }, self.theme.dim())
            }
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => self.block(),
            Tag::Heading { level, .. } => {
                self.block();
                let style = if matches!(level, HeadingLevel::H1 | HeadingLevel::H2) {
                    self.theme.heading()
                } else {
                    self.theme.bold()
                };
                self.styles.push(style);
            }
            Tag::BlockQuote(_) => {
                self.block();
                self.containers.push(Container::Quote);
            }
            Tag::CodeBlock(kind) => {
                self.block();
                let language = match kind {
                    CodeBlockKind::Fenced(info) => {
                        info.split([' ', ',', '{']).next().unwrap_or("").to_string()
                    }
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some(Code {
                    language,
                    text: String::new(),
                });
            }
            Tag::List(start) => {
                if self.in_item() {
                    self.flush();
                } else {
                    self.block();
                }
                self.containers.push(Container::List { next: start });
            }
            Tag::Item => {
                self.flush();
                let marker = match self.containers.last_mut() {
                    Some(Container::List { next: Some(n) }) => {
                        let marker = format!("{n}. ");
                        *n += 1;
                        marker
                    }
                    _ => "- ".to_string(),
                };
                self.containers.push(Container::Item {
                    marker,
                    first: true,
                });
            }
            Tag::Table(_) => {
                self.block();
                self.table = Some(Table::default());
            }
            Tag::HtmlBlock => self.block(),
            Tag::TableHead | Tag::TableRow | Tag::TableCell => {}
            Tag::Emphasis => self.styles.push(self.theme.italic()),
            Tag::Strong => self.styles.push(self.theme.bold()),
            Tag::Strikethrough => self
                .styles
                .push(Style::default().add_modifier(Modifier::CROSSED_OUT)),
            Tag::Link { dest_url, .. } => {
                self.links.push((dest_url.to_string(), self.line.len()));
                self.styles.push(self.theme.link());
            }
            Tag::Image { dest_url, .. } => {
                self.push("[image: ", self.theme.dim());
                self.links.push((dest_url.to_string(), self.line.len()));
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.flush();
                self.needs_blank = true;
            }
            TagEnd::Heading(_) => {
                self.styles.pop();
                self.flush();
                self.needs_blank = true;
            }
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.containers.pop();
                self.needs_blank = true;
            }
            TagEnd::List(_) => {
                self.flush();
                self.containers.pop();
                if !self.in_item() {
                    self.needs_blank = true;
                }
            }
            TagEnd::Item => {
                self.flush();
                self.containers.pop();
            }
            TagEnd::TableCell => {
                if let Some(table) = &mut self.table {
                    let cell = std::mem::take(&mut table.cell);
                    table.row.push(cell.trim().to_string());
                }
            }
            TagEnd::TableHead => {
                if let Some(table) = &mut self.table {
                    let row = std::mem::take(&mut table.row);
                    table.rows.push(row);
                    table.head_rows = table.rows.len();
                }
            }
            TagEnd::TableRow => {
                if let Some(table) = &mut self.table {
                    let row = std::mem::take(&mut table.row);
                    table.rows.push(row);
                }
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    self.end_table(table);
                }
                self.needs_blank = true;
            }
            TagEnd::HtmlBlock => {
                self.flush();
                self.needs_blank = true;
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.styles.pop();
            }
            TagEnd::Link => {
                self.styles.pop();
                if let Some((url, start)) = self.links.pop() {
                    let text: String = self.line[start.min(self.line.len())..]
                        .iter()
                        .map(|s| s.content.as_ref())
                        .collect();
                    if !url.is_empty() && text != url && format!("mailto:{text}") != url {
                        self.push(&format!(" ({url})"), self.theme.dim());
                    }
                }
            }
            TagEnd::Image => {
                if let Some((url, _)) = self.links.pop() {
                    self.push(&format!("] ({url})"), self.theme.dim());
                }
            }
            _ => {}
        }
    }

    fn end_code(&mut self) {
        let Some(code) = self.code.take() else {
            return;
        };
        let text = code.text.strip_suffix('\n').unwrap_or(&code.text);
        let highlighted = if self.highlight {
            highlight(text, &code.language, self.theme)
        } else {
            None
        };
        let lines = highlighted.unwrap_or_else(|| {
            text.split('\n')
                .map(|l| Line::from(Span::styled(sanitize(l), self.theme.plain())))
                .collect()
        });
        let (first, rest) = self.prefixes();
        let indent = Span::raw("  ");
        for (i, line) in lines.iter().enumerate() {
            let mut first_prefix = if i == 0 { first.clone() } else { rest.clone() };
            first_prefix.push(indent.clone());
            let mut rest_prefix = rest.clone();
            rest_prefix.push(indent.clone());
            self.out
                .extend(wrap(line, self.width, &first_prefix, &rest_prefix));
        }
        self.items_started();
        self.needs_blank = true;
    }

    fn end_table(&mut self, table: Table) {
        let columns = table.rows.iter().map(Vec::len).max().unwrap_or(0);
        if columns == 0 {
            return;
        }
        let mut widths = vec![0; columns];
        for row in &table.rows {
            for (i, cell) in row.iter().enumerate() {
                widths[i] = widths[i].max(text_width(cell));
            }
        }
        let (first, rest) = self.prefixes();
        let prefix_width: usize = rest.iter().map(|s| text_width(&s.content)).sum();
        let total: usize = widths.iter().sum::<usize>() + 3 * (columns - 1);
        let fits = prefix_width + total <= self.width;
        for (r, row) in table.rows.iter().enumerate() {
            let style = if r < table.head_rows {
                self.theme.bold()
            } else {
                self.theme.plain()
            };
            let mut spans = Vec::new();
            for (i, cell) in row.iter().enumerate() {
                if i > 0 {
                    spans.push(Span::styled(" │ ", self.theme.dim()));
                }
                let pad = if fits && i + 1 < row.len() {
                    widths[i] - text_width(cell)
                } else {
                    0
                };
                spans.push(Span::styled(format!("{cell}{}", " ".repeat(pad)), style));
            }
            let lead = if r == 0 { &first } else { &rest };
            self.out
                .extend(wrap(&Line::from(spans), self.width, lead, &rest));
            if fits && r + 1 == table.head_rows {
                let rule: Vec<String> = widths.iter().map(|w| "─".repeat(*w)).collect();
                let mut spans = rest.clone();
                spans.push(Span::styled(rule.join("─┼─"), self.theme.dim()));
                self.out.push(Line::from(spans));
            }
        }
        self.items_started();
    }
}

/// A reply as it streams in, split into the Markdown blocks that are complete, which can be
/// rendered once and go into the scrollback, and the block still growing, which the live region
/// shows.
///
/// A block is complete once the next one starts: at a line that is not indented after a blank
/// line (an indented one may continue a list item), at a fenced code block's closing fence, or at
/// a fence that opens without indentation (a code block ends the paragraph, list or quote before
/// it). Blank lines inside a fenced code block end nothing, and a list item after a blank line
/// goes on the list before it, so a loose list is numbered as a whole.
#[derive(Debug, Default)]
pub struct Stream {
    text: String,
    /// Where the blocks not yet taken start.
    taken: usize,
    /// Where the complete blocks end.
    complete: usize,
    /// Where the next line to look at starts: lines are looked at once they end.
    scanned: usize,
    /// The last line looked at was blank.
    after_blank: bool,
    /// The last line looked at without indentation was a list item.
    in_list: bool,
    /// The fenced code block open at `scanned`.
    fence: Option<OpenFence>,
}

#[derive(Debug)]
struct OpenFence {
    /// Where its first line of code starts.
    content: usize,
    marker: char,
    len: usize,
    /// It opened without indentation, so its closing fence ends a block.
    top: bool,
}

impl Stream {
    /// Adds `delta` to the reply.
    pub fn push(&mut self, delta: &str) {
        self.text.push_str(delta);
        while let Some(end) = self.text[self.scanned..].find('\n') {
            let start = self.scanned;
            let end = start + end;
            self.line(start, end);
            self.scanned = end + 1;
        }
        // A line that has begun without indentation after a blank one starts a block, whatever
        // follows on it, unless it may be an item of the list before it.
        let begun = self.text[self.scanned..].chars().next();
        let may_be_item = |c: char| matches!(c, '-' | '*' | '+') || c.is_ascii_digit();
        if self.fence.is_none()
            && self.after_blank
            && begun.is_some_and(|c| !c.is_whitespace() && !(self.in_list && may_be_item(c)))
        {
            self.complete = self.scanned;
        }
    }

    /// Looks at the line `start..end`, which has ended.
    fn line(&mut self, start: usize, end: usize) {
        let line = &self.text[start..end];
        if let Some(open) = &self.fence {
            if closes(line, open) {
                if open.top {
                    self.complete = end + 1;
                }
                self.fence = None;
            }
            self.after_blank = false;
            return;
        }
        if line.trim().is_empty() {
            self.after_blank = true;
            return;
        }
        let opened = opens_fence(line);
        let indented = line.starts_with(char::is_whitespace);
        let item = list_item(line);
        let list_goes_on = item && self.in_list && opened.is_none();
        if !indented && (self.after_blank || opened.is_some()) && !list_goes_on {
            self.complete = start;
        }
        if !indented {
            self.in_list = item;
        }
        if let Some((marker, len, indent)) = opened {
            self.fence = Some(OpenFence {
                content: end + 1,
                marker,
                len,
                top: indent == 0,
            });
        }
        self.after_blank = false;
    }

    /// The blocks that became complete since they were last taken, which are then taken.
    pub fn take_complete(&mut self) -> Option<&str> {
        if self.complete <= self.taken {
            return None;
        }
        let taken = self.taken;
        self.taken = self.complete;
        Some(&self.text[taken..self.complete])
    }

    /// What has not been taken: the block still growing.
    pub fn rest(&self) -> &str {
        &self.text[self.taken..]
    }

    /// The Markdown the live region draws for the rest, cut short in an open code block to its
    /// last `lines` lines, so the cost of drawing it does not grow with the block.
    pub fn live(&self, lines: usize) -> Cow<'_, str> {
        let Some(open) = &self.fence else {
            return Cow::Borrowed(self.rest());
        };
        let code = &self.text[open.content..];
        match code.rmatch_indices('\n').nth(lines) {
            Some((cut, _)) => Cow::Owned(format!(
                "{}{}",
                &self.text[self.taken..open.content],
                &code[cut + 1..]
            )),
            None => Cow::Borrowed(self.rest()),
        }
    }

    /// The reply ended: what of it has not been taken, and the stream starts again. `message` is
    /// the reply as a whole, when it came: its end after what was taken, as long as it starts with
    /// that, and all of it otherwise.
    pub fn finish(&mut self, message: Option<&str>) -> String {
        let taken = &self.text[..self.taken];
        let rest = match message {
            Some(message) => message.strip_prefix(taken).unwrap_or(message),
            None => &self.text[self.taken..],
        }
        .to_string();
        *self = Stream::default();
        rest
    }
}

/// A fence of three or more backticks or tildes, indented by at most three spaces: its
/// character, length and indentation.
fn fence(line: &str) -> Option<(char, usize, usize)> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let marker = rest.chars().next().filter(|c| matches!(c, '`' | '~'))?;
    let len = rest.len() - rest.trim_start_matches(marker).len();
    (len >= 3).then_some((marker, len, indent))
}

/// The fence `line` opens a code block with. A backtick fence's info string has no backticks
/// (`` ``` `` followed by more backticks on the line is inline code).
fn opens_fence(line: &str) -> Option<(char, usize, usize)> {
    let (marker, len, indent) = fence(line)?;
    let info = &line[indent + len..];
    (marker == '~' || !info.contains('`')).then_some((marker, len, indent))
}

/// Whether `line` starts a list item: `-`, `*` or `+`, or a number and `.` or `)`, then a space
/// or the end of the line.
fn list_item(line: &str) -> bool {
    let digits = line.len() - line.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    let marker = match digits {
        0 => line.strip_prefix(['-', '*', '+']),
        1..=9 => line[digits..].strip_prefix(['.', ')']),
        _ => None,
    };
    marker.is_some_and(|rest| rest.is_empty() || rest.starts_with([' ', '\t']))
}

/// Whether `line` closes the code block `open` began.
fn closes(line: &str, open: &OpenFence) -> bool {
    fence(line).is_some_and(|(marker, len, indent)| {
        marker == open.marker && len >= open.len && line[indent + len..].trim().is_empty()
    })
}
