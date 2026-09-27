//! Where bash may end a command substitution elsewhere than the analysis does.
//!
//! bash 3.2 (macOS `/bin/bash`) reads a `$(…)` twice, and neither pass parses it as a
//! program:
//!
//! 1. At parse time, `parse_matched_pair` (parse.y) decides where the word ends. It counts
//!    parentheses, pairs quotes and backticks, removes backslash-newlines, and sees a comment
//!    only after a blank or a newline, never inside `"$(…)"`. It knows nothing of
//!    here-documents, `case` patterns or `${…}`.
//! 2. When the word is expanded, `extract_delimited_string` (subst.c) finds the `$(…)` again
//!    in the word's text, and there it sees a comment after a blank or a newline even inside
//!    `"$(…)"`. The substitutions of a here-document body or of a `${…}` are only read this
//!    way.
//!
//! bash 4 and later, like brush-parser, parse a substitution as a program. So bash 3.2 can
//! end a `$(…)` at another `)` than the analysis, and then run text the analysis took for
//! data, or the reverse. brush-parser in turn misses some comments that bash 5 sees.
//!
//! [`divergence`] follows a program the way bash 3.2 reads it and requires that, for every
//! `$(…)`, both bash 3.2 passes end it where brush-parser does; then it checks the text
//! bash 3.2 runs the same way. [`substitution_misread`] checks each substitution the
//! analysis takes against bash 5's comment rules.
//!
//! The ports keep the structure of the C functions of bash 3.2.57, named where each is
//! mirrored. They work on `char`s where bash works on bytes, which is the same for these
//! rules: every character they test is ASCII.

use brush_parser::word::WordPiece;

use crate::argv::{self, decode_ansi_c, guarded, parser_options, unsafe_heredoc};

/// bash marks some characters of a word's text with these (`CTLESC`, `CTLNUL`); `CTLESC`
/// then quotes the next character.
const CTLESC: char = '\u{1}';
const CTLNUL: char = '\u{7f}';
/// Constructs nested deeper than this are not followed: that counts as a disagreement.
const MAX_NESTING: usize = 64;

const ENDS_ELSEWHERE: &str =
    "bash 3.2 (macOS /bin/bash) ends a `$(…)` here elsewhere than the analysis does";
const EXPANDS_ELSEWHERE: &str =
    "bash 3.2 (macOS /bin/bash) expands a `$(…)` here to other text than the analysis reads";
const BRACE_ELSEWHERE: &str =
    "bash 3.2 (macOS /bin/bash) ends a `${…}` here elsewhere than the analysis does";
const COMMENT_CONTINUES: &str =
    "a comment in a `$(…)` ends in a backslash, which bash versions read differently";
const COMMENT_MISREAD: &str =
    "bash reads a comment in a `$(…)` here and ends it elsewhere than the analysis does";
const TOO_DEEP: &str = "command substitutions are nested too deeply to compare with bash 3.2";

/// Why bash 3.2 may read a command substitution of the program `src` differently from the
/// analysis, if it may. `src` is a program bash parses from its start: a command, or `bash
/// -c` or `eval` text.
///
/// Accepted without a reason, because bash 3.2 then runs nothing the analysis did not see:
/// - bash 3.2 reaches the end of `src` inside a construct: it reports a syntax error and
///   runs nothing from that command on, and everything before it agreed;
/// - its expansion reports a bad substitution: that command does not run.
pub(crate) fn divergence(src: &str) -> Option<&'static str> {
    if !src.contains(['$', '`']) {
        return None;
    }
    // bash 5 ends a comment at a newline even after a backslash; bash 3.2 removes the
    // backslash-newline while it reads a `$(…)`, and brush-parser joins the lines in places.
    let substitutes = src.contains("$(") || src.contains("$\\\n");
    if substitutes && src.split('\n').any(comment_continues) {
        return Some(COMMENT_CONTINUES);
    }
    let b: Vec<char> = src.chars().collect();
    Reader { b: &b, depth: 0 }.program().err()
}

/// Why bash may end the command substitution whose text the analysis took as `text`
/// elsewhere, if it may: bash 5 reads a comment at the start of any word of a `$(…)`, and
/// ends it at a newline even after a backslash, where brush-parser does not always.
pub(crate) fn substitution_misread(text: &str) -> Option<&'static str> {
    if text.split('\n').any(comment_continues) {
        return Some(COMMENT_CONTINUES);
    }
    // Only comments are compared here: where the analysis reads other constructs (a `case`
    // pattern, a here-document) differently, the text it took does not parse.
    if !text.contains('#') {
        return None;
    }
    let t: Vec<char> = "$(".chars().chain(text.chars()).chain([')']).collect();
    (ends5(&t, 2, 0) != Some(t.len() - 1)).then_some(COMMENT_MISREAD)
}

/// How reading a program as bash 3.2 does stopped early.
enum Stop {
    /// bash 3.2 reached the end of the text inside a construct: a syntax error.
    Eof,
    /// bash 3.2 and the analysis read a construct differently.
    Diverges(&'static str),
}

/// A construct whose text bash 3.2 checks or runs later, found while reading a word.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    /// `$(…)`.
    Substitution,
    /// `<(…)` or `>(…)`.
    Process,
    /// `${…}`.
    Brace,
    /// `` `…` ``.
    Backtick,
}

/// Where a construct is: from its `$`, `<`, `>` or opening backtick to its closing
/// character, in the source (`raw`) and in the text bash 3.2 built from it (`text`).
#[derive(Clone, Copy, Debug)]
struct Construct {
    kind: Kind,
    raw: (usize, usize),
    text: (usize, usize),
    /// Inside double quotes, where bash drops the backslash of `\"` before it reads a
    /// backquoted command.
    in_dquote: bool,
}

impl Construct {
    fn shifted(self, by: usize) -> Construct {
        Construct {
            text: (self.text.0 + by, self.text.1 + by),
            ..self
        }
    }
}

/// What [`pair`] read.
struct Pair {
    /// The source index of the closing character.
    end: usize,
    /// The text bash 3.2 keeps, ending with the closing character.
    text: Vec<char>,
    /// The constructs of a double-quoted string, with `text` offsets.
    nested: Vec<Construct>,
}

/// The flags of `parse_matched_pair`.
#[derive(Clone, Copy, Default)]
struct Flags {
    /// `P_FIRSTCLOSE`: the first closing character ends the construct.
    first_close: bool,
    /// `P_ALLOWESC`: a backslash escapes in single quotes (`$'…'`).
    allow_esc: bool,
    /// `P_DQUOTE`: inside double quotes.
    dquote: bool,
    /// `P_COMMAND`: a command, where comments are seen.
    command: bool,
}

