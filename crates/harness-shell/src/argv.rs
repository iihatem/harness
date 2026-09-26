//! Shell words → argv tokens, plus the canonical quoting used for display and rule matching.

use std::borrow::Cow;

use brush_parser::word::{Parameter, ParameterExpr, WordPiece, WordPieceWithSource};

/// Nesting limit for command substitutions, `sh -c`, `eval` and similar re-parsing.
pub(crate) const MAX_DEPTH: usize = 8;

/// One argv element after quote removal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Tok {
    /// Fully known text.
    Lit(String),
    /// Pathname expansion whose matches all lie below the literal directory `dir`
    /// (`""` is the current directory): `target/*.o` has `dir == "target/"`.
    Glob { dir: String },
    /// Only known at run time: parameter, command, tilde, brace or arithmetic
    /// expansion, or an undecodable `$'…'` string.
    Dyn,
}

impl Tok {
    pub(crate) fn lit(&self) -> Option<&str> {
        match self {
            Tok::Lit(s) => Some(s),
            _ => None,
        }
    }
}

/// Parser options matching a non-interactive `bash -c`.
pub(crate) fn parser_options() -> brush_parser::ParserOptions {
    // extglob is off in `bash -c`; with it on, `!(curl x)` would parse as a pattern
    // word instead of a negated subshell that runs curl.
    brush_parser::ParserOptions {
        enable_extended_globbing: false,
        ..Default::default()
    }
}

/// What scanning a word found besides its token.
#[derive(Default)]
pub(crate) struct Scan {
    /// Command substitutions, to be analyzed as sub-commands.
    pub subs: Vec<String>,
    /// Why the text cannot be fully analyzed, although scanning went on.
    pub opaque: Vec<String>,
    /// Text of the word that bash reads literally but may evaluate later.
    pub hidden: Option<Hidden>,
}

/// Literal text in a word (quoted, escaped, or otherwise left unexpanded when bash
/// reads the word) that builtins such as `let`, `declare` or `unset` evaluate again as
/// arithmetic, a subscript or a variable name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Hidden {
    /// A substitution marker (`$(`, `${`, `$[` or a backtick).
    Substitution,
    /// A quoted or escaped `[` or `]`.
    Subscript,
}

/// Converts a raw shell word into a token, recording in `scan` the source of every
/// command substitution it contains (the caller must analyze those too).
pub(crate) fn word_to_tok(raw: &str, scan: &mut Scan) -> Result<Tok, String> {
    let pieces = parse(raw, false)?.ok_or_else(|| format!("cannot parse the word `{raw}`"))?;
    let mut b = Builder::default();
    b.pieces(raw, &pieces, false, scan, 0)?;
    scan.hidden = b.hidden();
    Ok(b.finish())
}

/// Collects the command substitutions of an unquoted-delimiter heredoc body.
pub(crate) fn heredoc_substitutions(body: &str, scan: &mut Scan) -> Result<(), String> {
    let pieces = parse(body, true)?.ok_or("cannot parse a here-document body")?;
    Builder::default().pieces(body, &pieces, true, scan, 0)
}

/// Collects the command substitutions in arithmetic or array-index text.
pub(crate) fn substitutions_in(text: &str, scan: &mut Scan) -> Result<(), String> {
    scan_nested(text, scan, 0)
}

/// Whether `text` contains a substitution marker (`$(`, `${`, `$[` or a backtick).
/// bash may run it even from inside quotes when it re-evaluates the text as arithmetic.
fn subst_marker(text: &str) -> bool {
    text.contains("$(") || text.contains("${") || text.contains("$[") || text.contains('`')
}

/// Scans text that bash evaluates as an arithmetic expression. Unquoted substitutions
/// are collected as usual, but a marker surviving quote removal (single-quoted or
/// escaped) means the arithmetic evaluation can run a command, which is undecomposable.
pub(crate) fn arithmetic_in(text: &str, scan: &mut Scan) -> Result<(), String> {
    arith_scan(text, scan, 0)
}

