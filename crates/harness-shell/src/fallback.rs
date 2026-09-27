//! Rough word splitting for input the parser rejected (or that is too long to
//! parse). Used only to look for denied or destructive commands, never to allow.

use std::collections::{BinaryHeap, HashMap, HashSet};
use std::mem::take;
use std::rc::Rc;

use crate::argv::{Tok, continued, decode_ansi_c};

/// Here-documents nested in substitutions inside here-document bodies deeper than this
/// are not tracked.
const MAX_HEREDOC_DEPTH: usize = 8;
/// A here-document delimiter is looked for this far at most.
const MAX_DELIMITER_CHARS: usize = 1024;
/// The splits restarted in a text process at most this many times its length.
const RESTART_BUDGET: usize = 16;
/// A chain of untracked here-documents on one line has at most this many candidate ends
/// per here-document.
const MAX_CANDIDATES: usize = 64;

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
/// parentheses end a command, and `$(…)`, `<(…)`, `>(…)` and backtick bodies become
/// commands of their own. Leading keywords and `NAME=value` words are dropped.
///
/// Here-document bodies are split into [`Rough::data`] commands where the scan can tell,
/// as bash would, where they start and end. Where it cannot, the body is split as program
/// text, and the text is split again from each line after which bash may end that body
/// (see [`Splitter::restarts`]); from the line where tracking stopped, it is also split
/// line by line. The text is also split as the scan did before it followed bash (see
/// [`Splitter::legacy`]), so no command that split found is lost.
pub(crate) fn rough_commands(src: &str) -> Vec<Rough> {
    split(src, Text::Program, 0)
}