/// The index at or after `i` where `b` has no backslash-newline, which bash's `shell_getc`
/// removes when it reads outside single quotes.
fn skip_continuations(b: &[char], mut i: usize) -> usize {
    while b.get(i) == Some(&'\\') && b.get(i + 1) == Some(&'\n') {
        i += 2;
    }
    i
}

/// bash 3.2's `parse_matched_pair (qc, open, close, lenp, flags)`, reading `b` from `from`,
/// just after the opening character.
fn pair(
    b: &[char],
    from: usize,
    qc: Option<char>,
    open: char,
    close: char,
    flags: Flags,
    depth: usize,
) -> Result<Pair, Stop> {
    if depth > MAX_NESTING {
        return Err(Stop::Diverges(TOO_DEEP));
    }
    let mut count = 1usize;
    let (mut pass_next, mut was_dollar, mut in_comment) = (false, false, false);
    let check_comment = flags.command && !matches!(qc, Some('`' | '\'' | '"')) && !flags.dquote;
    // RFLAGS: the flags passed to recursive calls.
    let rflags = Flags {
        dquote: qc == Some('"') || flags.dquote,
        ..Flags::default()
    };
    let mut ret: Vec<char> = Vec::new();
    let mut nested = Vec::new();
    let mut dollar_at = 0;
    let mut i = from;
    loop {
        // shell_getc (qc != '\'' && pass_next_character == 0 && backq_backslash == 0)
        if qc != Some('\'') && !pass_next {
            i = skip_continuations(b, i);
        }
        let Some(&ch) = b.get(i) else {
            // "unexpected EOF while looking for matching …"
            return Err(Stop::Eof);
        };
        let at = i;
        i += 1;
        if in_comment {
            ret.push(ch);
            if ch == '\n' {
                in_comment = false;
            }
            continue;
        } else if check_comment
            && ch == '#'
            && ret.last().is_none_or(|&l| matches!(l, '\n' | ' ' | '\t'))
        {
            in_comment = true;
        }
        if pass_next {
            // The last character was a backslash.
            pass_next = false;
            if qc != Some('\'') && ch == '\n' {
                // A double-quoted backslash-newline disappears.
                ret.pop();
                continue;
            }
            if ch == CTLESC || ch == CTLNUL {
                ret.push(CTLESC);
            }
            ret.push(ch);
            continue;
        } else if ch == CTLESC || ch == CTLNUL {
            ret.extend([CTLESC, ch]);
            continue;
        } else if ch == close {
            count -= 1;
        } else if open != close && was_dollar && open == '{' && ch == open {
            // A nested `${`.
            count += 1;
        } else if !flags.first_close && ch == open {
            count += 1;
        }
        ret.push(ch);
        if count == 0 {
            return Ok(Pair {
                end: at,
                text: ret,
                nested,
            });
        }
        if open == '\'' {
            if flags.allow_esc && ch == '\\' {
                pass_next = true;
            }
            continue;
        }
        if ch == '\\' {
            pass_next = true;
        }
        if open != close {
            // Quotes inside a grouping construct.
            if matches!(ch, '\'' | '"' | '`') {
                let ansi = was_dollar && ch == '\'';
                let inner_flags = Flags {
                    allow_esc: ansi,
                    ..rflags
                };
                let inner = pair(b, i, Some(ch), ch, ch, inner_flags, depth + 1)?;
                i = inner.end + 1;
                let body = &inner.text[..inner.text.len() - 1];
                if ansi {
                    // `$'…'` is translated here (`extended_quote` is on): back up before
                    // the `$'`.
                    ret.truncate(ret.len() - 2);
                    let decoded = ansiexpand(body);
                    if rflags.dquote {
                        ret.extend(decoded);
                    } else {
                        ret.extend(sh_single_quote(&decoded));
                    }
                } else if was_dollar && ch == '"' {
                    // `$"…"`: `localeexpand` leaves the text as it is.
                    ret.truncate(ret.len() - 2);
                    ret.push('"');
                    ret.extend(body);
                    ret.push('"');
                } else {
                    ret.extend(inner.text);
                }
            }
        } else if open == '"' && ch == '`' {
            // An old-style command substitution within double quotes.
            let start = ret.len() - 1;
            let inner = pair(b, i, None, '`', '`', rflags, depth + 1)?;
            i = inner.end + 1;
            ret.extend(inner.text);
            nested.push(Construct {
                kind: Kind::Backtick,
                raw: (at, inner.end),
                text: (start, ret.len() - 1),
                in_dquote: true,
            });
        } else if open != '`' && was_dollar && matches!(ch, '(' | '{' | '[') {
            // `$(…)`, `${…}` or `$[…]` inside a quoted string (only double quotes reach
            // here).
            if open == ch {
                count -= 1;
            }
            let (inner_close, inner_flags, kind) = match ch {
                '(' => (
                    ')',
                    Flags {
                        dquote: false,
                        ..rflags
                    },
                    Some(Kind::Substitution),
                ),
                '{' => (
                    '}',
                    Flags {
                        first_close: true,
                        ..rflags
                    },
                    Some(Kind::Brace),
                ),
                _ => (']', rflags, None),
            };
            let start = ret.len() - 2;
            let inner = pair(b, i, None, ch, inner_close, inner_flags, depth + 1)?;
            i = inner.end + 1;
            ret.extend(inner.text);
            if let Some(kind) = kind {
                nested.push(Construct {
                    kind,
                    raw: (dollar_at, inner.end),
                    text: (start, ret.len() - 1),
                    in_dquote: true,
                });
            }
        }
        was_dollar = ch == '$';
        if was_dollar {
            dollar_at = at;
        }
    }
}

/// The text of a `$'…'` string after bash's `ansiexpand`.
fn ansiexpand(body: &[char]) -> Vec<char> {
    let raw: String = body.iter().collect();
    decode_ansi_c(&raw).unwrap_or(raw).chars().collect()
}

/// bash's `sh_single_quote`: `s` in single quotes, each `'` in it written `'\''`.
fn sh_single_quote(s: &[char]) -> Vec<char> {
    let mut out = vec!['\''];
    for &c in s {
        if c == '\'' {
            out.extend(['\'', '\\', '\'', '\'']);
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// How an expansion-time extraction ended.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Ext {
    /// At this index: the closing character.
    End(usize),
    /// At the end of the text: bash reports a bad substitution and runs nothing of the
    /// command.
    Bad,
}

/// A `$(…)` or backquoted command found inside a `${…}` or a double-quoted string at
/// expansion time: the index of its `$` or opening backtick, and of its closing character.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Found {
    backtick: bool,
    at: usize,
    end: usize,
}

/// The expansion-time extractions of bash 3.2 (subst.c) over the text `t` of a word or a
/// here-document body. `found` collects the substitutions of a `${…}` (and of the double
/// quotes in it) that bash expands with its value.
struct Extract<'a> {
    t: &'a [char],
    found: Vec<Found>,
    too_deep: bool,
}

