//! Rough word splitting for input the parser rejected (or that is too long to
//! parse). Used only to look for denied or destructive commands, never to allow.

use std::mem::take;

use crate::argv::{Tok, decode_ansi_c};

/// Here-documents nested in substitutions inside here-document bodies deeper than this
/// are not tracked.
const MAX_HEREDOC_DEPTH: usize = 8;
/// A here-document delimiter is looked for this far at most.
const MAX_DELIMITER_CHARS: usize = 1024;

const KEYWORDS: &[&str] = &[
    "!", "{", "}", "if", "then", "else", "elif", "fi", "do", "done", "while", "until", "for", "in",
    "case", "esac", "select", "function", "time", "coproc",
];

/// A command found by the rough scan.
pub(crate) struct Rough {
    pub words: Vec<Tok>,
    /// Found in here-document text, which is data rather than commands (only the
    /// substitutions in a body with an unquoted delimiter run).
    pub data: bool,
}

/// Splits `src` into rough commands: quotes are removed (`$'…'` decoded), operators and
/// parentheses end a command, and `$(…)`/backtick bodies become commands of their
/// own. Leading keywords and `NAME=value` words are dropped. Here-document bodies are
/// split into [`Rough::data`] commands where the scan can tell, as bash would, where
/// they start and end; from the first here-document where it cannot, the text is split
/// as program text, and line by line as well.
pub(crate) fn rough_commands(src: &str) -> Vec<Rough> {
    Splitter::new(src, Text::Program, 0).split()
}

/// Like [`rough_commands`], for the body of a here-document with an unquoted delimiter.
pub(crate) fn rough_heredoc(body: &str) -> Vec<Rough> {
    Splitter::new(body, Text::Body { live: true }, 1).split()
}

/// Rough nesting depth of brackets and compound-command keywords, quotes ignored.
/// Checked before parsing: the parser recurses once per level, so deeply nested
/// input could otherwise exhaust the stack.
pub(crate) fn rough_nesting(src: &str) -> usize {
    let (mut depth, mut max) = (0usize, 0usize);
    for word in src.split(|c: char| c.is_whitespace() || ";&|".contains(c)) {
        let opens = word.matches(['(', '{']).count()
            + usize::from(matches!(
                word,
                "if" | "while" | "until" | "for" | "case" | "select"
            ));
        let closes =
            word.matches([')', '}']).count() + usize::from(matches!(word, "fi" | "done" | "esac"));
        depth += opens;
        max = max.max(depth);
        depth = depth.saturating_sub(closes);
    }
    max
}

/// The kind of text being split.
#[derive(Clone, Copy, PartialEq)]
enum Text {
    Program,
    /// A here-document body: its quotes are literal, and its substitutions run (and
    /// are program text) only when `live`, that is when the delimiter is unquoted.
    Body {
        live: bool,
    },
}

/// A here-document whose body starts after the current line.
struct Heredoc {
    delimiter: String,
    quoted: bool,
    strip_tabs: bool,
    /// The substitution depth of its operator; its body starts after a newline at that
    /// depth.
    level: usize,
}

/// An open `${…}` or `$[…]`: a `<<` in it is text, and a newline in it does not start
/// here-document bodies.
struct Brace {
    /// `}` or `]`.
    close: char,
    /// The quote it was opened in.
    quote: Option<char>,
    /// Open brackets of a `$[…]`, its own included.
    brackets: usize,
}

struct Splitter {
    src: Vec<char>,
    at: usize,
    commands: Vec<Rough>,
    words: Vec<Tok>,
    word: String,
    in_word: bool,
    /// The word has `$'…'` text that cannot be decoded.
    undecodable: bool,
    quote: Option<char>,
    text: Text,
    /// Inside `$((…))`, where `<<` is a shift.
    arithmetic: bool,
    /// Open `((…))` arithmetic commands.
    arithmetic_commands: usize,
    heredocs: Vec<Heredoc>,
    /// Open `${…}`/`$[…]` expansions of the current substitution.
    braces: Vec<Brace>,
    /// Whether here-documents are tracked at all.
    track: bool,
    /// The start of the line where here-document tracking stopped. From there, text that
    /// bash reads as here-document data may be split as program text, where a quote in it
    /// could hide a later command, so that text is also split line by line.
    lost: Option<usize>,
    /// Here-document bodies this text is nested in.
    depth: usize,
    /// Outer contexts of the open `$(…)`/backtick substitutions.
    stack: Vec<Frame>,
}

