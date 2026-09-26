//! Rough word splitting for input the parser rejected (or that is too long to
//! parse). Used only to look for denied or destructive commands, never to allow.

use std::mem::take;

const KEYWORDS: &[&str] = &[
    "!", "{", "}", "if", "then", "else", "elif", "fi", "do", "done", "while", "until", "for", "in",
    "case", "esac", "select", "function", "time", "coproc",
];

/// Splits `src` into rough command word lists: quotes are removed, operators and
/// parentheses end a command, and `$(…)`/backtick bodies become commands of their
/// own. Leading keywords and `NAME=value` words are dropped.
pub(crate) fn rough_commands(src: &str) -> Vec<Vec<String>> {
    let mut s = Splitter::default();
    let mut chars = src.chars().peekable();
    while let Some(c) = chars.next() {
        if s.quote != Some('\'') {
            if c == '`' {
                let closes = s.quote.is_none() && s.stack.last().is_some_and(|f| f.opener == '`');
                if closes {
                    s.close()
                } else {
                    s.open('`')
                }
                continue;
            }
            if c == '$' && chars.next_if_eq(&'(').is_some() {
                s.open('(');
                continue;
            }
        }
        match s.quote {
            Some('\'') if c == '\'' => s.quote = None,
            Some('\'') => s.word.push(c),
            Some(_) if c == '"' => s.quote = None,
            Some(_) if c == '\\' => s.word.extend(chars.next()),
            Some(_) => s.word.push(c),
            None => match c {
                '\'' | '"' => {
                    s.quote = Some(c);
                    s.in_word = true;
                }
                '\\' => {
                    if let Some(n) = chars.next().filter(|&n| n != '\n') {
                        s.word.push(n);
                        s.in_word = true;
                    }
                }
                '#' if !s.in_word => while chars.next_if(|&n| n != '\n').is_some() {},
                ' ' | '\t' | '<' | '>' => s.end_word(),
                '(' => {
                    if let Some(f) = s.stack.last_mut() {
                        f.parens += 1;
                    }
                    s.end_command();
                }
                ')' => {
                    let closes = s
                        .stack
                        .last()
                        .is_some_and(|f| f.opener == '(' && f.parens == 0);
                    if closes {
                        s.close();
                    } else {
                        if let Some(f) = s.stack.last_mut() {
                            f.parens = f.parens.saturating_sub(1);
                        }
                        s.end_command();
                    }
                }
                ';' | '&' | '|' | '\n' | '{' | '}' => s.end_command(),
                _ => {
                    s.word.push(c);
                    s.in_word = true;
                }
            },
        }
    }
    while !s.stack.is_empty() {
        s.close();
    }
    s.end_command();

    s.commands
        .into_iter()
        .map(|cmd| {
            cmd.into_iter()
                .skip_while(|w| KEYWORDS.contains(&w.as_str()) || is_assignment(w))
                .collect::<Vec<_>>()
        })
        .filter(|cmd| !cmd.is_empty())
        .collect()
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

#[derive(Default)]
struct Splitter {
    commands: Vec<Vec<String>>,
    words: Vec<String>,
    word: String,
    in_word: bool,
    quote: Option<char>,
    /// Outer contexts of the open `$(…)`/backtick substitutions.
    stack: Vec<Frame>,
}

struct Frame {
    words: Vec<String>,
    word: String,
    quote: Option<char>,
    opener: char,
    parens: usize,
}

impl Splitter {
    fn end_word(&mut self) {
        if self.in_word {
            self.words.push(take(&mut self.word));
            self.in_word = false;
        }
    }

    fn end_command(&mut self) {
        self.end_word();
        self.commands.push(take(&mut self.words));
    }

    fn open(&mut self, opener: char) {
        self.stack.push(Frame {
            words: take(&mut self.words),
            word: take(&mut self.word),
            quote: self.quote.take(),
            opener,
            parens: 0,
        });
        self.in_word = false;
    }

    fn close(&mut self) {
        self.end_command();
        if let Some(f) = self.stack.pop() {
            self.words = f.words;
            self.word = f.word;
            self.quote = f.quote;
            self.in_word = true;
        }
    }
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

    #[test]
    fn splits_rough_commands() {
        let src = "A=1 c'u'rl x && (echo \"a$(git push -f)b\" | y) # z\nfor i in 1; do rm -rf /";
        assert_eq!(
            rough_commands(src),
            [
                vec!["curl", "x"],
                vec!["git", "push", "-f"],
                vec!["echo", "ab"],
                vec!["y"],
                vec!["i", "in", "1"],
                vec!["rm", "-rf", "/"],
            ]
        );
    }
}