/// Like [`rough_commands`], for the body of a here-document with an unquoted delimiter.
pub(crate) fn rough_heredoc(body: &str) -> Vec<Rough> {
    split(body, Text::Body { live: true }, 1)
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

/// Splits a text as [`split_bash`] and [`split_legacy`] do, dropping leading keywords and
/// assignments, and repeated commands.
fn split(src: &str, text: Text, depth: usize) -> Vec<Rough> {
    let mut commands = split_bash(src, text, depth);
    commands.append(&mut split_legacy(src, text, depth));
    let dropped = |w: &Tok| {
        w.lit()
            .is_some_and(|w| KEYWORDS.contains(&w) || is_assignment(w))
    };
    let mut seen = HashSet::new();
    commands
        .into_iter()
        .filter_map(|mut cmd| {
            let skip = cmd.words.iter().take_while(|w| dropped(w)).count();
            cmd.words.drain(..skip);
            let new = !cmd.words.is_empty() && seen.insert((cmd.words.clone(), cmd.data));
            new.then_some(cmd)
        })
        .collect()
}

/// Splits a text the way the scan did before it followed bash (see
/// [`Splitter::legacy`]), here-document bodies included.
fn split_legacy(src: &str, text: Text, depth: usize) -> Vec<Rough> {
    let mut legacy = Splitter::new(src.chars().collect(), 0, text, depth);
    legacy.legacy = true;
    legacy.run();
    legacy.commands
}

/// Splits a text following bash: from its start, then from each restart point its splits
/// find, and line by line from the first line where one of them stopped tracking
/// here-documents.
fn split_bash(src: &str, text: Text, depth: usize) -> Vec<Rough> {
    let src: Rc<[char]> = src.chars().collect();
    let mut first = Splitter::new(src.clone(), 0, text, depth);
    first.run();
    let mut shared = take(&mut first.shared);
    let mut commands = take(&mut first.commands);
    let mut lost = first.lost;
    let mut restarts: BinaryHeap<usize> = first.restarts.drain(..).collect();
    let mut budget = RESTART_BUDGET * src.len();
    // The latest restart first: it is the cheapest, and earlier ones often converge on it.
    while let Some(at) = restarts.pop() {
        if budget == 0 {
            break;
        }
        if !shared.clean.insert((at, false)) {
            continue;
        }
        let mut restart = Splitter::new(src.clone(), at, Text::Program, depth);
        restart.restarted = true;
        restart.shared = shared;
        restart.run();
        shared = take(&mut restart.shared);
        budget = budget.saturating_sub(restart.at.saturating_sub(at));
        commands.append(&mut restart.commands);
        lost = match (lost, restart.lost) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        restarts.extend(restart.restarts.drain(..));
    }
    if let Some(from) = lost {
        let rest: String = src[from..].iter().collect();
        for line in rest.split('\n') {
            let mut line = Splitter::new(line.chars().collect(), 0, text, depth);
            line.track = false;
            line.run();
            commands.append(&mut line.commands);
        }
    }
    commands
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
    /// The delimiter and whether it is quoted, when the scan trusts where the body ends:
    /// then the body is split as data.
    delimiter: Option<(String, bool)>,
    /// Every text bash may compare the body's lines with (see [`Splitter::readings`]).
    readings: Vec<String>,
    /// bash reads the `<<` as an operator; otherwise it only gives restart points.
    operator: bool,
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

/// What the splits of one text share.
#[derive(Default)]
struct Shared {
    /// Line starts a split reached in a clean state (program text, outside quotes,
    /// substitutions and expansions, with no here-document pending), and whether it had
    /// stopped tracking here-documents. A split in the same state there goes on alike.
    clean: HashSet<(usize, bool)>,
    /// The text's lines, built on first use.
    lines: Option<Lines>,
}

/// Where the lines of a text end, by the text bash may compare with a here-document
/// delimiter, in four variants: `[plain, stripped, joined, joined and stripped]`. A
/// joined line is a line ending in an odd number of backslashes joined with the lines
/// after it; a stripped one has its leading tabs removed, as `<<-` does to a whole joined
/// line.
struct Lines(HashMap<String, [Vec<usize>; 4]>);

impl Lines {
    fn new(src: &[char]) -> Self {
        let mut map: HashMap<String, [Vec<usize>; 4]> = HashMap::new();
        let mut add = |joined: bool, text: &str, after: usize| {
            for stripped in [false, true] {
                let text = if stripped {
                    text.trim_start_matches('\t')
                } else {
                    text
                };
                let variant = 2 * usize::from(joined) + usize::from(stripped);
                map.entry(text.to_owned()).or_default()[variant].push(after);
            }
        };
        let (mut at, mut joined, mut parts) = (0, String::new(), 0);
        while at < src.len() {
            let end = src[at..]
                .iter()
                .position(|&c| c == '\n')
                .map_or(src.len(), |p| at + p);
            let line: String = src[at..end].iter().collect();
            let after = (end + 1).min(src.len());
            add(false, &line, after);
            if continued(&line) {
                joined.push_str(&line[..line.len() - 1]);
                parts += 1;
            } else {
                if parts > 0 {
                    joined.push_str(&line);
                    add(true, &joined, after);
                }
                joined.clear();
                parts = 0;
            }
            at = end + 1;
        }
        Lines(map)
    }

    /// For each reading, where the first line bash may end a here-document at, reading
    /// its body from `from`, ends: the start of the line after it.
    fn ends(&self, readings: &[String], strip_tabs: bool, from: usize) -> Vec<usize> {
        let mut ends = Vec::new();
        for variants in readings.iter().filter_map(|r| self.0.get(r.as_str())) {
            for joined in [false, true] {
                let after = &variants[2 * usize::from(joined) + usize::from(strip_tabs)];
                if let Some(&end) = after.get(after.partition_point(|&p| p <= from)) {
                    ends.push(end);
                }
            }
        }
        ends
    }
}

struct Splitter {
    src: Rc<[char]>,
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
    /// Line starts after which bash may end a here-document whose end the scan does not
    /// know. Program text may resume there with a clean quote state, so the text is split
    /// again from each.
    restarts: Vec<usize>,
    /// This split starts at a restart point, and stops where another split already went
    /// on in the same state.
    restarted: bool,
    /// Splits here-documents as the scan did before it followed bash's rules: a `<<`
    /// outside arithmetic starts a body at the next newline of program text, whatever the
    /// substitution or expansion, its delimiter read up to an operator character.
    legacy: bool,
    done: bool,
    shared: Shared,
    /// The first newline at or after some position at or before [`Splitter::at`].
    newline: Option<usize>,
    /// Here-document bodies this text is nested in.
    depth: usize,
    /// Outer contexts of the open substitutions.
    stack: Vec<Frame>,
    /// Open backtick substitutions.
    backticks: usize,
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
    fn new(src: Rc<[char]>, at: usize, text: Text, depth: usize) -> Self {
        Splitter {
            src,
            at,
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
            restarts: Vec::new(),
            restarted: false,
            legacy: false,
            done: false,
            shared: Shared::default(),
            newline: None,
            depth,
            stack: Vec::new(),
            backticks: 0,
        }
    }

    fn run(&mut self) {
        while !self.done {
            let Some(c) = self.next() else { break };
            match self.text {
                Text::Program => self.program(c),
                Text::Body { live } => self.body(c, live),
            }
        }
        while !self.stack.is_empty() {
            self.close();
        }
        self.end_command();
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
            if !self.legacy {
                self.expansion(c);
            }
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
                // A process substitution.
                '<' | '>' if !self.legacy && self.peek() == Some('(') => {
                    self.end_word();
                    self.at += 1;
                    self.open('(');
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
                    if self.legacy {
                        self.legacy_bodies();
                    } else {
                        if self.braces.is_empty() {
                            self.heredoc_bodies();
                        }
                        self.line_start();
                    }
                }
                ';' | '&' | '|' | '{' | '}' => self.end_command(),
                _ => self.push(c),
            },
        }
    }

    /// At the start of a line of program text: a restarted split stops where another split
    /// already went on in the same clean state.
    fn line_start(&mut self) {
        let clean = self.track
            && self.text == Text::Program
            && self.stack.is_empty()
            && self.braces.is_empty()
            && self.heredocs.is_empty()
            && self.arithmetic_commands == 0;
        if clean && !self.shared.clean.insert((self.at, self.lost.is_some())) {
            self.done = self.restarted;
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

    /// After a `<` followed by another: a here-string (`<<<`), a shift in arithmetic, or
    /// a here-document whose body follows the current line. The delimiter word itself is
    /// left to be read as a word.
    ///
    /// A `<<` in `${…}` or `$[…]` is text, but the scan once took it for an operator, so
    /// it still gives restart points.
    fn redirection(&mut self) {
        self.at += 1;
        if self.next_if_eq('<') {
            return;
        }
        if self.legacy {
            return self.legacy_heredoc();
        }
        if self.arithmetic || !self.track {
            return;
        }
        let strip_tabs = self.peek() == Some('-');
        let operator = self.braces.is_empty();
        // bash reads backtick text only when it runs it, and may end `((…))` elsewhere.
        let trusted = operator
            && self.lost.is_none()
            && self.arithmetic_commands == 0
            && self.depth < MAX_HEREDOC_DEPTH
            && self.backticks == 0;
        let delimiter = self.delimiter(strip_tabs).filter(|_| trusted);
        if operator && delimiter.is_none() {
            self.lose_track();
        }
        let readings = self.readings(strip_tabs);
        self.heredocs.push(Heredoc {
            delimiter,
            readings,
            operator,
            strip_tabs,
            level: self.stack.len(),
        });
    }

    /// The delimiter of the here-document whose `<<` was just read, and whether it is
    /// quoted, if it is plain text or plain text in one pair of quotes: bash versions read
    /// other spellings (`$'…'`, backslashes, partial quoting, expansions) differently.
    fn delimiter(&self, strip_tabs: bool) -> Option<(String, bool)> {
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
        (ends && !delimiter.is_empty()).then(|| (delimiter, quote.is_some()))
    }

    /// Every text bash may compare the body lines of the here-document whose `<<` was just
    /// read with: its delimiter word as written; with quotes removed as bash does, `$'…'`
    /// decoded; with every quote character removed and each escaped character kept, as
    /// brush-parser does; and as this scan read it before it followed bash (up to a
    /// parenthesis, backslashes escaping outside single quotes, `$` kept).
    fn readings(&self, strip_tabs: bool) -> Vec<String> {
        let mut i = self.at + usize::from(strip_tabs);
        while matches!(self.src.get(i), Some(' ' | '\t')) {
            i += 1;
        }
        let limit = self.src.len().min(i + MAX_DELIMITER_CHARS);
        let src = &self.src[i..limit];
        let word = &src[..word_len(src)];
        if word.is_empty() {
            return Vec::new();
        }
        let mut readings = vec![
            word.iter().collect(),
            bash_unquoted(word),
            unquoted(word),
            older_delimiter(src).0,
        ];
        readings.sort();
        readings.dedup();
        readings
    }

    /// A here-document as [`Splitter::legacy`] reads it.
    fn legacy_heredoc(&mut self) {
        let arithmetic = self.arithmetic || self.arithmetic_commands > 0;
        if arithmetic || self.depth >= MAX_HEREDOC_DEPTH {
            return;
        }
        let strip_tabs = self.peek() == Some('-');
        let mut i = self.at + usize::from(strip_tabs);
        while matches!(self.src.get(i), Some(' ' | '\t')) {
            i += 1;
        }
        let limit = self.src.len().min(i + MAX_DELIMITER_CHARS);
        let (delimiter, quoted) = older_delimiter(&self.src[i..limit]);
        if !delimiter.is_empty() || quoted {
            self.heredocs.push(Heredoc {
                delimiter: Some((delimiter, quoted)),
                readings: Vec::new(),
                operator: true,
                strip_tabs,
                level: 0,
            });
        }
    }

    /// Splits the bodies of every pending here-document as [`Splitter::legacy`] does.
    fn legacy_bodies(&mut self) {
        for doc in take(&mut self.heredocs) {
            let Some((delimiter, quoted)) = doc.delimiter else {
                continue;
            };
            let (body, _) = self.read_body(&delimiter, doc.strip_tabs);
            let text = Text::Body { live: !quoted };
            self.commands
                .extend(split_legacy(&body, text, self.depth + 1));
        }
    }

    /// Splits the bodies of the here-documents started in this substitution on the line
    /// that just ended.
    fn heredoc_bodies(&mut self) {
        // Pending here-documents are in operator order, so their levels never decrease.
        let level = self.stack.len();
        let first = self.heredocs.partition_point(|d| d.level < level);
        let (docs, texts): (Vec<_>, Vec<_>) = self
            .heredocs
            .split_off(first)
            .into_iter()
            .partition(|d| d.operator);
        let from = self.at;
        for text in texts {
            self.untracked(std::iter::once(text), from);
        }
        let mut docs = docs.into_iter();
        while let Some(doc) = docs.next() {
            let Some((delimiter, quoted)) = &doc.delimiter else {
                let from = self.at;
                return self.untracked(std::iter::once(doc).chain(docs), from);
            };
            let start = self.at;
            let (body, continued) = self.read_body(delimiter, doc.strip_tabs);
            // bash joins a line ending in a backslash with the next before comparing it
            // with the delimiter, and bash 3.2 reads a body in `$(…)` as program text
            // while looking for the `)`.
            let doubtful = continued && !quoted
                || level > 0 && body.contains(['(', ')', '\'', '"', '`', '\\']);
            if doubtful {
                self.at = start;
                self.lose_track();
                return self.untracked(std::iter::once(doc).chain(docs), start);
            }
            let text = Text::Body { live: !quoted };
            self.commands
                .extend(split_bash(&body, text, self.depth + 1));
        }
    }

    /// Here-documents whose bodies would start in turn at `from`, each after the one
    /// before, where the scan does not know where they end: every line after which bash
    /// may end one of them is a restart point. A body is also looked for from `from`, in
    /// case bash does not read the operators before it as here-documents.
    fn untracked(&mut self, docs: impl Iterator<Item = Heredoc>, from: usize) {
        let src = &self.src;
        let lines = self.shared.lines.get_or_insert_with(|| Lines::new(src));
        let mut starts = vec![from];
        for doc in docs {
            if !starts.contains(&from) {
                starts.push(from);
            }
            let mut ends: Vec<usize> = starts
                .iter()
                .flat_map(|&start| lines.ends(&doc.readings, doc.strip_tabs, start))
                .collect();
            ends.sort_unstable();
            ends.dedup();
            ends.truncate(MAX_CANDIDATES);
            self.restarts.extend(&ends);
            starts = ends;
        }
    }

    /// Reads a here-document body up to its delimiter line, and whether a line of it ends
    /// in a backslash.
    fn read_body(&mut self, delimiter: &str, strip_tabs: bool) -> (String, bool) {
        let (mut body, mut continued) = (String::new(), false);
        while self.at < self.src.len() {
            let end = self.src[self.at..]
                .iter()
                .position(|&c| c == '\n')
                .map_or(self.src.len(), |p| self.at + p);
            let line: String = self.src[self.at..end].iter().collect();
            self.at = (end + 1).min(self.src.len());
            let line = if strip_tabs {
                line.trim_start_matches('\t')
            } else {
                &line
            };
            if line == delimiter {
                break;
            }
            continued |= line.ends_with('\\');
            body.push_str(line);
            body.push('\n');
        }
        (body, continued)
    }

    /// The start of the line after the current one.
    fn next_line(&mut self) -> usize {
        let newline = match self.newline.filter(|&n| n >= self.at) {
            Some(n) => n,
            None => self.src[self.at..]
                .iter()
                .position(|&c| c == '\n')
                .map_or(self.src.len(), |p| self.at + p),
        };
        self.newline = Some(newline);
        (newline + 1).min(self.src.len())
    }

    /// Stops trusting where here-documents end (see [`Splitter::lost`]); those pending
    /// only give restart points.
    fn lose_track(&mut self) {
        // Once lost, every here-document pushed is untracked already.
        if self.lost.is_some() {
            return;
        }
        for doc in &mut self.heredocs {
            doc.delimiter = None;
        }
        if self.track {
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
        self.backticks += usize::from(opener == '`');
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
        // bash 3.2 reads no body for a here-document still waiting when its substitution
        // ends; the lines after may be its body in other versions.
        let level = self.stack.len();
        let first = self.heredocs.partition_point(|d| d.level < level);
        if !self.legacy && first < self.heredocs.len() {
            let docs = self.heredocs.split_off(first);
            self.lose_track();
            let next_line = self.next_line();
            self.untracked(docs.into_iter(), next_line);
        }
        if let Some(f) = self.stack.pop() {
            self.backticks -= usize::from(f.opener == '`');
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

/// The length of the shell word at the start of `src`, as bash reads it: up to a blank,
/// newline or operator character outside quotes, escapes and `$(…)`, `${…}`, `$[…]` or
/// backticks.
fn word_len(src: &[char]) -> usize {
    let mut i = 0;
    while let Some(&c) = src.get(i) {
        match c {
            ' ' | '\t' | '\n' | ';' | '&' | '|' | '<' | '>' | '(' | ')' => break,
            '\\' => i += 1,
            '\'' | '"' | '`' => {
                let mut j = i + 1;
                while let Some(&d) = src.get(j).filter(|&&d| d != c) {
                    j += usize::from(d == '\\' && c != '\'') + 1;
                }
                i = j;
            }
            '$' if matches!(src.get(i + 1), Some('(' | '{' | '[')) => {
                let (open, close) = match src[i + 1] {
                    '(' => ('(', ')'),
                    '{' => ('{', '}'),
                    _ => ('[', ']'),
                };
                let mut depth = 0usize;
                let mut j = i + 1;
                while let Some(&d) = src.get(j) {
                    depth = if d == open {
                        depth + 1
                    } else if d == close {
                        depth - 1
                    } else {
                        depth
                    };
                    if depth == 0 {
                        break;
                    }
                    j += 1;
                }
                i = j;
            }
            _ => {}
        }
        i += 1;
    }
    i.min(src.len())
}

/// A word with its quotes removed as bash removes them from a here-document delimiter,
/// with `$'…'` decoded.
fn bash_unquoted(word: &[char]) -> String {
    let mut out = String::new();
    let mut i = 0;
    while let Some(&c) = word.get(i) {
        i += 1;
        match c {
            '\\' => {
                out.extend(word.get(i).filter(|&&n| n != '\n'));
                i += 1;
            }
            '\'' => {
                let end = word[i..]
                    .iter()
                    .position(|&d| d == '\'')
                    .map_or(word.len(), |n| i + n);
                out.extend(&word[i..end]);
                i = end + 1;
            }
            '$' if word.get(i) == Some(&'\'') => {
                let mut end = i + 1;
                while let Some(&d) = word.get(end).filter(|&&d| d != '\'') {
                    end += if d == '\\' { 2 } else { 1 };
                }
                let end = end.min(word.len());
                let raw: String = word[i + 1..end].iter().collect();
                out.push_str(&decode_ansi_c(&raw).unwrap_or(raw));
                i = end + 1;
            }
            // `$"…"`: the quotes follow.
            '$' if word.get(i) == Some(&'"') => {}
            '"' => {
                while let Some(&d) = word.get(i).filter(|&&d| d != '"') {
                    i += 1;
                    match (d, word.get(i)) {
                        ('\\', Some(&n @ ('$' | '`' | '"' | '\\'))) => {
                            out.push(n);
                            i += 1;
                        }
                        ('\\', Some('\n')) => i += 1,
                        _ => out.push(d),
                    }
                }
                i += 1;
            }
            _ => out.push(c),
        }
    }
    out
}

/// A word with every quote character removed and each escaped character kept, as
/// brush-parser reads a quoted here-document delimiter.
fn unquoted(word: &[char]) -> String {
    let (mut out, mut escaped) = (String::new(), false);
    for &c in word {
        if escaped {
            out.push(c);
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c != '\'' && c != '"' {
            out.push(c);
        }
    }
    out
}

/// The delimiter at the start of `src` as [`Splitter::legacy`] reads it, and whether it
/// is quoted.
fn older_delimiter(src: &[char]) -> (String, bool) {
    let (mut out, mut quoted, mut quote, mut i) = (String::new(), false, None, 0);
    while let Some(&c) = src.get(i) {
        i += 1;
        match (quote, c) {
            (_, '\n') | (None, ' ' | '\t' | ';' | '&' | '|' | '<' | '>' | '(' | ')') => break,
            (None, '\'' | '"') => {
                quote = Some(c);
                quoted = true;
            }
            (Some(q), _) if c == q => quote = None,
            (None | Some('"'), '\\') => {
                quoted = true;
                out.extend(src.get(i));
                i += 1;
            }
            _ => out.push(c),
        }
    }
    (out, quoted)
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
    fn deep_substitutions_split_in_linear_time() {
        for src in [
            format!("{}{}", "$(".repeat(40_000), "cat <<X ".repeat(40_000)),
            format!("{}{}", "cat <<X ".repeat(40_000), "$(\n".repeat(40_000)),
            format!("cat {}\n(", "<<\\A ".repeat(40_000)),
            "$(cat <<E)".repeat(20_000),
        ] {
            let start = std::time::Instant::now();
            rough_commands(&src);
            let took = start.elapsed();
            assert!(took < std::time::Duration::from_secs(1), "{took:?}");
        }
    }

    #[test]
    fn here_documents_bash_may_read_differently_are_program_text() {
        // A `<<` in `${…}` is text, and a body starts after a newline outside it. The scan
        // as it was before took `b` for data of `<<B`, which only adds a data command.
        assert_eq!(
            words(&rough_commands("cat <<A ${x:-<<B\n}\na\nA\nb")),
            [
                cmd(&["cat", "A", "$"], false),
                cmd(&["x:-", "B"], false),
                cmd(&["a"], true),
                cmd(&["b"], false),
                cmd(&["b"], true),
            ]
        );
        // bash reads this delimiter as `EOF`. The text is program text, split again after
        // the `EOF` line, and line by line from the delimiter's line, so the quote in `it's`
        // cannot hide `curl y`. The scan as it was before read the delimiter as `$EOF`.
        assert_eq!(
            words(&rough_commands("cat <<$'EOF'\nit's\nEOF\ncurl y")),
            [
                cmd(&["cat", "EOF"], false),
                cmd(&["its\nEOF\ncurl y"], false),
                cmd(&["curl", "y"], false),
                cmd(&["its"], false),
                cmd(&["EOF"], false),
                cmd(&["it's"], true),
                cmd(&["EOF"], true),
                cmd(&["curl", "y"], true),
            ]
        );
    }
}