impl Extract<'_> {
    fn new(t: &[char]) -> Extract<'_> {
        Extract {
            t,
            found: Vec::new(),
            too_deep: false,
        }
    }

    fn deeper(&mut self, depth: usize) -> bool {
        self.too_deep |= depth > MAX_NESTING;
        self.too_deep
    }

    /// `extract_delimited_string (string, &i, "$(", "(", ")", EX_COMMAND)`, from `from`,
    /// just after the `$(`.
    fn delimited(&mut self, from: usize, depth: usize) -> Ext {
        if self.deeper(depth) {
            return Ext::Bad;
        }
        let t = self.t;
        let mut nesting = 1usize;
        let (mut pass_character, mut in_comment) = (false, false);
        let mut i = from;
        while let Some(&c) = t.get(i) {
            if in_comment {
                in_comment = c != '\n';
                i += 1;
                continue;
            }
            if pass_character {
                // The previous character was a backslash.
                pass_character = false;
                i += 1;
                continue;
            }
            if c == '#' && (i == 0 || matches!(t[i - 1], '\n' | ' ' | '\t')) {
                in_comment = true;
                i += 1;
                continue;
            }
            if c == CTLESC || c == '\\' {
                pass_character = true;
                i += 1;
                continue;
            }
            // A nested opener, `$(` or `(`.
            let opener = match (c, t.get(i + 1)) {
                ('$', Some('(')) => Some(2),
                ('(', _) => Some(1),
                _ => None,
            };
            if let Some(len) = opener {
                match self.delimited(i + len, depth + 1) {
                    Ext::End(end) => i = end + 1,
                    Ext::Bad => return Ext::Bad,
                }
                continue;
            }
            if c == ')' {
                nesting -= 1;
                if nesting == 0 {
                    return Ext::End(i);
                }
            }
            match c {
                // Old-style command substitution, passed through verbatim.
                '`' => match string_extract_backquote(t, i + 1) {
                    Some(end) => i = end + 1,
                    None => return Ext::Bad,
                },
                '\'' => i = skip_single_quoted(t, i + 1),
                '"' => match self.skip_double_quoted(i + 1, false, depth + 1) {
                    Some(end) => i = end,
                    None => return Ext::Bad,
                },
                _ => i += 1,
            }
        }
        Ext::Bad
    }

    /// `skip_double_quoted (string, slen, sind)`: the index after the closing `"`, or the
    /// end of the text. `None` if a substitution in it reports a bad substitution. With
    /// `record`, its substitutions are added to [`Extract::found`].
    fn skip_double_quoted(&mut self, from: usize, record: bool, depth: usize) -> Option<usize> {
        if self.deeper(depth) {
            return None;
        }
        let t = self.t;
        let (mut pass_next, mut backquote) = (false, None);
        let mut i = from;
        while let Some(&c) = t.get(i) {
            if pass_next {
                pass_next = false;
                i += 1;
            } else if c == '\\' {
                pass_next = true;
                i += 1;
            } else if let Some(start) = backquote {
                if c == '`' {
                    backquote = None;
                    self.record(record, true, start, i);
                }
                i += 1;
            } else if c == '`' {
                backquote = Some(i);
                i += 1;
            } else if c == '$' && t.get(i + 1) == Some(&'(') {
                match self.delimited(i + 2, depth + 1) {
                    Ext::End(end) => {
                        self.record(record, false, i, end);
                        i = end + 1;
                    }
                    Ext::Bad => return None,
                }
            } else if c == '$' && t.get(i + 1) == Some(&'{') {
                match self.dollar_brace(i + 2, record, depth + 1) {
                    Ext::End(end) => i = end + 1,
                    Ext::Bad => return None,
                }
            } else if c != '"' {
                i += 1;
            } else {
                return Some(i + 1);
            }
        }
        Some(i)
    }

    fn record(&mut self, record: bool, backtick: bool, at: usize, end: usize) {
        if record {
            self.found.push(Found { backtick, at, end });
        }
    }

    /// `extract_dollar_brace_string (string, &i, quoted, flags)`, from `from`, just after
    /// the `${`. With `record`, the substitutions bash expands with its value are added to
    /// [`Extract::found`].
    fn dollar_brace(&mut self, from: usize, record: bool, depth: usize) -> Ext {
        if self.deeper(depth) {
            return Ext::Bad;
        }
        let t = self.t;
        let mut nesting = 1usize;
        let mut pass_character = false;
        let mut i = from;
        while let Some(&c) = t.get(i) {
            if pass_character {
                pass_character = false;
                i += 1;
                continue;
            }
            if c == CTLESC || c == '\\' {
                pass_character = true;
                i += 1;
                continue;
            }
            if c == '$' && t.get(i + 1) == Some(&'{') {
                nesting += 1;
                i += 2;
                continue;
            }
            if c == '}' {
                nesting -= 1;
                if nesting == 0 {
                    return Ext::End(i);
                }
                i += 1;
                continue;
            }
            match c {
                '`' => match string_extract_backquote(t, i + 1) {
                    Some(end) => {
                        self.record(record, true, i, end);
                        i = end + 1;
                    }
                    None => return Ext::Bad,
                },
                '$' if t.get(i + 1) == Some(&'(') => match self.delimited(i + 2, depth + 1) {
                    Ext::End(end) => {
                        self.record(record, false, i, end);
                        i = end + 1;
                    }
                    Ext::Bad => return Ext::Bad,
                },
                '\'' => i = skip_single_quoted(t, i + 1),
                '"' => match self.skip_double_quoted(i + 1, record, depth + 1) {
                    Some(end) => i = end,
                    None => return Ext::Bad,
                },
                _ => i += 1,
            }
        }
        Ext::Bad
    }
}

/// `string_extract (string, &si, "`", …)`: the index of the closing backtick, which a
/// backslash escapes; `None` at the end of the text.
fn string_extract_backquote(t: &[char], from: usize) -> Option<usize> {
    let mut i = from;
    while let Some(&c) = t.get(i) {
        match c {
            '\\' if i + 1 < t.len() => i += 2,
            '\\' => return None,
            '`' => return Some(i),
            _ => i += 1,
        }
    }
    None
}

/// `skip_single_quoted (string, slen, sind)`: the index after the closing `'`, or the end.
fn skip_single_quoted(t: &[char], from: usize) -> usize {
    match t[from.min(t.len())..].iter().position(|&c| c == '\'') {
        Some(n) => from + n + 1,
        None => t.len(),
    }
}

