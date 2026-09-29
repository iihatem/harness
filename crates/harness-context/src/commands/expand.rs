//! Expanding a custom command for one invocation: placeholders, file references, shell commands,
//! and the command's `allowed-tools`.

use std::path::Path;

use harness_core::{
    engine::RuleSet,
    permission::{Action, Decision, PermissionPolicy, resolve_path},
    turn::{InputPart, TurnInput},
};

use super::{CustomCommand, Scope, split_args};
use crate::read::read_regular;

/// Bytes of one referenced file included in the message.
const MAX_FILE_BYTES: usize = 256 * 1024;

/// A custom command expanded for one invocation.
#[derive(Debug, Clone)]
pub struct Expansion {
    /// The turn to run: its parts, what the user typed, and the command's allowed tools as rules
    /// for that turn. The model is left for the caller to resolve.
    pub input: TurnInput,
    /// The command's `model`, as written, when it may apply: always for a global command file,
    /// and for a project one only when the directory it comes from is trusted.
    pub model: Option<String>,
    pub warnings: Vec<String>,
    /// What harness decided that the user should know, such as an ignored `model`.
    pub notes: Vec<String>,
}

/// Whether project command files may choose the model: when the user trusts the directory they
/// come from, as `harness trust` run there records it.
#[derive(Debug, Clone, Copy)]
pub struct ProjectTrust<'a> {
    /// Where the project's command files come from: the repository root, or the working
    /// directory outside a repository.
    pub dir: &'a Path,
    pub trusted: bool,
}