fn arith_scan(text: &str, scan: &mut Scan, depth: usize) -> Result<(), String> {
    if !text.contains(['$', '`']) {
        return Ok(());
    }
    let mut b = Builder::default();
    match parse(text, false)? {
        Some(pieces) => b.pieces(text, &pieces, false, scan, depth)?,
        None if subst_marker(text) => {
            return Err("arithmetic text may run a command substitution".into());
        }
        None => return Ok(()),
    }
    // A marker left in the resolved text was quoted or escaped, so brush did not expose
    // it as a live substitution, but the arithmetic evaluation still runs it.
    if subst_marker(&b.text) {
        return Err("arithmetic text may run a command substitution".into());
    }
    Ok(())
}

/// Like the marker check of [`arith_scan`], for text whose substitutions were already
/// collected: parses it once and does not descend into its expansions.
fn arith_marker(text: &str) -> Result<(), String> {
    if !text.contains(['$', '`']) {
        return Ok(());
    }
    let hidden = match parse(text, false)? {
        Some(pieces) => {
            let mut literal = String::new();
            literal_text(&pieces, &mut literal);
            subst_marker(&literal)
        }
        None => subst_marker(text),
    };
    if hidden {
        return Err("arithmetic text may run a command substitution".into());
    }
    Ok(())
}

/// The text of `pieces` after quote removal, without the expansions.
fn literal_text(pieces: &[WordPieceWithSource], out: &mut String) {
    for p in pieces {
        match &p.piece {
            WordPiece::Text(t) | WordPiece::SingleQuotedText(t) => out.push_str(t),
            WordPiece::AnsiCQuotedText(t) => out.push_str(&decode_ansi_c(t).unwrap_or_default()),
            WordPiece::DoubleQuotedSequence(inner)
            | WordPiece::GettextDoubleQuotedSequence(inner) => literal_text(inner, out),
            WordPiece::EscapeSequence(e) => out.push_str(e.strip_prefix('\\').unwrap_or(e)),
            _ => {}
        }
    }
}

/// Collects command substitutions nested in expansion text such as `X:-$(cmd)` or an
/// arithmetic expression.
fn scan_nested(text: &str, scan: &mut Scan, depth: usize) -> Result<(), String> {
    if !text.contains(['$', '`']) {
        return Ok(());
    }
    match parse(text, false)? {
        Some(pieces) => Builder::default().pieces(text, &pieces, false, scan, depth),
        None if text.contains("$(") || text.contains('`') => {
            Err(format!("cannot parse the expansion `{text}`"))
        }
        None => Ok(()),
    }
}

/// Parses a word or a here-document body into pieces (`None` if brush-parser rejects
/// it), unless it nests expansions too deeply to parse in bounded time.
fn parse(text: &str, heredoc: bool) -> Result<Option<Vec<WordPieceWithSource>>, String> {
    if let Some(why) = too_nested(text, heredoc) {
        return Err(why.into());
    }
    let options = parser_options();
    let pieces = if heredoc {
        brush_parser::word::parse_heredoc(text, &options)
    } else {
        brush_parser::word::parse(text, &options)
    };
    Ok(pieces.ok())
}

/// Why a `${…}` in [`hides_subscript`] is undecomposable.
const HIDDEN_SUBSCRIPT: &str =
    "a `${…}` subscript contains quoted, escaped or substituted text that bash may evaluate";

/// A `${…}` whose inner text has, after a `[`, a quote, a backslash or a substitution
/// marker hides a subscript that bash evaluates as arithmetic, which can run a command.
fn hides_subscript(inner: &str) -> bool {
    inner.find('[').is_some_and(|at| {
        let rest = &inner[at..];
        rest.contains(['\'', '"', '\\']) || subst_marker(rest)
    })
}

/// More nested `${…}` than this is not handed to brush-parser.
const MAX_BRACES: usize = 4;
/// Constructs a text may leave unterminated (a here-document body can).
const MAX_UNTERMINATED: usize = 1;

/// A construct open at some point of a scan by [`too_nested`].
#[derive(Clone, Copy, PartialEq)]
enum Open {
    /// A shell word; a here-document body keeps its quotes literal.
    Word {
        heredoc: bool,
    },
    DoubleQuote,
    /// `${…}`.
    Brace,
    /// `name[…]` or `$[…]`; `param` for the subscript of a `${name[…]}`.
    Bracket {
        param: bool,
    },
    /// `$(…)` or a parenthesis inside another construct.
    Paren,
}