struct Frame {
    words: Vec<Tok>,
    word: String,
    undecodable: bool,
    quote: Option<char>,
    text: Text,
    arithmetic: bool,
    braces: Vec<Brace>,
    opener: char,
    parens: usize,
}

impl Splitter {
    fn new(src: &str, text: Text, depth: usize) -> Self {
        Splitter {
            src: src.chars().collect(),
            at: 0,
            commands: Vec::new(),
            words: Vec::new(),
            word: String::new(),
            in_word: false,
            undecodable: false,
            quote: None,
            text,
            arithmetic: false,
            arithmetic_commands: 0,
            heredocs: Vec::new(),
            braces: Vec::new(),
            track: true,
            lost: None,
            depth,
            stack: Vec::new(),
        }
    }

    fn split(mut self) -> Vec<Rough> {
        while let Some(c) = self.next() {
            match self.text {
                Text::Program => self.program(c),
                Text::Body { live } => self.body(c, live),
            }
        }
        while !self.stack.is_empty() {
            self.close();
        }
        self.end_command();
        if let Some(from) = self.lost {
            let rest: String = self.src[from..].iter().collect();
            for line in rest.split('\n') {
                let mut line = Splitter::new(line, self.text, self.depth);
                line.track = false;
                self.commands.extend(line.split());
            }
        }
        let dropped = |w: &Tok| {
            w.lit()
                .is_some_and(|w| KEYWORDS.contains(&w) || is_assignment(w))
        };
        self.commands
            .into_iter()
            .filter_map(|mut cmd| {
                let skip = cmd.words.iter().take_while(|w| dropped(w)).count();
                cmd.words.drain(..skip);
                (!cmd.words.is_empty()).then_some(cmd)
            })
            .collect()
    }

    fn next(&mut self) -> Option<char> {
        let c = self.src.get(self.at).copied();
        self.at += usize::from(c.is_some());
        c
    }

    fn peek(&self) -> Option<char> {
        self.src.get(self.at).copied()
    }

    fn next_if_eq(&mut self, c: char) -> bool {
        let next = self.peek() == Some(c);
        self.at += usize::from(next);
        next
    }

    fn push(&mut self, c: char) {
        self.word.push(c);
        self.in_word = true;
    }

    fn program(&mut self, c: char) {
        if self.quote != Some('\'') {
            if c == '`' {
                let closes =
                    self.quote.is_none() && self.stack.last().is_some_and(|f| f.opener == '`');
                if closes {
                    self.close()
                } else {
                    self.open('`')
                }
                return;
            }
            if c == '$' && self.next_if_eq('(') {
                return self.open_substitution();
            }
            self.expansion(c);
        }
        match self.quote {
            Some('\'') if c == '\'' => self.quote = None,
            Some('\'') => self.word.push(c),
            Some(_) if c == '"' => self.quote = None,
            Some(_) if c == '\\' => {
                if let Some(n) = self.next() {
                    self.word.push(n);
                }
            }
            Some(_) => self.word.push(c),
            None => match c {
                '$' if self.next_if_eq('\'') => self.ansi_c(),
                // `$"…"` is a double-quoted string.
                '$' if self.peek() == Some('"') => self.in_word = true,
                '\'' | '"' => {
                    self.quote = Some(c);
                    self.in_word = true;
                }
                '\\' => {
                    if let Some(n) = self.next().filter(|&n| n != '\n') {
                        self.push(n);
                    }
                }
                '#' if !self.in_word => {
                    while self.peek().is_some_and(|n| n != '\n') {
                        self.at += 1;
                    }
                }
                '<' if self.peek() == Some('<') => {
                    self.end_word();
                    self.redirection();
                }
                ' ' | '\t' | '<' | '>' => self.end_word(),
                '(' => {
                    if self.peek() == Some('(') {
                        self.arithmetic_commands += 1;
                    }
                    if let Some(f) = self.stack.last_mut() {
                        f.parens += 1;
                    }
                    self.end_command();
                }
                ')' => {
                    if self.peek() == Some(')') {
                        self.arithmetic_commands = self.arithmetic_commands.saturating_sub(1);
                    }
                    let closes = self
                        .stack
                        .last()
                        .is_some_and(|f| f.opener == '(' && f.parens == 0);
                    if closes {
                        self.close();
                    } else {
                        if let Some(f) = self.stack.last_mut() {
                            f.parens = f.parens.saturating_sub(1);
                        }
                        self.end_command();
                    }
                }
                '\n' => {
                    self.end_command();
                    if self.braces.is_empty() {
                        self.heredoc_bodies();
                    }
                }
                ';' | '&' | '|' | '{' | '}' => self.end_command(),
                _ => self.push(c),
            },
        }
    }

