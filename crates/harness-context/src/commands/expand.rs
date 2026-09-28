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
/// - `` !`cmd` `` becomes a shell part; placeholders inside it become shell-quoted arguments.
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
            if !text.is_empty() {
                parts.push(InputPart::Text(std::mem::take(&mut text)));
            }
            let shell = substitute(&after[..end], args, &words, true);
            parts.push(InputPart::Shell(shell));
            rest = &after[end + 1..];
            continue;
        }
        if let Some((value, len)) = placeholder(rest, args, &words, false) {
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

/// A placeholder at the start of `text`, and how many bytes it takes.
fn placeholder(text: &str, args: &str, words: &[String], quote: bool) -> Option<(String, usize)> {
    let value = |raw: &str| {
        if quote {
            shell_quote(raw)
        } else {
            raw.to_string()
        }
    };
    if text.starts_with("$ARGUMENTS") {
        return Some((value(args.trim()), "$ARGUMENTS".len()));
    }
    let digit = text.strip_prefix('$')?.chars().next()?;
    let n = digit.to_digit(10).filter(|n| (1..=9).contains(n))? as usize;
    let word = words.get(n - 1).map(String::as_str).unwrap_or("");
    Some((value(word), 2))
}

/// `text` with every placeholder replaced.
fn substitute(text: &str, args: &str, words: &[String], quote: bool) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        if let Some((value, len)) = placeholder(rest, args, words, quote) {
            out.push_str(&value);
            rest = &rest[len..];
        } else {
            out.push(c);
            rest = &rest[c.len_utf8()..];
        }
    }
    out
}

/// `text` as one single-quoted shell word.
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
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