/// Why brush-parser should not be given `text`, if it should not. Its word grammar
/// re-parses nested text once per alternative it tries: about 15 times for the
/// subscript of a `${name[…]}` (so a `${…}` nested there costs 15 × 15), and twice for
/// each enclosing construct left unterminated. Parse time grows exponentially with
/// that nesting. This scan follows the grammar's quoting in one pass and, where
/// unsure, assumes the deeper reading.
pub(crate) fn too_nested(text: &str, heredoc: bool) -> Option<&'static str> {
    const TOO_DEEP: &str = "expansions are nested too deeply";
    let b = text.as_bytes();
    // The index just past the quote closing a quoted string that starts at `from`.
    let closing = |from: usize, quote: u8, escapes: bool| {
        let mut j = from;
        while let Some(&c) = b.get(j) {
            match c {
                b'\\' if escapes => j += 2,
                _ if c == quote => return Some(j + 1),
                _ => j += 1,
            }
        }
        None
    };
    let mut stack = vec![Open::Word { heredoc }];
    let (mut braces, mut param_subscripts) = (0, 0);
    let mut i = 0;
    while let Some(&c) = b.get(i) {
        let top = *stack.last()?;
        let quotes = !matches!(top, Open::DoubleQuote | Open::Word { heredoc: true });
        let nests = matches!(top, Open::Brace | Open::Bracket { .. } | Open::Paren);
        i += 1;
        match c {
            b'\\' => i += 1,
            b'\'' if quotes => i = closing(i, b'\'', false).unwrap_or(i),
            b'`' => i = closing(i, b'`', true).unwrap_or(i),
            b'$' if b.get(i) == Some(&b'\'') && quotes => {
                i = closing(i + 1, b'\'', true).unwrap_or(i);
            }
            b'$' if b.get(i) == Some(&b'{') => {
                if param_subscripts > 0 {
                    return Some(HIDDEN_SUBSCRIPT);
                }
                i += 1;
                braces += 1;
                if braces > MAX_BRACES {
                    return Some(TOO_DEEP);
                }
                stack.push(Open::Brace);
                let name_at = i + usize::from(matches!(b.get(i), Some(b'!' | b'#')));
                let name = name_len(b, name_at);
                if name > 0 && b.get(name_at + name) == Some(&b'[') {
                    param_subscripts += 1;
                    stack.push(Open::Bracket { param: true });
                    i = name_at + name + 1;
                }
            }
            b'$' if b.get(i) == Some(&b'(') => {
                i += 1;
                stack.push(Open::Paren);
            }
            b'$' if b.get(i) == Some(&b'[') => {
                i += 1;
                stack.push(Open::Bracket { param: false });
            }
            b'"' if top == Open::DoubleQuote => {
                stack.pop();
            }
            b'"' if quotes => stack.push(Open::DoubleQuote),
            b'}' if top == Open::Brace => {
                stack.pop();
                braces -= 1;
            }
            b']' if matches!(top, Open::Bracket { .. }) => {
                if stack.pop() == Some(Open::Bracket { param: true }) {
                    param_subscripts -= 1;
                }
            }
            b')' if top == Open::Paren => {
                stack.pop();
            }
            b'(' if nests => stack.push(Open::Paren),
            b'[' if nests && i >= 2 && (b[i - 2].is_ascii_alphanumeric() || b[i - 2] == b'_') => {
                stack.push(Open::Bracket { param: false });
            }
            _ => {}
        }
    }
    (stack.len() > 1 + MAX_UNTERMINATED).then_some("leaves nested expansions unterminated")
}