/// `de_backslash (string)`: removes the backslashes quoting a backtick, `$` or backslash in
/// a backquoted command before bash runs it. In double quotes, `string_extract_double_quoted`
/// dropped the backslash of `\"` before.
fn backquoted_text(t: &[char], in_dquote: bool) -> Vec<char> {
    let mut out = Vec::with_capacity(t.len());
    let mut i = 0;
    while let Some(&c) = t.get(i) {
        let next = t.get(i + 1).copied();
        if c == '\\' && (matches!(next, Some('`' | '\\' | '$')) || in_dquote && next == Some('"')) {
            i += 1;
        }
        out.extend(t.get(i));
        i += 1;
    }
    out
}

/// A here-document waiting for the newline after which its body starts.
struct Heredoc {
    /// The delimiter after quote removal.
    delimiter: Vec<char>,
    /// A quoted delimiter makes the body literal, read without joining lines.
    quoted: bool,
    /// `<<-`: leading tabs are stripped.
    strip_tabs: bool,
}

/// A word as bash 3.2's `read_token_word` builds it.
#[derive(Default)]
struct Word {
    /// The word's text: backslash-newlines removed, `$'…'` translated, `CTLESC` added.
    text: Vec<char>,
    constructs: Vec<Construct>,
}

/// Reads a program as bash 3.2 does, checking each construct as it goes.
struct Reader<'a> {
    b: &'a [char],
    /// Command substitutions this text is nested in.
    depth: usize,
}

