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

/// Converts a raw shell word into a token, appending the source of every command
/// substitution it contains to `subs` (the caller must analyze those too).
pub(crate) fn word_to_tok(raw: &str, subs: &mut Vec<String>) -> Result<Tok, String> {
    let pieces = brush_parser::word::parse(raw, &parser_options())
        .map_err(|_| format!("cannot parse the word `{raw}`"))?;
    let mut b = Builder::default();
    b.pieces(raw, &pieces, false, subs, 0)?;
    Ok(b.finish())
}

/// Collects the command substitutions of an unquoted-delimiter heredoc body.
pub(crate) fn heredoc_substitutions(body: &str, subs: &mut Vec<String>) -> Result<(), String> {
    let pieces = brush_parser::word::parse_heredoc(body, &parser_options())
        .map_err(|_| "cannot parse a here-document body".to_string())?;
    Builder::default().pieces(body, &pieces, true, subs, 0)
}

/// Collects the command substitutions in arithmetic or array-index text.
pub(crate) fn substitutions_in(text: &str, subs: &mut Vec<String>) -> Result<(), String> {
    scan_nested(text, subs, 0)
}

/// Whether `text` contains a command-substitution marker (`$(`, `${` or a backtick).
/// bash may run it even from inside quotes when it re-evaluates the text as arithmetic.
pub(crate) fn subst_marker(text: &str) -> bool {
    text.contains("$(") || text.contains("${") || text.contains('`')
}

/// Scans text that bash evaluates as an arithmetic expression. Unquoted substitutions
/// are collected as usual, but a marker surviving quote removal (single-quoted or
/// escaped) means the arithmetic evaluation can run a command, which is undecomposable.
pub(crate) fn arithmetic_in(text: &str, subs: &mut Vec<String>) -> Result<(), String> {
    arith_scan(text, subs, 0)
}

fn arith_scan(text: &str, subs: &mut Vec<String>, depth: usize) -> Result<(), String> {
    if !text.contains(['$', '`']) {
        return Ok(());
    }
    let mut b = Builder::default();
    match brush_parser::word::parse(text, &parser_options()) {
        Ok(pieces) => b.pieces(text, &pieces, false, subs, depth)?,
        Err(_) if subst_marker(text) => {
            return Err("arithmetic text may run a command substitution".into());
        }
        Err(_) => return Ok(()),
    }
    // A marker left in the resolved text was quoted or escaped, so brush did not expose
    // it as a live substitution, but the arithmetic evaluation still runs it.
    if subst_marker(&b.text) {
        return Err("arithmetic text may run a command substitution".into());
    }
    Ok(())
}

/// Collects command substitutions nested in expansion text such as `X:-$(cmd)` or an
/// arithmetic expression.
fn scan_nested(text: &str, subs: &mut Vec<String>, depth: usize) -> Result<(), String> {
    if !text.contains(['$', '`']) {
        return Ok(());
    }
    match brush_parser::word::parse(text, &parser_options()) {
        Ok(pieces) => Builder::default().pieces(text, &pieces, false, subs, depth),
        Err(_) if text.contains("$(") || text.contains('`') => {
            Err(format!("cannot parse the expansion `{text}`"))
        }
        Err(_) => Ok(()),
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
}

impl Builder {
    fn pieces(
        &mut self,
        src: &str,
        pieces: &[WordPieceWithSource],
        quoted: bool,
        subs: &mut Vec<String>,
        depth: usize,
    ) -> Result<(), String> {
        if depth > MAX_DEPTH {
            return Err("expansions are nested too deeply".into());
        }
        for p in pieces {
            match &p.piece {
                WordPiece::Text(t) => self.push_text(t, quoted),
                WordPiece::SingleQuotedText(t) => self.text.push_str(t),
                WordPiece::AnsiCQuotedText(t) => match decode_ansi_c(t) {
                    Some(s) => self.text.push_str(&s),
                    None => self.dynamic = true,
                },
                WordPiece::DoubleQuotedSequence(inner)
                | WordPiece::GettextDoubleQuotedSequence(inner) => {
                    self.pieces(src, inner, true, subs, depth)?;
                }
                // `\x` → `x`; a backslash-newline is a line continuation.
                WordPiece::EscapeSequence(e) if e == "\\\n" => {}
                WordPiece::EscapeSequence(e) => {
                    self.text.push_str(e.strip_prefix('\\').unwrap_or(e));
                }
                WordPiece::TildeExpansion(_) => self.dynamic = true,
                WordPiece::ParameterExpansion(pe) => {
                    self.dynamic = true;
                    // `${X:-$(cmd)}` runs cmd: re-parse the braces' content.
                    let raw = src.get(p.start_index..p.end_index).unwrap_or_default();
                    if let Some(inner) = raw.strip_prefix("${").and_then(|r| r.strip_suffix('}')) {
                        scan_nested(inner, subs, depth + 1)?;
                    }
                    // An array subscript or a substring offset is evaluated as arithmetic.
                    match pe {
                        ParameterExpr::Parameter {
                            parameter: Parameter::NamedWithIndex { index, .. },
                            ..
                        } => arith_scan(index, subs, depth + 1)?,
                        ParameterExpr::Substring { offset, length, .. } => {
                            arith_scan(&offset.value, subs, depth + 1)?;
                            if let Some(length) = length {
                                arith_scan(&length.value, subs, depth + 1)?;
                            }
                        }
                        _ => {}
                    }
                }
                WordPiece::CommandSubstitution(c) | WordPiece::BackquotedCommandSubstitution(c) => {
                    self.dynamic = true;
                    subs.push(c.clone());
                }
                WordPiece::ArithmeticExpression(e) => {
                    self.dynamic = true;
                    arith_scan(&e.value, subs, depth + 1)?;
                }
            }
        }
        Ok(())
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
        word_to_tok(raw, &mut Vec::new()).unwrap()
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
        let mut subs = Vec::new();
        word_to_tok("${X:-$(curl a)}$(( $(curl b) ))`curl c`", &mut subs).unwrap();
        assert_eq!(subs, ["curl a", "curl b", "curl c"]);
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