/// Like [`too_nested`], for the words brush-parser word-parses while parsing the
/// program itself: those shaped like an array-element assignment (`name[…]…`).
pub(crate) fn program_too_nested(src: &str) -> Option<&'static str> {
    let options = parser_options().tokenizer_options();
    let tokens = brush_parser::tokenize_str_with_options(src, &options).ok()?;
    tokens.iter().find_map(|token| match token {
        brush_parser::Token::Word(w, _) => {
            let name = name_len(w.as_bytes(), 0);
            let assignment = name > 0 && w.as_bytes().get(name) == Some(&b'[');
            if assignment {
                too_nested(w, false)
            } else {
                None
            }
        }
        _ => None,
    })
}

/// Length of the shell variable name starting at `b[from]` (0 if there is none).
fn name_len(b: &[u8], from: usize) -> usize {
    match b.get(from) {
        Some(c) if c.is_ascii_alphabetic() || *c == b'_' => b[from..]
            .iter()
            .take_while(|c| c.is_ascii_alphanumeric() || **c == b'_')
            .count(),
        _ => 0,
    }
}

#[derive(Default)]
struct Builder {
    text: String,
    /// The unquoted parts of the word (brace expansion only happens there).
    unquoted: String,
    dynamic: bool,
    /// Byte offset in `text` of the first unquoted glob character.
    glob_at: Option<usize>,
    /// A `[` or `]` was single-quoted, `$'…'`-quoted or escaped.
    inert_bracket: bool,
}

impl Builder {
    fn pieces(
        &mut self,
        src: &str,
        pieces: &[WordPieceWithSource],
        quoted: bool,
        scan: &mut Scan,
        depth: usize,
    ) -> Result<(), String> {
        if depth > MAX_DEPTH {
            return Err("expansions are nested too deeply".into());
        }
        for p in pieces {
            match &p.piece {
                WordPiece::Text(t) => self.push_text(t, quoted),
                WordPiece::SingleQuotedText(t) => self.push_inert(t),
                WordPiece::AnsiCQuotedText(t) => match decode_ansi_c(t) {
                    Some(s) => self.push_inert(&s),
                    None => self.dynamic = true,
                },
                WordPiece::DoubleQuotedSequence(inner)
                | WordPiece::GettextDoubleQuotedSequence(inner) => {
                    self.pieces(src, inner, true, scan, depth)?;
                }
                // `\x` → `x`; a backslash-newline is a line continuation.
                WordPiece::EscapeSequence(e) if e == "\\\n" => {}
                WordPiece::EscapeSequence(e) => self.push_inert(e.strip_prefix('\\').unwrap_or(e)),
                WordPiece::TildeExpansion(_) => self.dynamic = true,
                WordPiece::ParameterExpansion(pe) => {
                    self.dynamic = true;
                    // `${X:-$(cmd)}` runs cmd: re-parse the braces' content, once.
                    let raw = src.get(p.start_index..p.end_index).unwrap_or_default();
                    if let Some(inner) = raw.strip_prefix("${").and_then(|r| r.strip_suffix('}')) {
                        if hides_subscript(inner) {
                            scan.opaque.push(HIDDEN_SUBSCRIPT.into());
                        }
                        scan_nested(inner, scan, depth + 1)?;
                    }
                    // An array subscript or a substring offset is evaluated as arithmetic
                    // (its substitutions were collected with the braces' content).
                    match pe {
                        ParameterExpr::Parameter {
                            parameter: Parameter::NamedWithIndex { index, .. },
                            ..
                        } => arith_marker(index)?,
                        ParameterExpr::Substring { offset, length, .. } => {
                            arith_marker(&offset.value)?;
                            if let Some(length) = length {
                                arith_marker(&length.value)?;
                            }
                        }
                        _ => {}
                    }
                }
                WordPiece::CommandSubstitution(c) | WordPiece::BackquotedCommandSubstitution(c) => {
                    self.dynamic = true;
                    scan.subs.push(c.clone());
                }
                WordPiece::ArithmeticExpression(e) => {
                    self.dynamic = true;
                    arith_scan(&e.value, scan, depth + 1)?;
                }
            }
        }
        Ok(())
    }

    fn push_inert(&mut self, t: &str) {
        self.inert_bracket |= t.contains(['[', ']']);
        self.text.push_str(t);
    }