impl Reader<'_> {
    fn program(&self) -> Result<(), &'static str> {
        match self.walk() {
            Ok(()) | Err(Stop::Eof) => Ok(()),
            Err(Stop::Diverges(why)) => Err(why),
        }
    }

    /// bash 3.2's `read_token`, token by token; here-document bodies are read after the
    /// newline that ends their line (`gather_here_documents`).
    fn walk(&self) -> Result<(), Stop> {
        let b = self.b;
        let mut pending: Vec<Heredoc> = Vec::new();
        let mut i = 0;
        loop {
            // Blanks, and backslash-newlines that `shell_getc` removes.
            loop {
                i = skip_continuations(b, i);
                if !matches!(b.get(i), Some(' ' | '\t')) {
                    break;
                }
                i += 1;
            }
            let Some(&c) = b.get(i) else {
                return Ok(());
            };
            let next_at = skip_continuations(b, i + 1);
            let next = b.get(next_at).copied();
            match c {
                // A comment, up to the newline.
                '#' => {
                    i = b[i..]
                        .iter()
                        .position(|&c| c == '\n')
                        .map_or(b.len(), |n| i + n);
                }
                '\n' => {
                    i += 1;
                    for doc in pending.drain(..) {
                        let (body, end, ended) = read_body(b, i, &doc);
                        i = end;
                        if !doc.quoted {
                            self.expand_body(&body)?;
                        }
                        if !ended {
                            // The rest of the text is the body.
                            return Ok(());
                        }
                    }
                }
                '(' if next == Some('(') => i = self.arithmetic_command(i, next_at)?,
                ';' | '&' | '|' | '(' | ')' => i += 1,
                // Process substitution is read as a word.
                '<' | '>' if next == Some('(') => self.word(&mut i)?,
                '<' if next == Some('<') => {
                    let third_at = skip_continuations(b, next_at + 1);
                    match b.get(third_at) {
                        // A here-string: its word follows.
                        Some('<') => i = third_at + 1,
                        third => {
                            let strip_tabs = third == Some(&'-');
                            i = if strip_tabs {
                                third_at + 1
                            } else {
                                next_at + 1
                            };
                            if let Some(doc) = self.heredoc(&mut i, strip_tabs)? {
                                pending.push(doc);
                            }
                        }
                    }
                }
                '<' | '>' => i += 1,
                _ => self.word(&mut i)?,
            }
        }
    }

    /// The delimiter word of a here-document operator just read, if there is one.
    fn heredoc(&self, i: &mut usize, strip_tabs: bool) -> Result<Option<Heredoc>, Stop> {
        let b = self.b;
        loop {
            *i = skip_continuations(b, *i);
            if !matches!(b.get(*i), Some(' ' | '\t')) {
                break;
            }
            *i += 1;
        }
        let word_starts = b
            .get(*i)
            .is_some_and(|c| !matches!(c, '\n' | ';' | '&' | '|' | '(' | ')' | '<' | '>'));
        if !word_starts {
            return Ok(None);
        }
        let word = self.read_word(i)?;
        let quoted = word.text.iter().any(|c| matches!(c, '\'' | '"' | '\\'));
        Ok(Some(Heredoc {
            delimiter: quote_removal(&word.text),
            quoted,
            strip_tabs,
        }))
    }

    /// `parse_dparen` at a `((` (at `b[i]`, its second `(` at `b[second]`): an arithmetic
    /// command if the matching `)` is followed by another, which is skipped; otherwise bash
    /// reads the text again as a nested subshell.
    ///
    /// The substitutions in an arithmetic command are not checked here. That is safe only
    /// because the analysis makes every arithmetic command undecomposable, and roughly
    /// scans its text (`parse.rs`, `control_flow`).
    fn arithmetic_command(&self, i: usize, second: usize) -> Result<usize, Stop> {
        let inner = pair(self.b, second + 1, None, '(', ')', Flags::default(), 0)?;
        if self.b.get(inner.end + 1) == Some(&')') {
            Ok(inner.end + 2)
        } else {
            Ok(i + 1)
        }
    }

    /// Reads a word and checks its constructs.
    fn word(&self, i: &mut usize) -> Result<(), Stop> {
        let word = self.read_word(i)?;
        self.check(&word)
    }

    /// bash 3.2's `read_token_word`, from `b[*i]`: builds the word's text and notes its
    /// constructs.
    fn read_word(&self, i: &mut usize) -> Result<Word, Stop> {
        let b = self.b;
        let mut w = Word::default();
        while let Some(&c) = b.get(*i) {
            let next_at = skip_continuations(b, *i + 1);
            let next = b.get(next_at).copied();
            match c {
                '\\' => {
                    *i += 1;
                    if b.get(*i) == Some(&'\n') {
                        // A backslash-newline is removed.
                        *i += 1;
                        continue;
                    }
                    w.text.push('\\');
                    // got_escaped_character
                    if let Some(&n) = b.get(*i) {
                        if n == CTLESC || n == CTLNUL {
                            w.text.push(CTLESC);
                        }
                        w.text.push(n);
                        *i += 1;
                    }
                }
                '\'' | '"' | '`' => {
                    let flags = Flags {
                        command: c == '`',
                        ..Flags::default()
                    };
                    let inner = pair(b, *i + 1, Some(c), c, c, flags, 0)?;
                    let base = w.text.len() + 1;
                    if c == '`' {
                        w.constructs.push(Construct {
                            kind: Kind::Backtick,
                            raw: (*i, inner.end),
                            text: (base - 1, base + inner.text.len() - 1),
                            in_dquote: false,
                        });
                    }
                    w.constructs
                        .extend(inner.nested.iter().map(|n| n.shifted(base)));
                    w.text.push(c);
                    w.text.extend(inner.text);
                    *i = inner.end + 1;
                }
                // `$(…)`, `<(…)`, `>(…)`, `$((…))`, `${…}` and `$[…]`.
                '$' | '<' | '>'
                    if next == Some('(') || c == '$' && matches!(next, Some('{' | '[')) =>
                {
                    let open = next.unwrap_or('(');
                    let (close, flags, kind) = match (c, open) {
                        ('$', '{') => (
                            '}',
                            Flags {
                                first_close: true,
                                ..Flags::default()
                            },
                            Some(Kind::Brace),
                        ),
                        ('$', '[') => (']', Flags::default(), None),
                        (_, _) => (
                            ')',
                            Flags {
                                command: true,
                                ..Flags::default()
                            },
                            Some(if c == '$' {
                                Kind::Substitution
                            } else {
                                Kind::Process
                            }),
                        ),
                    };
                    let inner = pair(b, next_at + 1, None, open, close, flags, 0)?;
                    let start = w.text.len();
                    w.text.extend([c, open]);
                    w.text.extend(inner.text);
                    if let Some(kind) = kind {
                        w.constructs.push(Construct {
                            kind,
                            raw: (*i, inner.end),
                            text: (start, w.text.len() - 1),
                            in_dquote: false,
                        });
                    }
                    *i = inner.end + 1;
                }
                // `$'…'` and `$"…"`.
                '$' if matches!(next, Some('\'' | '"')) => {
                    let q = next.unwrap_or('"');
                    let flags = Flags {
                        allow_esc: q == '\'',
                        ..Flags::default()
                    };
                    let inner = pair(b, next_at + 1, Some(q), q, q, flags, 0)?;
                    let body = &inner.text[..inner.text.len() - 1];
                    if q == '\'' {
                        w.text.extend(sh_single_quote(&ansiexpand(body)));
                    } else {
                        let base = w.text.len() + 1;
                        w.constructs
                            .extend(inner.nested.iter().map(|n| n.shifted(base)));
                        w.text.push('"');
                        w.text.extend(body);
                        w.text.push('"');
                    }
                    *i = inner.end + 1;
                }
                '$' if next == Some('$') => {
                    w.text.extend(['$', '$']);
                    *i = next_at + 1;
                }
                ' ' | '\t' | '\n' | ';' | '&' | '|' | '(' | ')' | '<' | '>' => break,
                _ => {
                    if c == CTLESC || c == CTLNUL {
                        w.text.push(CTLESC);
                    }
                    w.text.push(c);
                    *i += 1;
                }
            }
        }
        Ok(w)
    }

    /// Checks the constructs of a word: where bash 3.2 ends each at parse time must be
    /// where brush-parser ends it and where bash 3.2 finds it again when it expands the
    /// word; then the text it runs is checked as a program.
    fn check(&self, word: &Word) -> Result<(), Stop> {
        let t = &word.text;
        for c in &word.constructs {
            let (start, end) = c.text;
            match c.kind {
                Kind::Substitution => {
                    if !brush_reads_one_word(&self.b[c.raw.0..=c.raw.1]) {
                        return Err(Stop::Diverges(ENDS_ELSEWHERE));
                    }
                    match Extract::new(t).delimited(start + 2, 0) {
                        Ext::End(e) if e == end => {}
                        // The command does not run: nothing more of this word matters.
                        Ext::Bad => return Ok(()),
                        Ext::End(_) => return Err(Stop::Diverges(EXPANDS_ELSEWHERE)),
                    }
                    self.run(&t[start + 2..end])?;
                }
                Kind::Process => self.run(&t[start + 2..end])?,
                Kind::Brace => {
                    let brace = &t[start..=end];
                    let nests = brace.contains(&'`') || brace.windows(2).any(|w| w == ['$', '(']);
                    if !nests {
                        continue;
                    }
                    if !brush_reads_one_word(&self.b[c.raw.0..=c.raw.1]) {
                        return Err(Stop::Diverges(BRACE_ELSEWHERE));
                    }
                    let mut extract = Extract::new(t);
                    match extract.dollar_brace(start + 2, true, 0) {
                        Ext::End(e) if e == end && !extract.too_deep => {}
                        Ext::Bad if !extract.too_deep => return Ok(()),
                        _ => return Err(Stop::Diverges(BRACE_ELSEWHERE)),
                    }
                    self.expanded(t, &extract.found, false)?;
                }
                Kind::Backtick => {
                    self.run(&backquoted_text(&t[start + 1..end], c.in_dquote))?;
                }
            }
        }
        Ok(())
    }

    /// Checks the substitutions bash 3.2 found in expanded text `t` (a here-document body,
    /// or the value of a `${…}`): brush-parser's word parser, which reads that text for the
    /// analysis, must end each at the same `)`.
    fn expanded(&self, t: &[char], found: &[Found], heredoc: bool) -> Result<(), Stop> {
        for f in found {
            if f.backtick {
                self.run(&backquoted_text(&t[f.at + 1..f.end], false))?;
                continue;
            }
            let text = &t[f.at + 2..f.end];
            // `param_expand`: a `$(…)` whose text is in parentheses is arithmetic, and only
            // the substitutions in it run.
            let arithmetic = text.first() == Some(&'(') && text.last() == Some(&')');
            if !arithmetic && !brush_reads_one_substitution(&t[f.at..=f.end], heredoc) {
                return Err(Stop::Diverges(EXPANDS_ELSEWHERE));
            }
            self.run(text)?;
        }
        Ok(())
    }

    /// bash 3.2's `expand_word_internal` over the body of a here-document with an unquoted
    /// delimiter (`Q_HERE_DOCUMENT`): quotes are literal, and a backslash quotes only `$`,
    /// a backtick or a backslash.
    fn expand_body(&self, body: &[char]) -> Result<(), Stop> {
        let mut i = 0;
        while let Some(&c) = body.get(i) {
            let next = body.get(i + 1);
            match c {
                '\\' => i += 2,
                '`' => {
                    let Some(end) = string_extract_backquote(body, i + 1) else {
                        return Ok(());
                    };
                    self.run(&backquoted_text(&body[i + 1..end], false))?;
                    i = end + 1;
                }
                '$' if next == Some(&'(') => {
                    let mut extract = Extract::new(body);
                    match extract.delimited(i + 2, 0) {
                        Ext::End(end) if !extract.too_deep => {
                            let found = Found {
                                backtick: false,
                                at: i,
                                end,
                            };
                            self.expanded(body, &[found], true)?;
                            i = end + 1;
                        }
                        Ext::Bad if !extract.too_deep => return Ok(()),
                        _ => return Err(Stop::Diverges(TOO_DEEP)),
                    }
                }
                '$' if next == Some(&'{') => {
                    let mut extract = Extract::new(body);
                    match extract.dollar_brace(i + 2, true, 0) {
                        Ext::End(end) if !extract.too_deep => {
                            self.expanded(body, &extract.found, false)?;
                            i = end + 1;
                        }
                        Ext::Bad if !extract.too_deep => return Ok(()),
                        _ => return Err(Stop::Diverges(TOO_DEEP)),
                    }
                }
                _ => i += 1,
            }
        }
        Ok(())
    }

    /// Checks `text`, which bash 3.2 runs as a program (`parse_and_execute`). A syntax error
    /// there only ends that program, after what came before it ran.
    fn run(&self, text: &[char]) -> Result<(), Stop> {
        if self.depth >= argv::MAX_DEPTH {
            return Err(Stop::Diverges(TOO_DEEP));
        }
        let nested = Reader {
            b: text,
            depth: self.depth + 1,
        };
        match nested.walk() {
            Ok(()) | Err(Stop::Eof) => Ok(()),
            Err(diverges) => Err(diverges),
        }
    }
}

