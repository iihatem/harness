//! Rule pattern matching. Patterns only know one wildcard: `*` matches any run of
//! characters, including `/` and spaces (`**` is the same as `*`).

use crate::argv::{Tok, quote};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Unit {
    Char(char),
    /// A run-time value: only a `*` in the pattern can cover it.
    Dyn,
}

/// Matches `text` against `pattern`, where `*` matches any characters.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let units: Vec<Unit> = text.chars().map(Unit::Char).collect();
    wildcard(pattern, &units)
}

/// Matches a bash rule pattern against an argv, rendered as its shell-quoted join.
/// A pattern ending in ` *` also matches the bare command (`git *` matches `git`).
pub(crate) fn argv_matches(pattern: &str, argv: &[Tok]) -> bool {
    let mut units = Vec::new();
    for (i, tok) in argv.iter().enumerate() {
        if i > 0 {
            units.push(Unit::Char(' '));
        }
        match tok {
            Tok::Lit(s) => units.extend(quote(s).chars().map(Unit::Char)),
            Tok::Glob { .. } | Tok::Dyn => units.push(Unit::Dyn),
        }
    }
    wildcard(pattern, &units)
        || pattern
            .strip_suffix(" *")
            .is_some_and(|bare| wildcard(bare, &units))
}

/// Whether `pattern` could match `argv` once its run-time parts are known: the argv's literal
/// words before its first run-time token must agree with the pattern's text before its first `*`
/// (one is a prefix of the other). Fully literal argvs return `false` (`argv_matches` decides them).
pub(crate) fn argv_may_match(pattern: &str, argv: &[Tok]) -> bool {
    let mut known = String::new();
    for (i, tok) in argv.iter().enumerate() {
        if i > 0 {
            known.push(' ');
        }
        match tok {
            Tok::Lit(s) => known.push_str(&quote(s)),
            Tok::Glob { .. } | Tok::Dyn => {
                let fixed = pattern.split('*').next().unwrap_or_default();
                return fixed.starts_with(&known) || known.starts_with(fixed);
            }
        }
    }
    false
}

/// Iterative wildcard match with single-star backtracking (linear in practice).
fn wildcard(pattern: &str, text: &[Unit]) -> bool {
    let pat: Vec<char> = pattern.chars().collect();
    let (mut p, mut t) = (0, 0);
    let mut resume: Option<(usize, usize)> = None;
    while t < text.len() {
        match pat.get(p) {
            Some('*') => {
                resume = Some((p, t));
                p += 1;
            }
            Some(&c) if text[t] == Unit::Char(c) => {
                p += 1;
                t += 1;
            }
            _ => match resume {
                Some((sp, st)) => {
                    p = sp + 1;
                    t = st + 1;
                    resume = Some((sp, st + 1));
                }
                None => return false,
            },
        }
    }
    pat[p..].iter().all(|&c| c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lits(words: &[&str]) -> Vec<Tok> {
        words.iter().map(|w| Tok::Lit((*w).to_string())).collect()
    }

    #[test]
    fn plain_globs() {
        assert!(glob_match("docs/**", "docs/a/b.md"));
        assert!(glob_match("*.rs", "src/lib.rs"));
        assert!(glob_match("a*b*c", "aXbYc"));
        assert!(!glob_match("a*b", "ac"));
        assert!(glob_match("", ""));
        assert!(!glob_match("", "x"));
    }

    #[test]
    fn argv_patterns() {
        assert!(argv_matches("git status*", &lits(&["git", "status", "-s"])));
        assert!(!argv_matches("git status*", &lits(&["git status", "x"])));
        assert!(argv_matches("git *", &lits(&["git"])));
        assert!(!argv_matches("git *", &lits(&["gitk"])));
        let dynamic = vec![Tok::Lit("cargo".into()), Tok::Dyn];
        assert!(argv_matches("cargo *", &dynamic));
        assert!(!argv_matches("cargo test*", &dynamic));
    }

    #[test]
    fn argv_may_match_dynamic() {
        let npm_dyn = vec![Tok::Lit("npm".into()), Tok::Dyn];
        assert!(argv_may_match("npm publish*", &npm_dyn));

        let echo_dyn = vec![Tok::Lit("echo".into()), Tok::Dyn];
        assert!(!argv_may_match("curl*", &echo_dyn));

        let only_dyn = vec![Tok::Dyn];
        assert!(argv_may_match("curl*", &only_dyn));

        // Fully literal argv returns false (argv_matches decides it)
        assert!(!argv_may_match("curl*", &lits(&["curl", "x"])));
    }
}