    /// Follows `${…}` and `$[…]` in program text outside single quotes.
    fn expansion(&mut self, c: char) {
        let quote = self.quote;
        match c {
            '$' if matches!(self.peek(), Some('{' | '[')) => {
                let close = if self.peek() == Some('{') { '}' } else { ']' };
                self.braces.push(Brace {
                    close,
                    quote,
                    brackets: 0,
                });
            }
            // bash versions disagree on whether single quotes quote directly inside a
            // double-quoted `${…}`, so where it ends is unknown.
            '\'' if self.braces.last().is_some_and(|b| b.quote == Some('"')) => {
                self.lose_track();
            }
            '[' | ']' | '}' => {
                let Some(b) = self.braces.last_mut().filter(|b| b.quote == quote) else {
                    return;
                };
                match (c, b.close) {
                    ('[', ']') => b.brackets += 1,
                    (']', ']') => {
                        b.brackets = b.brackets.saturating_sub(1);
                        if b.brackets == 0 {
                            self.braces.pop();
                        }
                    }
                    ('}', '}') => {
                        self.braces.pop();
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    /// A character of here-document text.
    fn body(&mut self, c: char, live: bool) {
        match c {
            '\\' if live => match self.peek() {
                Some('\n') => self.at += 1,
                Some(n @ ('$' | '`' | '\\')) => {
                    self.at += 1;
                    self.push(n);
                }
                _ => self.push(c),
            },
            '`' if live => self.open('`'),
            '$' if live && self.next_if_eq('(') => self.open_substitution(),
            ' ' | '\t' | '<' | '>' => self.end_word(),
            ';' | '&' | '|' | '\n' | '(' | ')' | '{' | '}' => self.end_command(),
            _ => self.push(c),
        }
    }

    /// Reads the rest of a `$'…'` string (its `$'` is consumed) into the word.
    fn ansi_c(&mut self) {
        let mut raw = String::new();
        while let Some(c) = self.next() {
            match c {
                '\'' => break,
                '\\' => {
                    raw.push(c);
                    raw.extend(self.next());
                }
                _ => raw.push(c),
            }
        }
        match decode_ansi_c(&raw) {
            Some(text) => self.word.push_str(&text),
            None => self.undecodable = true,
        }
        self.in_word = true;
    }

    /// After a `<` followed by another: a here-string (`<<<`), a shift in arithmetic or
    /// text in `${…}`/`$[…]`, or a here-document whose body follows the current line. The
    /// delimiter word itself is left to be read as a word.
    fn redirection(&mut self) {
        self.at += 1;
        if self.next_if_eq('<') || self.arithmetic || !self.braces.is_empty() {
            return;
        }
        // bash reads backtick text only when it runs it, and may end `((…))` elsewhere.
        let trusted = self.arithmetic_commands == 0
            && self.depth < MAX_HEREDOC_DEPTH
            && !self.stack.iter().any(|f| f.opener == '`');
        match self.delimiter() {
            Some(doc) if trusted && self.track && self.lost.is_none() => self.heredocs.push(doc),
            _ => self.lose_track(),
        }
    }

    /// The here-document whose `<<` was just read, if its delimiter is plain text or plain
    /// text in one pair of quotes: bash versions read other spellings (`$'…'`, backslashes,
    /// partial quoting, expansions) differently.
    fn delimiter(&self) -> Option<Heredoc> {
        let strip_tabs = self.peek() == Some('-');
        let mut i = self.at + usize::from(strip_tabs);
        while matches!(self.src.get(i), Some(' ' | '\t')) {
            i += 1;
        }
        let quote = self.src.get(i).copied().filter(|&c| c == '\'' || c == '"');
        i += usize::from(quote.is_some());
        let start = i;
        while self.src.get(i).is_some_and(|&c| is_plain(c)) && i - start < MAX_DELIMITER_CHARS {
            i += 1;
        }
        let delimiter: String = self.src[start..i].iter().collect();
        if let Some(q) = quote {
            if self.src.get(i) != Some(&q) {
                return None;
            }
            i += 1;
        }
        let ends = self.src.get(i).is_none_or(|c| " \t\n;&|<>()".contains(*c));
        (ends && !delimiter.is_empty()).then(|| Heredoc {
            delimiter,
            quoted: quote.is_some(),
            strip_tabs,
            level: self.stack.len(),
        })
    }

    /// Splits the bodies of the here-documents started in this substitution on the line
    /// that just ended.
    fn heredoc_bodies(&mut self) {
        let level = self.stack.len();
        let first = self
            .heredocs
            .iter()
            .position(|d| d.level == level)
            .unwrap_or(self.heredocs.len());
        for doc in self.heredocs.split_off(first) {
            let start = self.at;
            let (body, continued) = self.read_body(&doc);
            // bash joins a line ending in a backslash with the next before comparing it
            // with the delimiter, and bash 3.2 reads a body in `$(…)` as program text
            // while looking for the `)`.
            let doubtful = continued && !doc.quoted
                || level > 0 && body.contains(['(', ')', '\'', '"', '`', '\\']);
            if doubtful {
                self.at = start;
                return self.lose_track();
            }
            let text = Text::Body { live: !doc.quoted };
            let depth = self.depth + 1;
            self.commands
                .extend(Splitter::new(&body, text, depth).split());
        }
    }

    /// Reads a here-document body up to its delimiter line, and whether a line of it ends
    /// in a backslash.
    fn read_body(&mut self, doc: &Heredoc) -> (String, bool) {
        let (mut body, mut continued) = (String::new(), false);
        while self.at < self.src.len() {
            let end = self.src[self.at..]
                .iter()
                .position(|&c| c == '\n')
                .map_or(self.src.len(), |p| self.at + p);
            let line: String = self.src[self.at..end].iter().collect();
            self.at = (end + 1).min(self.src.len());
            let line = if doc.strip_tabs {
                line.trim_start_matches('\t')
            } else {
                &line
            };
            if line == doc.delimiter {
                break;
            }
            continued |= line.ends_with('\\');
            body.push_str(line);
            body.push('\n');
        }
        (body, continued)
    }

    /// Stops tracking here-documents (see [`Splitter::lost`]).
    fn lose_track(&mut self) {
        self.heredocs.clear();
        if self.track && self.lost.is_none() {
            let line = self.src[..self.at]
                .iter()
                .rposition(|&c| c == '\n')
                .map_or(0, |p| p + 1);
            self.lost = Some(line);
        }
    }

    fn end_word(&mut self) {
        if self.in_word {
            let word = take(&mut self.word);
            let tok = if take(&mut self.undecodable) {
                Tok::Dyn
            } else {
                Tok::Lit(word)
            };
            self.words.push(tok);
            self.in_word = false;
        }
    }

    fn end_command(&mut self) {
        self.end_word();
        let data = matches!(self.text, Text::Body { .. });
        let words = take(&mut self.words);
        self.commands.push(Rough { words, data });
    }

    /// Opens a `$(…)` (its `$(` is consumed).
    fn open_substitution(&mut self) {
        let arithmetic = self.peek() == Some('(');
        self.open('(');
        self.arithmetic = arithmetic;
    }

    fn open(&mut self, opener: char) {
        self.stack.push(Frame {
            words: take(&mut self.words),
            word: take(&mut self.word),
            undecodable: take(&mut self.undecodable),
            quote: self.quote.take(),
            text: self.text,
            arithmetic: self.arithmetic,
            braces: take(&mut self.braces),
            opener,
            parens: 0,
        });
        self.text = Text::Program;
        self.arithmetic = false;
        self.in_word = false;
    }

    fn close(&mut self) {
        self.end_command();
        // bash reads no body for a here-document still waiting when its substitution
        // ends.
        if self
            .heredocs
            .last()
            .is_some_and(|d| d.level == self.stack.len())
        {
            self.lose_track();
        }
        if let Some(f) = self.stack.pop() {
            self.words = f.words;
            self.word = f.word;
            self.undecodable = f.undecodable;
            self.quote = f.quote;
            self.text = f.text;
            self.arithmetic = f.arithmetic;
            self.braces = f.braces;
            self.in_word = true;
        }
    }
}

/// Whether `c` may be in a here-document delimiter the scan tracks: it neither quotes,
/// escapes nor expands.
fn is_plain(c: char) -> bool {
    c.is_ascii_alphanumeric() || "_-.,:/+=@%^!?~".contains(c)
}

fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && !name.starts_with(|c: char| c.is_ascii_digit())
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nesting() {
        assert_eq!(rough_nesting("cargo test && git status"), 0);
        assert_eq!(rough_nesting("echo \"$(a $(b))\"; (c)"), 2);
        assert_eq!(rough_nesting("if x; then while y; do { z; }; done; fi"), 3);
    }

    fn words(commands: &[Rough]) -> Vec<(Vec<String>, bool)> {
        commands
            .iter()
            .map(|c| {
                let words = c
                    .words
                    .iter()
                    .map(|w| w.lit().unwrap_or("…").to_string())
                    .collect();
                (words, c.data)
            })
            .collect()
    }

    fn cmd(words: &[&str], data: bool) -> (Vec<String>, bool) {
        (words.iter().map(|w| (*w).to_string()).collect(), data)
    }

    #[test]
    fn splits_rough_commands() {
        let src = "A=1 c'u'rl x && (echo \"a$(git push -f)b\" | y) # z\nfor i in 1; do rm -rf /";
        assert_eq!(
            words(&rough_commands(src)),
            [
                cmd(&["curl", "x"], false),
                cmd(&["git", "push", "-f"], false),
                cmd(&["echo", "ab"], false),
                cmd(&["y"], false),
                cmd(&["i", "in", "1"], false),
                cmd(&["rm", "-rf", "/"], false),
            ]
        );
    }

    #[test]
    fn decodes_ansi_c_and_gettext_strings() {
        let src = "$'\\x63url' x; c$'u'rl y; $\"curl\" z; $'\\xff' w";
        assert_eq!(
            words(&rough_commands(src)),
            [
                cmd(&["curl", "x"], false),
                cmd(&["curl", "y"], false),
                cmd(&["curl", "z"], false),
                cmd(&["…", "w"], false),
            ]
        );
    }

    #[test]
    fn here_document_bodies_are_data() {
        // A substitution leaves an empty word behind in its enclosing command.
        let src = "cat <<EOF; a\nb $(c)\nEOF\ncat <<-'X' <<Y\n\tdata $(e)\n\tX\nf\nY\ng\necho $((1<<2))\nh";
        assert_eq!(
            words(&rough_commands(src)),
            [
                cmd(&["cat", "EOF"], false),
                cmd(&["a"], false),
                cmd(&["c"], false),
                cmd(&["b", ""], true),
                cmd(&["cat", "-X", "Y"], false),
                cmd(&["data", "$"], true),
                cmd(&["e"], true),
                cmd(&["f"], true),
                cmd(&["g"], false),
                cmd(&["1", "2"], false),
                cmd(&["echo", ""], false),
                cmd(&["h"], false),
            ]
        );
        assert_eq!(
            words(&rough_heredoc("it's $(curl x)\ncurl y\n")),
            [
                cmd(&["curl", "x"], false),
                cmd(&["it's", ""], true),
                cmd(&["curl", "y"], true),
            ]
        );
    }

    #[test]
    fn here_documents_bash_may_read_differently_are_program_text() {
        // A `<<` in `${…}` is text, and a body starts after a newline outside it.
        assert_eq!(
            words(&rough_commands("cat <<A ${x:-<<B\n}\na\nA\nb")),
            [
                cmd(&["cat", "A", "$"], false),
                cmd(&["x:-", "B"], false),
                cmd(&["a"], true),
                cmd(&["b"], false),
            ]
        );
        // bash reads this delimiter as `EOF`. From its line on, the text is program text,
        // split line by line as well, so the quote in `it's` cannot hide `curl y`.
        assert_eq!(
            words(&rough_commands("cat <<$'EOF'\nit's\nEOF\ncurl y")),
            [
                cmd(&["cat", "EOF"], false),
                cmd(&["its\nEOF\ncurl y"], false),
                cmd(&["cat", "EOF"], false),
                cmd(&["its"], false),
                cmd(&["EOF"], false),
                cmd(&["curl", "y"], false),
            ]
        );
    }
}