/// Reads a here-document body from `b[from]` as bash 3.2's `make_here_document` does: line
/// by line, joining continuation lines when the delimiter is unquoted, up to a line equal
/// to the delimiter. Returns the body, the index after it, and whether the delimiter line
/// was found.
fn read_body(b: &[char], from: usize, doc: &Heredoc) -> (Vec<char>, usize, bool) {
    let mut body = Vec::new();
    let mut at = from;
    while at < b.len() {
        // read_a_line (remove_quoted_newline)
        let mut line = Vec::new();
        let mut pass_next = false;
        while let Some(&c) = b.get(at) {
            at += 1;
            if pass_next {
                pass_next = false;
                line.push(c);
            } else if c == '\\' && !doc.quoted {
                if b.get(at) == Some(&'\n') {
                    at += 1;
                    continue;
                }
                pass_next = true;
                line.push(c);
            } else {
                line.push(c);
            }
            if c == '\n' && !pass_next {
                break;
            }
        }
        let text = line.strip_suffix(&['\n']).unwrap_or(&line);
        if text == doc.delimiter.as_slice() {
            return (body, at, true);
        }
        let text = if doc.strip_tabs {
            let tabs = text.iter().take_while(|&&c| c == '\t').count();
            &text[tabs..]
        } else {
            text
        };
        if text == doc.delimiter.as_slice() {
            return (body, at, true);
        }
        body.extend(text);
        body.push('\n');
    }
    (body, at, false)
}

/// The here-document delimiter word `word`, as written, after bash's quote removal
/// (`string_quote_removal`), which is what bash compares the body's lines with.
pub(crate) fn delimiter(word: &str) -> String {
    let w: Vec<char> = word.chars().collect();
    quote_removal(&w).into_iter().collect()
}

/// bash's `string_quote_removal (w, 0)`: an escaped character stands for itself, single
/// quotes keep their text, and in double quotes a backslash escapes only `$`, a backtick,
/// `"`, a backslash or a newline.
fn quote_removal(w: &[char]) -> Vec<char> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(&c) = w.get(i) {
        i += 1;
        match c {
            '\\' => {
                // A trailing backslash stays.
                out.push(w.get(i).copied().unwrap_or('\\'));
                i += 1;
            }
            '\'' => {
                let end = w[i..]
                    .iter()
                    .position(|&c| c == '\'')
                    .map_or(w.len(), |n| i + n);
                out.extend(&w[i..end]);
                i = end + 1;
            }
            '"' => {
                while let Some(&d) = w.get(i).filter(|&&d| d != '"') {
                    i += 1;
                    if d == '\\' && matches!(w.get(i), Some('$' | '`' | '"' | '\\' | '\n')) {
                        out.extend(w.get(i));
                        i += 1;
                    } else {
                        out.push(d);
                    }
                }
                i += 1;
            }
            _ => out.push(c),
        }
    }
    out
}

/// Whether brush-parser's tokenizer reads `written`, a construct starting at its `$`, as
/// one word ending where `written` ends. Text that could make the tokenizer loop or panic
/// is refused by the same screen as the program itself.
fn brush_reads_one_word(written: &[char]) -> bool {
    let written: String = written.iter().collect();
    if unsafe_heredoc(&written).is_some() {
        return false;
    }
    let options = parser_options().tokenizer_options();
    let Some(Ok(tokens)) = guarded(|| brush_parser::tokenize_str_with_options(&written, &options))
    else {
        return false;
    };
    let len = written.chars().count();
    matches!(
        tokens.as_slice(),
        [brush_parser::Token::Word(_, span)] if span.start.index == 0 && span.end.index == len
    )
}

/// Whether brush-parser's word parser (for a here-document body when `heredoc`) reads
/// `written`, a `$(…)`, as one command substitution spanning all of it.
fn brush_reads_one_substitution(written: &[char], heredoc: bool) -> bool {
    let written: String = written.iter().collect();
    let Ok(Some(pieces)) = argv::parse(&written, heredoc) else {
        return false;
    };
    matches!(
        pieces.as_slice(),
        [piece] if matches!(piece.piece, WordPiece::CommandSubstitution(_))
            && piece.start_index == 0
            && piece.end_index == written.len()
    )
}

/// Whether `line` may hold a comment that ends in an odd number of backslashes.
fn comment_continues(line: &str) -> bool {
    let b = line.as_bytes();
    argv::continued(line)
        && b.iter()
            .enumerate()
            .any(|(k, &c)| c == b'#' && (k == 0 || b" \t;&|()<>".contains(&b[k - 1])))
}

