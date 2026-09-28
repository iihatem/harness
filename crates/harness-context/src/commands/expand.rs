//! Expanding a custom command for one invocation: placeholders, file references, shell commands,
//! and the command's `allowed-tools`.

use std::{io::Read, path::Path};

use harness_core::{
    engine::RuleSet,
    permission::{Action, Decision, PermissionPolicy, resolve_path},
    turn::{InputPart, TurnInput},
};

use super::{CustomCommand, Scope, split_args};

/// Bytes of one referenced file included in the message.
const MAX_FILE_BYTES: usize = 256 * 1024;

/// A custom command expanded for one invocation.
#[derive(Debug, Clone)]
pub struct Expansion {
    /// The turn to run: its parts, what the user typed, and the command's allowed tools as rules
    /// for that turn. The model is left for the caller to resolve.
    pub input: TurnInput,
    /// The command's `model`, as written, when it may apply: always for a global command file,
    /// and for a project one only in a trusted workspace.
    pub model: Option<String>,
    pub warnings: Vec<String>,
    /// What harness decided that the user should know, such as an ignored `model`.
    pub notes: Vec<String>,
}

/// Expands `command` invoked with `args` (the text after its name):
///
/// - `$ARGUMENTS` becomes `args`, and `$1` to `$9` its words (see [`split_args`]). When the body
///   uses none of them, non-empty arguments are appended as `ARGUMENTS: <args>`.
/// - `` !`cmd` `` becomes a shell part. A placeholder inside it becomes its value quoted for where
///   it lands, so the value reaches the command as data (see [`substitute`]). When a placeholder
///   is somewhere that cannot be done, the part is never run: it stays as
///   `` [not expanded: `cmd`] ``, with a warning.
/// - `@path` becomes the content of that file when it is inside `workspace` and `policy` lets
///   harness read it without asking; other `@` words stay as written.
///
/// Arguments are inserted as they are and never expanded themselves.
///
/// A project command file's `model` is kept only when `trusted` (the workspace's project settings
/// are trusted); otherwise a note says it was ignored. A global command file's always is.
pub fn expand(
    command: &CustomCommand,
    args: &str,
    workspace: &Path,
    policy: &dyn PermissionPolicy,
    trusted: bool,
) -> Expansion {
    let words = split_args(args);
    let mut warnings = Vec::new();
    let mut notes = Vec::new();
    let mut parts = Vec::new();
    let mut text = String::new();
    let body = command.body.as_str();
    let mut rest = body;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix("!`")
            && let Some(end) = after.find(['`', '\n'])
            && after.as_bytes()[end] == b'`'
            && end > 0
        {
            let shell = &after[..end];
            match substitute(shell, args, &words) {
                Ok(expanded) => {
                    if !text.is_empty() {
                        parts.push(InputPart::Text(std::mem::take(&mut text)));
                    }
                    parts.push(InputPart::Shell(expanded));
                }
                Err(why) => {
                    warnings.push(format!(
                        "/{}: did not run !`{shell}`: {why}, where an argument cannot be quoted safely",
                        command.name
                    ));
                    text.push_str(&format!("[not expanded: `{shell}`]"));
                }
            }
            rest = &after[end + 1..];
            continue;
        }
        if let Some((value, len)) = placeholder(rest, args, &words) {
            text.push_str(&value);
            rest = &rest[len..];
            continue;
        }
        if rest.starts_with('@') && starts_word(body, rest) {
            let token_len = rest[1..]
                .find(|c: char| {
                    c.is_whitespace() || matches!(c, ')' | ',' | ';' | '"' | '\'' | '`')
                })
                .map_or(rest.len(), |i| i + 1);
            if let Some((content, used)) =
                reference(&rest[1..token_len], workspace, policy, &mut warnings)
            {
                text.push_str(&content);
                rest = &rest[1 + used..];
                continue;
            }
        }
        let c = rest.chars().next().expect("rest is not empty");
        text.push(c);
        rest = &rest[c.len_utf8()..];
    }
    let uses_arguments =
        body.contains("$ARGUMENTS") || (1..=9).any(|n| body.contains(&format!("${n}")));
    if !uses_arguments && !args.trim().is_empty() {
        text.truncate(text.trim_end().len());
        text.push_str(&format!("\n\nARGUMENTS: {}", args.trim()));
    }
    if !text.is_empty() {
        parts.push(InputPart::Text(text));
    }
    let (allow, rule_warnings) = allowed_tools_rules(&command.allowed_tools);
    warnings.extend(rule_warnings);
    let typed = format!("/{} {}", command.name, args.trim());
    let model = match &command.model {
        Some(model) if command.scope == Scope::Project && !trusted => {
            notes.push(format!(
                "/{} asks for model {model}, but a project command file chooses the model only in a trusted workspace; using the session's model",
                command.name
            ));
            None
        }
        model => model.clone(),
    };
    Expansion {
        input: TurnInput {
            parts,
            display: Some(typed.trim_end().to_string()),
            rules: RuleSet {
                allow,
                ..RuleSet::default()
            },
            ..TurnInput::default()
        },
        model,
        warnings,
        notes,
    }
}