    /// Literal text bash may evaluate later: a substitution marker anywhere in the
    /// word's literal text, or a quoted or escaped bracket.
    fn hidden(&self) -> Option<Hidden> {
        if subst_marker(&self.text) {
            Some(Hidden::Substitution)
        } else if self.inert_bracket {
            Some(Hidden::Subscript)
        } else {
            None
        }
    }

    fn push_text(&mut self, t: &str, quoted: bool) {
        if !quoted {
            // `(` only survives parsing inside patterns; `=~`/`:~` tilde-expand in
            // assignment-like words.
            if t.contains('(') || t.contains("=~") || t.contains(":~") {
                self.dynamic = true;
            }
            self.unquoted.push_str(t);
            let bracket = t.find('[').filter(|&i| t[i..].contains(']'));
            let glob = t.find(['*', '?']).into_iter().chain(bracket).min();
            if let (None, Some(i)) = (self.glob_at, glob) {
                self.glob_at = Some(self.text.len() + i);
            }
        }
        self.text.push_str(t);
    }

    fn finish(self) -> Tok {
        if self.dynamic || has_brace_expansion(&self.unquoted) {
            return Tok::Dyn;
        }
        let Some(at) = self.glob_at else {
            return Tok::Lit(self.text);
        };
        let dir_end = self.text[..at].rfind('/').map_or(0, |j| j + 1);
        // A pattern segment starting with `.` (e.g. `.*`) may match `..`.
        if self.text[dir_end..]
            .split('/')
            .any(|seg| seg.starts_with('.'))
        {
            return Tok::Dyn;
        }
        Tok::Glob {
            dir: self.text[..dir_end].to_string(),
        }
    }
}

/// `{a,b}` or `{1..3}` (a lone `{}` stays literal, as `find -exec … {}` relies on).
fn has_brace_expansion(unquoted: &str) -> bool {
    unquoted.match_indices('{').any(|(i, _)| {
        let rest = &unquoted[i + 1..];
        rest.find('}')
            .is_some_and(|end| rest[..end].contains(',') || rest[..end].contains(".."))
    })
}

/// Decodes the body of a bash `$'…'` string. `None` if the result cannot be known
/// statically (NUL truncation, invalid code point, non-UTF-8 bytes).
fn decode_ansi_c(s: &str) -> Option<String> {
    let mut out: Vec<u8> = Vec::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    let mut buf = [0u8; 4];
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        let Some(e) = chars.next() else {
            out.push(b'\\');
            break;
        };
        match e {
            'a' => out.push(0x07),
            'b' => out.push(0x08),
            'e' | 'E' => out.push(0x1b),
            'f' => out.push(0x0c),
            'n' => out.push(b'\n'),
            'r' => out.push(b'\r'),
            't' => out.push(b'\t'),
            'v' => out.push(0x0b),
            '\\' | '\'' | '"' | '?' => out.push(e as u8),
            '0'..='7' => {
                let mut v = e.to_digit(8)?;
                for _ in 0..2 {
                    match chars.peek().and_then(|d| d.to_digit(8)) {
                        Some(d) => {
                            v = v * 8 + d;
                            chars.next();
                        }
                        None => break,
                    }
                }
                out.push((v & 0xff) as u8);
            }
            'x' | 'u' | 'U' => {
                let max = match e {
                    'x' => 2,
                    'u' => 4,
                    _ => 8,
                };
                let mut v: u32 = 0;
                let mut n = 0;
                while n < max {
                    match chars.peek().and_then(|d| d.to_digit(16)) {
                        Some(d) => {
                            v = v.checked_mul(16)? + d;
                            chars.next();
                            n += 1;
                        }
                        None => break,
                    }
                }
                if n == 0 {
                    out.extend_from_slice(&[b'\\', e as u8]);
                } else if e == 'x' {
                    out.push(v as u8);
                } else {
                    out.extend_from_slice(char::from_u32(v)?.encode_utf8(&mut buf).as_bytes());
                }
            }
            'c' => {
                let ctl = chars.next().filter(char::is_ascii)?;
                out.push(ctl as u8 & 0x1f);
            }
            other => {
                out.push(b'\\');
                out.extend_from_slice(other.encode_utf8(&mut buf).as_bytes());
            }
        }
    }
    if out.contains(&0) {
        return None;
    }
    String::from_utf8(out).ok()
}