/// Expands `command` invoked with `args` (the text after its name):
///
/// - `$ARGUMENTS` becomes `args`, and `$1` to `$9` its words (see [`split_args`]). When the body
///   uses none of them, non-empty arguments are appended as `ARGUMENTS: <args>`.
/// - `` !`cmd` `` becomes a shell part, `cmd` as written. Nothing is filled into it: when it uses
///   the arguments, [`prelude`] sets them as shell parameters first (ruling P3-R5), so `"$1"` is
///   the first word as data, as in any script. When it uses them together with a construct where
///   bash may evaluate a parameter's value ([`evaluates_parameters`]), the part is never run: it
///   stays as `` [not expanded: `cmd`] ``, with a warning.
/// - `@path` becomes the content of that file when it is inside `workspace` and `policy` lets
///   harness read it without asking; other `@` words stay as written.
///
/// Arguments are inserted as they are and never expanded themselves.
///
/// A project command file's `model` is kept only when `trust` says the directory it comes from is
/// trusted; otherwise a note says it was ignored, and where to run `harness trust`. A global
/// command file's always is.
pub fn expand(
    command: &CustomCommand,
    args: &str,
    workspace: &Path,
    policy: &dyn PermissionPolicy,
    trust: ProjectTrust<'_>,
) -> Expansion {
    let words = split_args(args);
    let mut warnings = Vec::new();
    let mut notes = Vec::new();
    let mut parts = Vec::new();
    let mut text = String::new();
    let body = command.body.as_str();
    // Whether a shell part got the prelude, and whether any uses the arguments.
    let mut with_prelude = false;
    let mut shell_uses_arguments = false;
    let mut rest = body;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix("!`")
            && let Some(end) = after.find(['`', '\n'])
            && after.as_bytes()[end] == b'`'
            && end > 0
        {
            let shell = &after[..end];
            let run = if !uses_shell_arguments(shell) {
                Ok(shell.to_string())
            } else if let Some(why) = evaluates_parameters(shell) {
                Err(why)
            } else {
                with_prelude = true;
                Ok(format!("{}{shell}", prelude(args, &words)))
            };
            match run {
                Ok(run) => {
                    if !text.is_empty() {
                        parts.push(InputPart::Text(std::mem::take(&mut text)));
                    }
                    parts.push(InputPart::Shell(run));
                }
                Err(why) => {
                    warnings.push(format!(
                        "/{}: did not run !`{shell}`: it uses the command's arguments together with {why}, where bash may run an argument as code",
                        command.name
                    ));
                    text.push_str(&format!("[not expanded: `{shell}`]"));
                }
            }
            shell_uses_arguments |= uses_shell_arguments(shell);
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
    let uses_arguments = shell_uses_arguments
        || body.contains("$ARGUMENTS")
        || (1..=9).any(|n| body.contains(&format!("${n}")));
    if !uses_arguments && !args.trim().is_empty() {
        text.truncate(text.trim_end().len());
        text.push_str(&format!("\n\nARGUMENTS: {}", args.trim()));
    }
    if !text.is_empty() {
        parts.push(InputPart::Text(text));
    }
    let (mut allow, rule_warnings) = allowed_tools_rules(&command.allowed_tools);
    warnings.extend(rule_warnings);
    // The prelude's `set --` is unlisted, so where the command file pre-approves commands it
    // must not make them ask. `ARGUMENTS='…'` alone the shell analysis passes over.
    if with_prelude && allow.iter().any(|rule| rule.starts_with("bash:")) {
        allow.extend(["bash:set --".to_string(), "bash:set -- *".to_string()]);
    }
    let typed = format!("/{} {}", command.name, args.trim());
    let model = match &command.model {
        Some(model) if command.scope == Scope::Project && !trust.trusted => {
            notes.push(format!(
                "/{} asks for model {model}, but a project command file chooses the model only in a trusted workspace; using the session's model (run `harness trust` in {}, where the command files come from, to allow it)",
                command.name,
                trust.dir.display()
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

/// The prelude that gives a `!` command the invocation's arguments (ruling P3-R5):
/// `ARGUMENTS='<args>'; set -- '<word 1>' '<word 2>' …; `. It is harness's own text, at the top
/// level of the command, so single quotes with `'` written as `'\''` hold any value as data.
fn prelude(args: &str, words: &[String]) -> String {
    let quote = |value: &str| format!("'{}'", value.replace('\'', r"'\''"));
    let mut prelude = format!("ARGUMENTS={}; set --", quote(args.trim()));
    for word in words {
        prelude.push(' ');
        prelude.push_str(&quote(word));
    }
    prelude.push_str("; ");
    prelude
}

/// Whether the shell text `shell` uses the invocation's arguments: `$1` to `$9`, `$ARGUMENTS`,
/// `$@`, `$*` or `$#`, also as `${…}`, `${#…}` or `${!…}`.
fn uses_shell_arguments(shell: &str) -> bool {
    shell.match_indices('$').any(|(i, _)| {
        let after = &shell[i + 1..];
        let after = after.strip_prefix('{').unwrap_or(after);
        if after.starts_with('#') {
            return true;
        }
        let after = after.strip_prefix('!').unwrap_or(after);
        after.starts_with(|c: char| matches!(c, '1'..='9' | '@' | '*'))
            || after.starts_with("ARGUMENTS")
    })
}

/// Words that run their operands, or evaluate them as arithmetic, a variable name or a
/// subscript, and the name the warning gives each.
const EVALUATING_WORDS: [(&str, &str); 15] = [
    ("let", "`let`"),
    ("declare", "`declare`"),
    ("typeset", "`typeset`"),
    ("local", "`local`"),
    ("eval", "`eval`"),
    ("trap", "`trap`"),
    ("read", "`read`"),
    ("source", "`source`"),
    ("unset", "`unset`"),
    ("mapfile", "`mapfile`"),
    ("readarray", "`readarray`"),
    ("printf", "`printf -v`"),
    // `-W` expands a word list and `-C` runs a command.
    ("compgen", "`compgen`"),
    ("complete", "`complete`"),
    // `enable -f` loads a shared library.
    ("enable", "`enable`"),
];

/// Words after which the next word is a command.
const COMMAND_PREFIXES: [&str; 12] = [
    "then", "do", "else", "elif", "if", "while", "until", "!", "time", "command", "builtin", "exec",
];

/// The first construct in the shell text `shell` where bash may evaluate a parameter's value as
/// code, arithmetic or a variable name, when there is one. A deliberately coarse, lexical check,
/// which may find one where bash would not: quotes and backslashes are dropped first, so that
/// `e''val` and `\eval` are `eval`, and words are split at blanks and `;&|()<>`.
fn evaluates_parameters(shell: &str) -> Option<&'static str> {
    let text: String = shell
        .chars()
        .filter(|c| !matches!(c, '\'' | '"' | '\\'))
        .collect();
    if text.contains("((") {
        return Some("`((` (arithmetic)");
    }
    if text.contains("$[") {
        return Some("`$[` (arithmetic)");
    }
    if text.contains("${!") {
        return Some("`${!` (indirection)");
    }
    if text.contains("=(") {
        return Some("an array assignment `=(`");
    }
    let bytes = text.as_bytes();
    if (1..bytes.len())
        .any(|i| bytes[i] == b'[' && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_'))
    {
        return Some("a subscript `name[`");
    }
    if substring_expansion(&text) {
        return Some("a substring `${…:…}` (arithmetic)");
    }
    if transform_expansion(&text) {
        return Some("a `${…@…}` transform (`@P` runs a prompt string's commands)");
    }
    // Words, and whether each is where a command starts.
    let mut words: Vec<(&str, bool)> = Vec::new();
    let mut command_start = true;
    let mut start = None;
    for (i, c) in text.char_indices().chain([(text.len(), ';')]) {
        let separator = c.is_whitespace() || matches!(c, ';' | '&' | '|' | '(' | ')' | '<' | '>');
        match (separator, start) {
            (true, Some(s)) => {
                let word = &text[s..i];
                words.push((word, command_start));
                command_start = COMMAND_PREFIXES.contains(&word);
                start = None;
            }
            (false, None) => start = Some(i),
            _ => {}
        }
        if separator && matches!(c, ';' | '&' | '|' | '(' | ')' | '\n') {
            command_start = true;
        }
    }
    let has = |w: &str| words.iter().any(|(word, _)| *word == w);
    for (word, why) in EVALUATING_WORDS {
        let found = if word == "printf" {
            has("printf") && words.iter().any(|(w, _)| w.starts_with("-v"))
        } else {
            has(word)
        };
        if found {
            return Some(why);
        }
    }
    if words.iter().any(|(word, first)| *word == "." && *first) {
        return Some("`.` as a command");
    }
    if has("getopts") {
        return Some("`getopts`");
    }
    if has("wait")
        && words
            .iter()
            .any(|(w, _)| w.starts_with('-') && !w.starts_with("--") && w.contains('p'))
    {
        return Some("`wait -p`");
    }
    if let Some(why) = export_names_from_parameters(shell) {
        return Some(why);
    }
    // `export` and `readonly` take their operands as names, which may be array elements: a
    // parameter in a name (before any `=`) is evaluated. `export NAME="$1"` is only a value.
    // This coarser reading also finds them inside quoted text run by another shell.
    for (i, (word, _)) in words.iter().enumerate() {
        if matches!(*word, "export" | "readonly")
            && words[i + 1..]
                .iter()
                .take_while(|(_, first)| !first)
                .filter(|(w, _)| !w.starts_with('-'))
                .any(|(w, _)| {
                    w.split('=')
                        .next()
                        .is_some_and(|name| name.contains(['$', '`']))
                })
        {
            return Some("a parameter in a name for `export` or `readonly`");
        }
    }
    let comparisons = ["-eq", "-ne", "-lt", "-le", "-gt", "-ge", "-v"];
    if has("[[") && comparisons.iter().any(|c| has(c)) {
        return Some("`[[` with an arithmetic comparison or `-v`");
    }
    if (has("[") || has("test")) && has("-v") {
        return Some("`-v` in a test");
    }
    None
}

/// A word of shell text, read with its quotes: the text without them, and whether a `$` or
/// backtick in it is unquoted, so that bash splits what it expands to.
struct RawWord {
    text: String,
    unquoted_expansion: bool,
}

/// The words of the shell text `shell`, read with quotes and backslashes as bash reads them at the
/// top level, with `None` where a command ends (`;`, `&`, `|`, `(`, `)` or a newline). Text inside
/// quotes is not read again as a nested command.
fn raw_words(shell: &str) -> Vec<Option<RawWord>> {
    #[derive(PartialEq)]
    enum Quote {
        None,
        Single,
        Double,
    }
    let mut words = Vec::new();
    let mut word: Option<RawWord> = None;
    let mut quote = Quote::None;
    let mut chars = shell.chars();
    fn current(word: &mut Option<RawWord>) -> &mut RawWord {
        word.get_or_insert_with(|| RawWord {
            text: String::new(),
            unquoted_expansion: false,
        })
    }
    while let Some(c) = chars.next() {
        match quote {
            Quote::Single if c == '\'' => quote = Quote::None,
            Quote::Single => current(&mut word).text.push(c),
            Quote::Double if c == '"' => quote = Quote::None,
            Quote::Double if c == '\\' => current(&mut word).text.extend(chars.next()),
            Quote::Double => current(&mut word).text.push(c),
            Quote::None => match c {
                '\'' => {
                    current(&mut word);
                    quote = Quote::Single;
                }
                '"' => {
                    current(&mut word);
                    quote = Quote::Double;
                }
                '\\' => current(&mut word).text.extend(chars.next()),
                '$' | '`' => {
                    let w = current(&mut word);
                    w.unquoted_expansion = true;
                    w.text.push(c);
                }
                ';' | '&' | '|' | '(' | ')' | '\n' => {
                    words.extend(word.take().map(Some));
                    words.push(None);
                }
                c if c.is_whitespace() || matches!(c, '<' | '>') => {
                    words.extend(word.take().map(Some))
                }
                c => current(&mut word).text.push(c),
            },
        }
    }
    words.extend(word.take().map(Some));
    words
}

/// Why an `export` or `readonly` in `shell` may take a name from a parameter, if it may: an
/// operand with a parameter before its `=`, or one with an unquoted `$` or backtick, which bash
/// splits when the builtin is reached through `builtin`, `command`, a quoted name or a preceding
/// assignment. `export NAME="$1"` has neither.
fn export_names_from_parameters(shell: &str) -> Option<&'static str> {
    let words = raw_words(shell);
    let operands = words.iter().enumerate().flat_map(|(i, word)| {
        let builtin = word
            .as_ref()
            .is_some_and(|w| matches!(w.text.as_str(), "export" | "readonly"));
        let rest = if builtin { &words[i + 1..] } else { &[][..] };
        rest.iter().map_while(Option::as_ref)
    });
    let mut unquoted = false;
    for operand in operands {
        let name = operand.text.split('=').next().unwrap_or_default();
        if name.contains(['$', '`']) {
            return Some("a parameter in a name for `export` or `readonly`");
        }
        unquoted |= operand.unquoted_expansion;
    }
    unquoted.then_some("an unquoted parameter in an `export` or `readonly` operand")
}

/// Whether `text` has a `${name@op}` transform, such as `${1@P}`, which expands the value as a
/// prompt string and so runs its command substitutions (bash 4.4 and later). Every operator is
/// refused. An `@` later in the expansion, as in `${1:-user@host}`, is text.
fn transform_expansion(text: &str) -> bool {
    text.match_indices("${").any(|(i, _)| {
        let inside = &text[i + 2..];
        let inside = inside.strip_prefix(['#', '!']).unwrap_or(inside);
        let name = if inside.starts_with(|c: char| "@*#?$!-".contains(c)) {
            1
        } else {
            inside
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(inside.len())
        };
        let after = &inside[name..];
        after.starts_with('@') && after[1..].starts_with(|c: char| c.is_ascii_alphabetic())
    })
}

/// Whether `text` has a `${name:offset}` or `${name:offset:length}` expansion, whose offset and
/// length are arithmetic. `${name:-…}`, `${name:=…}`, `${name:?…}` and `${name:+…}` are not.
fn substring_expansion(text: &str) -> bool {
    text.match_indices("${").any(|(i, _)| {
        let inside = &text[i + 2..];
        let inside = &inside[..inside.find('}').unwrap_or(inside.len())];
        inside
            .find(':')
            .is_some_and(|colon| !inside[colon + 1..].starts_with(['-', '=', '?', '+']))
    })
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
    // `path` is resolved through symlinks, so the read may refuse one swapped in since.
    let bytes = read_regular(&path, MAX_FILE_BYTES as u64).ok()?;
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