/// Whether `rest` (a suffix of `body`) starts at the beginning of a word.
fn starts_word(body: &str, rest: &str) -> bool {
    let before = &body[..body.len() - rest.len()];
    before
        .chars()
        .next_back()
        .is_none_or(|c| c.is_whitespace() || c == '(')
}

/// A placeholder at the start of `text`, its value, and how many bytes it takes.
fn placeholder(text: &str, args: &str, words: &[String]) -> Option<(String, usize)> {
    if text.starts_with("$ARGUMENTS") {
        return Some((args.trim().to_string(), "$ARGUMENTS".len()));
    }
    let digit = text.strip_prefix('$')?.chars().next()?;
    let n = digit.to_digit(10).filter(|n| (1..=9).contains(n))? as usize;
    let word = words.get(n - 1).map(String::as_str).unwrap_or("");
    Some((word.to_string(), 2))
}

/// How the shell reads the text at some point of a `!` command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Quoting {
    /// Not quoted: a value is written as one single-quoted word, with `'` as `'\''`.
    Unquoted,
    /// Inside `'…'`: a value is written as it is, with `'` as `'\''`.
    Single,
    /// Inside `"…"` or `$"…"`: `\`, `"`, `$` and backticks in a value are escaped.
    Double,
    /// Inside `$'…'`, where backslash escapes apply: no value can be written safely.
    AnsiC,
    /// A comment, which runs to the end of the command, where quoting means nothing.
    Comment,
}

/// Whether `c` ends an unquoted word.
fn ends_word(c: char) -> bool {
    c.is_whitespace() || matches!(c, ';' | '&' | '|' | '(' | ')' | '<' | '>')
}

/// Whether the text after `prev` starts a new word.
fn word_start(prev: Option<char>) -> bool {
    prev.is_none_or(ends_word)
}

/// The byte length of the first character of `text`, or 0.
fn first_len(text: &str) -> usize {
    text.chars().next().map_or(0, char::len_utf8)
}