/// Where bash 5 ends a `$(…)` whose text starts at `t[from]`: the index of its `)`, or
/// `None` if it does not end in `t`. bash 5 parses the text as a program; this follows
/// what decides where it ends: quotes, nested parentheses and substitutions, here-document
/// bodies, and comments, which start at any word.
fn ends5(t: &[char], from: usize, depth: usize) -> Option<usize> {
    if depth > MAX_NESTING {
        return None;
    }
    let mut pending: Vec<Heredoc> = Vec::new();
    let mut parens = 1usize;
    let mut word_start = true;
    let mut i = from;
    while let Some(&c) = t.get(i) {
        let next = t.get(i + 1).copied();
        match c {
            '\\' if next == Some('\n') => {
                i += 2;
                continue;
            }
            '\\' => i += 2,
            '#' if word_start => {
                i = t[i..]
                    .iter()
                    .position(|&c| c == '\n')
                    .map_or(t.len(), |n| i + n);
                continue;
            }
            '\'' => {
                i = t[i + 1..]
                    .iter()
                    .position(|&c| c == '\'')
                    .map(|n| i + n + 2)?
            }
            '$' if next == Some('\'') => i = ansi_c_end(t, i + 2)? + 1,
            '"' => i = dquote_end5(t, i + 1, depth + 1)?,
            '`' => i = string_extract_backquote(t, i + 1)? + 1,
            '$' if next == Some('(') && t.get(i + 2) == Some(&'(') => {
                i = arithmetic_end(t, i + 3, depth + 1)?;
            }
            '$' if next == Some('(') => i = ends5(t, i + 2, depth + 1)? + 1,
            '$' if next == Some('{') => i = brace_end5(t, i + 2, depth + 1)? + 1,
            '(' if word_start && next == Some('(') => i = arithmetic_end(t, i + 2, depth + 1)?,
            '(' => {
                parens += 1;
                i += 1;
            }
            ')' => {
                parens -= 1;
                if parens == 0 {
                    return Some(i);
                }
                i += 1;
            }
            '<' if next == Some('<') && t.get(i + 2) == Some(&'<') => i += 3,
            '<' if next == Some('<') => {
                let strip_tabs = t.get(i + 2) == Some(&'-');
                i += 2 + usize::from(strip_tabs);
                while matches!(t.get(i), Some(' ' | '\t')) {
                    i += 1;
                }
                let start = i;
                i += delimiter_len(&t[i..]);
                let word = &t[start..i];
                if !word.is_empty() {
                    pending.push(Heredoc {
                        delimiter: quote_removal(word),
                        quoted: word.iter().any(|c| matches!(c, '\'' | '"' | '\\')),
                        strip_tabs,
                    });
                }
                word_start = true;
                continue;
            }
            '\n' => {
                i += 1;
                for doc in pending.drain(..) {
                    let (_, end, ended) = read_body(t, i, &doc);
                    if !ended {
                        return None;
                    }
                    i = end;
                }
            }
            _ => i += 1,
        }
        word_start = matches!(
            c,
            ' ' | '\t' | '\n' | ';' | '&' | '|' | '(' | ')' | '<' | '>'
        );
    }
    None
}

/// The index of the `'` closing a `$'…'` string whose text starts at `t[from]`.
fn ansi_c_end(t: &[char], from: usize) -> Option<usize> {
    let mut i = from;
    while let Some(&c) = t.get(i) {
        match c {
            '\\' => i += 2,
            '\'' => return Some(i),
            _ => i += 1,
        }
    }
    None
}

/// The index after the `"` closing a double-quoted string whose text starts at `t[from]`,
/// its substitutions read as bash 5 reads them.
fn dquote_end5(t: &[char], from: usize, depth: usize) -> Option<usize> {
    if depth > MAX_NESTING {
        return None;
    }
    let mut i = from;
    while let Some(&c) = t.get(i) {
        let next = t.get(i + 1).copied();
        match c {
            '\\' => i += 2,
            '"' => return Some(i + 1),
            '`' => i = string_extract_backquote(t, i + 1)? + 1,
            '$' if next == Some('(') && t.get(i + 2) == Some(&'(') => {
                i = arithmetic_end(t, i + 3, depth + 1)?;
            }
            '$' if next == Some('(') => i = ends5(t, i + 2, depth + 1)? + 1,
            '$' if next == Some('{') => i = brace_end5(t, i + 2, depth + 1)? + 1,
            _ => i += 1,
        }
    }
    None
}

/// The index of the `}` closing a `${…}` whose text starts at `t[from]`.
fn brace_end5(t: &[char], from: usize, depth: usize) -> Option<usize> {
    if depth > MAX_NESTING {
        return None;
    }
    let mut i = from;
    while let Some(&c) = t.get(i) {
        let next = t.get(i + 1).copied();
        match c {
            '\\' => i += 2,
            '}' => return Some(i),
            '\'' => {
                i = t[i + 1..]
                    .iter()
                    .position(|&c| c == '\'')
                    .map(|n| i + n + 2)?
            }
            '"' => i = dquote_end5(t, i + 1, depth + 1)?,
            '`' => i = string_extract_backquote(t, i + 1)? + 1,
            '$' if next == Some('(') => i = ends5(t, i + 2, depth + 1)? + 1,
            '$' if next == Some('{') => i = brace_end5(t, i + 2, depth + 1)? + 1,
            _ => i += 1,
        }
    }
    None
}

/// The index after the `))` closing arithmetic whose text starts at `t[from]`, after its
/// `((`: parentheses are counted and quotes skipped. `depth` counts the constructs it is
/// nested in.
fn arithmetic_end(t: &[char], from: usize, depth: usize) -> Option<usize> {
    if depth > MAX_NESTING {
        return None;
    }
    let mut parens = 0usize;
    let mut i = from;
    while let Some(&c) = t.get(i) {
        match c {
            '\\' => i += 1,
            '\'' => i += t[i + 1..].iter().position(|&c| c == '\'')? + 1,
            '"' => i = dquote_end5(t, i + 1, depth + 1)? - 1,
            '(' => parens += 1,
            ')' if parens > 0 => parens -= 1,
            ')' => return (t.get(i + 1) == Some(&')')).then_some(i + 2),
            _ => {}
        }
        i += 1;
    }
    None
}