/// Quotes `s` so the shell would read it back as one word (single quotes when needed).
pub(crate) fn quote(s: &str) -> Cow<'_, str> {
    let safe = !s.is_empty()
        && s.char_indices().all(|(i, c)| {
            c.is_ascii_alphanumeric() || "_-+=:,./@%^".contains(c) || (i > 0 && "~#".contains(c))
        });
    if safe {
        Cow::Borrowed(s)
    } else {
        Cow::Owned(format!("'{}'", s.replace('\'', r"'\''")))
    }
}

/// Human-readable rendering of an argv; run-time values are shown as `…`.
pub(crate) fn display(argv: &[Tok]) -> String {
    argv.iter()
        .map(|t| match t {
            Tok::Lit(s) => quote(s),
            Tok::Glob { .. } | Tok::Dyn => Cow::Borrowed("…"),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Last path component of a command name (`/usr/bin/git` → `git`).
pub(crate) fn basename(s: &str) -> &str {
    s.rsplit('/').next().unwrap_or(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tok(raw: &str) -> Tok {
        word_to_tok(raw, &mut Scan::default()).unwrap()
    }

    #[test]
    fn quote_removal() {
        assert_eq!(tok("c''url"), Tok::Lit("curl".into()));
        assert_eq!(tok(r"\curl"), Tok::Lit("curl".into()));
        assert_eq!(tok(r#""a\"b""#), Tok::Lit("a\"b".into()));
        assert_eq!(tok(r"$'\x63url'"), Tok::Lit("curl".into()));
        assert_eq!(tok(r"$'\101é\cA'"), Tok::Lit("Aé\u{1}".into()));
        assert_eq!(tok(r"$'a\qb'"), Tok::Lit(r"a\qb".into()));
        assert_eq!(tok("HEAD~1"), Tok::Lit("HEAD~1".into()));
        assert_eq!(tok("["), Tok::Lit("[".into()));
        assert_eq!(tok("-I{}"), Tok::Lit("-I{}".into()));
    }

    #[test]
    fn dynamic_words() {
        for raw in [
            "$X",
            "~/x",
            "t{e,}st",
            "{1..3}",
            "{a,\"b\"}",
            "$((1+2))",
            r"$'\x00'",
            "a=~/x",
            "\"$(id)\"",
        ] {
            assert_eq!(tok(raw), Tok::Dyn, "{raw}");
        }
        assert_eq!(
            tok("target/*.o"),
            Tok::Glob {
                dir: "target/".into()
            }
        );
        assert_eq!(tok("*"), Tok::Glob { dir: String::new() });
        assert_eq!(tok("'*'"), Tok::Lit("*".into()));
        assert_eq!(tok(".*"), Tok::Dyn);
    }

    #[test]
    fn nested_substitutions_are_collected() {
        let mut scan = Scan::default();
        word_to_tok("${X:-$(curl a)}$(( $(curl b) ))`curl c`", &mut scan).unwrap();
        assert_eq!(scan.subs, ["curl a", "curl b", "curl c"]);
    }

    #[test]
    fn nesting_limits() {
        for ok in [
            "${a[$i]}",
            "${x:-${y:-${z:-${w}}}}",
            "'${a[${b}]}'",
            "${x:-[a]}",
        ] {
            assert_eq!(too_nested(ok, false), None, "{ok}");
        }
        for deep in ["${a[${i}]}", "\"${x:-${y:-${z:-${w:-${v}}}}}\"", "$(( $(("] {
            assert!(too_nested(deep, false).is_some(), "{deep}");
        }
        // Quotes are literal in a here-document body.
        assert!(too_nested("'${a[${b}]}'", true).is_some());
    }

    #[test]
    fn quoting() {
        assert_eq!(quote("git"), "git");
        assert_eq!(quote("git status"), "'git status'");
        assert_eq!(quote("it's"), r"'it'\''s'");
        assert_eq!(quote(""), "''");
        assert_eq!(quote("~x"), "'~x'");
    }
}