/// The `!` command `text` with every placeholder replaced by its value, encoded for where it
/// lands (see [`Quoting`]), so that the command gets the value as data. The shell has no newlines
/// or backticks here: the command ends at the first of them.
///
/// A placeholder is refused, and the whole command with it, where its value cannot be encoded
/// safely: after a `\` or a `$`, in a comment, inside `$'…'`, `${…}`, `[[ … ]]` or `[…]` within a
/// word (an array subscript or `$[…]`, which the shell evaluates as arithmetic), and anywhere after
/// `((` or `$((` (arithmetic), or a `$(` within double quotes (the end of either is not tracked).
/// The error says which placeholder, and why.
fn substitute(text: &str, args: &str, words: &[String]) -> Result<String, String> {
    use Quoting::*;
    let mut out = String::with_capacity(text.len());
    let mut quoting = Unquoted;
    // Set once the quoting can no longer be followed: why every later placeholder is refused.
    let mut untracked: Option<&str> = None;
    // Open braces of a `${…}`.
    let mut braces = 0usize;
    // Inside `[[ … ]]`, where the shell may evaluate a value as arithmetic.
    let mut in_test = false;
    // Inside `[…]` within an unquoted word: an array subscript, or `$[…]`.
    let mut in_subscript = false;
    let mut prev: Option<char> = None;
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        if let Some((value, len)) = placeholder(rest, args, words) {
            let name = &rest[..len];
            let refused = match quoting {
                _ if untracked.is_some() => untracked,
                _ if braces > 0 => Some("is inside `${…}`"),
                Comment => Some("is in a comment"),
                AnsiC => Some("is inside `$'…'`"),
                Unquoted | Double if prev == Some('$') => Some("follows a `$`"),
                _ if in_test => Some("is inside `[[ … ]]`"),
                Unquoted if in_subscript => Some("is inside `[…]` within a word"),
                _ => None,
            };
            if let Some(why) = refused {
                return Err(format!("{name} {why}"));
            }
            match quoting {
                Unquoted => {
                    out.push('\'');
                    out.push_str(&value.replace('\'', r"'\''"));
                    out.push('\'');
                }
                Single => out.push_str(&value.replace('\'', r"'\''")),
                Double => {
                    for ch in value.chars() {
                        if matches!(ch, '\\' | '"' | '$' | '`') {
                            out.push('\\');
                        }
                        out.push(ch);
                    }
                }
                AnsiC | Comment => unreachable!("refused above"),
            }
            // The value is part of a word.
            prev = Some('\'');
            rest = &rest[len..];
            continue;
        }
        let after = &rest[c.len_utf8()..];
        let mut len = c.len_utf8();
        // What `prev` becomes; an escaped character is part of a word, whatever it is.
        let mut last = Some(c);
        if c == '\\' && matches!(quoting, Unquoted | Double) && braces == 0 {
            if let Some((_, used)) = placeholder(after, args, words) {
                return Err(format!("{} follows a backslash", &after[..used]));
            }
            len += first_len(after);
            last = Some('\\');
        } else if braces > 0 {
            match c {
                '{' => braces += 1,
                '}' => braces -= 1,
                '\'' | '"' | '\\' | '`' => {
                    untracked = Some("comes after a quote, backslash or backtick inside `${…}`")
                }
                '$' if after.starts_with('(') => {
                    untracked = Some("comes after a `$(` inside `${…}`")
                }
                _ => {}
            }
        } else {
            match quoting {
                Comment => {}
                Single => {
                    if c == '\'' {
                        quoting = Unquoted;
                    }
                }
                AnsiC => match c {
                    '\\' => len += first_len(after),
                    '\'' => quoting = Unquoted,
                    _ => {}
                },
                Double => match c {
                    '"' => quoting = Unquoted,
                    '$' if after.starts_with('(') => {
                        untracked = Some("comes after a `$(` inside double quotes")
                    }
                    '$' if after.starts_with('{') => {
                        braces = 1;
                        len += 1;
                        last = Some('{');
                    }
                    '`' => untracked = Some("comes after a backtick"),
                    _ => {}
                },
                Unquoted => match c {
                    '\'' => quoting = Single,
                    '"' => quoting = Double,
                    '$' if after.starts_with('\'') => {
                        quoting = AnsiC;
                        len += 1;
                        last = Some('\'');
                    }
                    '$' if after.starts_with('"') => {
                        quoting = Double;
                        len += 1;
                        last = Some('"');
                    }
                    '$' if after.starts_with("((") => {
                        untracked = Some("comes after `$((` (arithmetic)")
                    }
                    '$' if after.starts_with('{') => {
                        braces = 1;
                        len += 1;
                        last = Some('{');
                    }
                    // Inside `$(…)` the shell reads quotes as it does outside it.
                    '#' if word_start(prev) => quoting = Comment,
                    '(' if after.starts_with('(') && word_start(prev) => {
                        untracked = Some("comes after `((` (arithmetic)")
                    }
                    '[' if after.starts_with('[')
                        && word_start(prev)
                        && after[1..].starts_with(char::is_whitespace) =>
                    {
                        in_test = true;
                        len += 1;
                    }
                    ']' if in_test
                        && after.starts_with(']')
                        && word_start(prev)
                        && after[1..].chars().next().is_none_or(ends_word) =>
                    {
                        in_test = false;
                        len += 1;
                    }
                    '[' if !word_start(prev) => in_subscript = true,
                    ']' => in_subscript = false,
                    '`' => untracked = Some("comes after a backtick"),
                    c if ends_word(c) => in_subscript = false,
                    _ => {}
                },
            }
        }
        out.push_str(&rest[..len]);
        rest = &rest[len..];
        prev = last;
    }
    Ok(out)
}