/// The length of the here-document delimiter word at the start of `t`: up to a blank,
/// newline or operator character outside quotes.
fn delimiter_len(t: &[char]) -> usize {
    let mut i = 0;
    while let Some(&c) = t.get(i) {
        match c {
            ' ' | '\t' | '\n' | ';' | '&' | '|' | '(' | ')' | '<' | '>' => break,
            '\\' => i += 2,
            '\'' | '"' => {
                i += 1;
                while t.get(i).is_some_and(|&d| d != c) {
                    i += 1 + usize::from(c == '"' && t[i] == '\\');
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    i.min(t.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chars(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    /// Where `parse_matched_pair` ends a `$(…)` whose text is `s`, and the text it keeps.
    fn paren(s: &str, flags: Flags) -> Result<(usize, String), &'static str> {
        let b = chars(s);
        match pair(&b, 0, None, '(', ')', flags, 0) {
            Ok(p) => Ok((p.end, p.text.iter().collect())),
            Err(Stop::Eof) => Err("EOF"),
            Err(Stop::Diverges(why)) => Err(why),
        }
    }

    fn command() -> Flags {
        Flags {
            command: true,
            ..Flags::default()
        }
    }

    #[test]
    fn pair_matches_bash_3_2() {
        // The quote in the body opens a string: bash 3.2 reaches the end of input.
        assert_eq!(paren("cat <<'EOF'\nit's\nEOF\n)", command()), Err("EOF"));
        // No comment after `;`.
        assert_eq!(paren("true;# )\n)", command()).map(|p| p.0), Ok(7));
        // A comment after a blank, but not inside double quotes.
        assert_eq!(paren("true # )\n)", command()).map(|p| p.0), Ok(9));
        let dquote = Flags {
            dquote: true,
            ..command()
        };
        assert_eq!(paren("true # )\n)", dquote).map(|p| p.0), Ok(7));
        // A backslash-newline is removed, except in single quotes.
        assert_eq!(paren("a\\\nb)", command()).map(|p| p.1), Ok("ab)".into()));
        assert_eq!(
            paren("'a\\\nb')", command()).map(|p| p.1),
            Ok("'a\\\nb')".into())
        );
        // `${…}` is not followed: its `)` ends the `$(…)`.
        assert_eq!(paren("echo ${y:-)})", command()).map(|p| p.0), Ok(10));
        // `$'…'` is translated and single-quoted.
        assert_eq!(
            paren("echo $'it\\'s')", command()).map(|p| p.1),
            Ok("echo 'it'\\''s')".into())
        );
    }

    /// Where `extract_delimited_string` ends the `$(…)` at the start of `s`, and the
    /// substitutions a `${…}` there holds.
    fn extract(s: &str) -> Ext {
        let t = chars(s);
        Extract::new(&t).delimited(2, 0)
    }

    #[test]
    fn extract_matches_bash_3_2() {
        assert_eq!(extract("$( # c) \nzz\n)"), Ext::End(12));
        // A comment is not seen right after the `(`.
        assert_eq!(extract("$(# c)"), Ext::End(5));
        assert_eq!(extract("$(a;# c)\n)"), Ext::End(7));
        // Quotes and nested substitutions are skipped, comments included there.
        assert_eq!(extract("$(echo \"$( # )\n)\" ')')"), Ext::End(21));
        assert_eq!(extract("$( # c)"), Ext::Bad);
        let t = chars("${x:-$( # c)\nzz\n)`a`\"$(b)\"}");
        let mut e = Extract::new(&t);
        assert_eq!(e.dollar_brace(2, true, 0), Ext::End(26));
        let found: Vec<_> = e.found.iter().map(|f| (f.backtick, f.at, f.end)).collect();
        assert_eq!(found, [(false, 5, 16), (true, 17, 19), (false, 21, 24)]);
    }

    #[test]
    fn bash_5_ends_substitutions_at_comments() {
        let end = |s: &str| ends5(&chars(s), 2, 0);
        assert_eq!(end("$( # c)\nzz\n)"), Some(11));
        assert_eq!(end("$(#c)\n)"), Some(6));
        assert_eq!(end("$(a;#c)\n)"), Some(8));
        assert_eq!(end("$(a#b)"), Some(5));
        assert_eq!(end("$(echo ${#x})"), Some(12));
        // Here-document bodies are skipped, their quotes and comments too.
        assert_eq!(end("$(cat <<'EOF'\nit's # )\nEOF\n)"), Some(27));
        assert_eq!(end("$(cat <<-EOF\n\tit's\n\tEOF\n)"), Some(24));
        assert_eq!(end("$(echo $((1<<2)))"), Some(16));
        assert_eq!(end("$(cat <<EOF\nit's\n)"), None);
        // Nesting past the limit is not followed, through arithmetic and quotes too.
        let nest = |n: usize| {
            let mut inner = "1".to_string();
            for _ in 0..n {
                inner = format!("$((\"{inner}\"))");
            }
            format!("$(echo {inner})")
        };
        assert_eq!(end(&nest(10)), Some(nest(10).chars().count() - 1));
        assert_eq!(end(&nest(MAX_NESTING)), None);
        assert_eq!(substitution_misread(" # c"), Some(COMMENT_MISREAD));
        assert_eq!(substitution_misread("true;# c"), Some(COMMENT_MISREAD));
        assert_eq!(substitution_misread("echo a # c\n"), None);
        assert_eq!(substitution_misread("git log --format='#%h'"), None);
        assert_eq!(substitution_misread(" # c \\\n x"), Some(COMMENT_CONTINUES));
    }

    #[test]
    fn delimiters_after_quote_removal() {
        for (word, removed) in [
            ("EOF", "EOF"),
            ("'EOF'", "EOF"),
            ("'E'\\\\OF", "E\\OF"),
            ("\"E\\\\OF\"", "E\\OF"),
            ("\"E\\OF\"", "E\\OF"),
            ("E\\\"F", "E\"F"),
            ("'E\"F'", "E\"F"),
            ("EOF\\", "EOF\\"),
        ] {
            assert_eq!(delimiter(word), removed, "{word:?}");
        }
    }

    #[test]
    fn programs_bash_3_2_reads_differently() {
        for agrees in [
            "echo \"$(git status)\"",
            "git commit -m \"$(cat <<'EOF'\nIt's fixed\nEOF\n)\"",
            "x=$(cat <<'EOF'\nFix (closes #12)\nEOF\n)",
            "echo ${x:-$(pwd)} `date` \"`echo \\\"a\\\"`\"",
            "cat <<EOF\n$(date) ${HOME:-$(pwd)}\nEOF",
            "echo \"$(echo a;# note\n)\"",
            "(( x = 1 << 2 )); echo $(true)",
            // `$((…))` in expanded text is arithmetic.
            "echo ${x:$((n-1)):1} $(( ${#a[@]} - 1 ))",
            "cat <<EOF\n$((1 + 2)) ${x:-$((3))}\nEOF",
        ] {
            assert_eq!(divergence(agrees), None, "{agrees:?}");
        }
        for (differs, why) in [
            ("echo $(cat <<EOF)\nx\nEOF", ENDS_ELSEWHERE),
            ("echo $(true;# ); x\n)", ENDS_ELSEWHERE),
            ("echo \"$( # c)\nx\n)\"", EXPANDS_ELSEWHERE),
            ("echo \"$( # c)\"'\nx\n)'", EXPANDS_ELSEWHERE),
            ("echo ${x:-$( # c)}'\nx\n)}'", BRACE_ELSEWHERE),
            // Text brush-parser must not be given counts as read differently.
            ("echo ${x:-$(a <<)}", BRACE_ELSEWHERE),
            ("echo `echo $(true;# ); x\n)`", ENDS_ELSEWHERE),
            ("cat <<EOF\n$( # c)\nx\n)\nEOF", EXPANDS_ELSEWHERE),
            ("echo $( # \\\n x ;)", COMMENT_CONTINUES),
        ] {
            assert_eq!(divergence(differs), Some(why), "{differs:?}");
        }
    }
}