/// The content of the workspace file `token` names, and how many bytes of `token` it used (a
/// trailing `.`, `,`, `:`, `!` or `?` may be punctuation). `None` when no such file exists, or
/// when it exists but may not be included (a warning then says why).
fn reference(
    token: &str,
    workspace: &Path,
    policy: &dyn PermissionPolicy,
    warnings: &mut Vec<String>,
) -> Option<(String, usize)> {
    let trimmed = token.trim_end_matches(['.', ',', ':', '!', '?']);
    let candidate = [token, trimmed]
        .into_iter()
        .filter(|t| !t.is_empty())
        .find(|t| workspace.join(t).is_file())?;
    let path = resolve_path(workspace, Path::new(candidate));
    if !path.starts_with(workspace) {
        warnings.push(format!(
            "left @{candidate} as written: {} is outside the workspace",
            path.display()
        ));
        return None;
    }
    if policy.check(&Action::Read(path.clone())) != Decision::Allow {
        warnings.push(format!(
            "left @{candidate} as written: reading {} needs approval",
            path.display()
        ));
        return None;
    }
    let mut bytes = Vec::new();
    std::fs::File::open(&path)
        .and_then(|f| f.take(MAX_FILE_BYTES as u64).read_to_end(&mut bytes))
        .ok()?;
    Some((
        String::from_utf8_lossy(&bytes).into_owned(),
        candidate.len(),
    ))
}

/// Maps Claude Code `allowed-tools` entries to harness allow rules for one invocation:
///
/// - `Bash` is `bash:*`; `Bash(git add:*)` is `bash:git add` and `bash:git add *`; any other
///   `Bash(pattern)` is `bash:pattern`.
/// - `Write`, `Edit` and `MultiEdit` are `write:*`, or `write:<pattern>` with a pattern.
/// - `Read`, `Grep` and `Glob` add nothing: reads inside the workspace need no rule, and a
///   command file should not widen reads outside it.
///
/// Other entries are ignored with a warning (the second list).
pub fn allowed_tools_rules(entries: &[String]) -> (Vec<String>, Vec<String>) {
    let mut rules = Vec::new();
    let mut warnings = Vec::new();
    for entry in entries {
        let entry = entry.trim();
        let (tool, pattern) = match entry.split_once('(') {
            Some((tool, rest)) => (tool.trim(), rest.strip_suffix(')').map(str::trim)),
            None => (entry, None),
        };
        match (tool, pattern) {
            ("Bash", None) | ("Bash", Some("*" | "")) => rules.push("bash:*".to_string()),
            ("Bash", Some(pattern)) => match pattern.strip_suffix(":*") {
                Some(prefix) => {
                    rules.push(format!("bash:{}", prefix.trim()));
                    rules.push(format!("bash:{} *", prefix.trim()));
                }
                None => rules.push(format!("bash:{pattern}")),
            },
            ("Write" | "Edit" | "MultiEdit", None) => rules.push("write:*".to_string()),
            ("Write" | "Edit" | "MultiEdit", Some(pattern)) => {
                // Claude Code writes absolute paths as `//path`, and `/path` or `./path` for
                // paths in the project.
                let pattern = match pattern.strip_prefix("//") {
                    Some(absolute) => format!("/{absolute}"),
                    None => pattern
                        .strip_prefix("./")
                        .or_else(|| pattern.strip_prefix('/'))
                        .unwrap_or(pattern)
                        .to_string(),
                };
                rules.push(format!("write:{pattern}"));
            }
            ("Read" | "Grep" | "Glob", _) => {}
            _ => warnings.push(format!(
                "allowed-tools entry `{entry}` has no harness equivalent; ignored"
            )),
        }
    }
    (rules, warnings)
}
