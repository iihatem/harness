//! Slash commands: the built-ins, and Markdown command files from `.harness/commands`,
//! `.claude/commands` and `.opencode/commands` in the project, then from `commands/` in the
//! harness config directory and `~/.claude/commands`. The first definition of a name wins.

pub mod frontmatter;

use std::{
    io::Read,
    path::{Path, PathBuf},
};

/// The built-in commands and what they do, in `/help` order.
pub const BUILTINS: [(&str, &str); 12] = [
    ("help", "List commands"),
    ("model", "Switch the model"),
    ("mode", "Switch the approval mode"),
    ("new", "Start a new session"),
    ("resume", "Resume an earlier session"),
    ("rewind", "Rewind code, conversation, or both"),
    ("compact", "Summarize the conversation to free context"),
    ("context", "Show where the context window goes"),
    ("usage", "Show token usage per model"),
    ("login", "Sign in to a provider"),
    ("init", "Draft an AGENTS.md for this project"),
    ("quit", "Exit harness"),
];

/// Command files deeper than this below a commands directory are ignored.
const MAX_DEPTH: usize = 8;
/// Bytes read from one command file.
const MAX_FILE_BYTES: usize = 1024 * 1024;

/// A command defined by a Markdown file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomCommand {
    /// The name after `/`: `opsx:propose` for `opsx/propose.md`.
    pub name: String,
    pub path: PathBuf,
    pub description: Option<String>,
    pub argument_hint: Option<String>,
    /// The model for this command's invocations only.
    pub model: Option<String>,
    /// Claude Code tool patterns, such as `Bash(openspec:*)`, allowed for this command's
    /// invocations only.
    pub allowed_tools: Vec<String>,
    /// The file without its frontmatter.
    pub body: String,
}

/// The custom commands found, and what went wrong finding them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Commands {
    /// Sorted by name.
    pub custom: Vec<CustomCommand>,
    pub warnings: Vec<String>,
}

impl Commands {
    pub fn get(&self, name: &str) -> Option<&CustomCommand> {
        self.custom.iter().find(|c| c.name == name)
    }

    /// Every command and its description, built-ins first, for `/help` and completion.
    pub fn listing(&self) -> Vec<(String, String)> {
        let builtins = BUILTINS
            .iter()
            .map(|(name, description)| (name.to_string(), description.to_string()));
        let custom = self.custom.iter().map(|c| {
            let description = c
                .description
                .clone()
                .unwrap_or_else(|| format!("({})", c.path.display()));
            (c.name.clone(), description)
        });
        builtins.chain(custom).collect()
    }
}

pub fn is_builtin(name: &str) -> bool {
    BUILTINS.iter().any(|(builtin, _)| *builtin == name)
}

/// Finds the custom commands for a project rooted at `project_root` (the repository root, or the
/// working directory outside a repository). Project command files must resolve inside the project;
/// global ones inside their own commands directory.
pub fn discover(project_root: &Path, config_dir: &Path, home: Option<&Path>) -> Commands {
    let project = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf());
    let mut dirs: Vec<(PathBuf, PathBuf)> = [".harness", ".claude", ".opencode"]
        .iter()
        .map(|d| (project_root.join(d).join("commands"), project.clone()))
        .collect();
    let mut global = vec![config_dir.join("commands")];
    if let Some(home) = home {
        global.push(home.join(".claude/commands"));
    }
    for dir in global {
        let confine = dir.canonicalize().unwrap_or_else(|_| dir.clone());
        dirs.push((dir, confine));
    }
    let mut found = Finder::default();
    for (dir, confine) in &dirs {
        if dir.is_dir() {
            found.walk(dir, confine, &mut Vec::new());
        }
    }
    found.commands.custom.sort_by(|a, b| a.name.cmp(&b.name));
    found.commands
}

#[derive(Default)]
struct Finder {
    commands: Commands,
}

impl Finder {
    fn walk(&mut self, dir: &Path, confine: &Path, namespace: &mut Vec<String>) {
        if namespace.len() > MAX_DEPTH {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.is_dir() {
                namespace.push(name);
                self.walk(&path, confine, namespace);
                namespace.pop();
            } else if let Some(stem) = name.strip_suffix(".md") {
                self.file(&path, confine, namespace, stem);
            }
        }
    }

    fn file(&mut self, path: &Path, confine: &Path, namespace: &[String], stem: &str) {
        let segments: Vec<&str> = namespace.iter().map(String::as_str).chain([stem]).collect();
        if segments
            .iter()
            .any(|s| s.is_empty() || s.contains(|c: char| c.is_whitespace() || c == ':'))
        {
            self.commands.warnings.push(format!(
                "ignored {}: command names cannot contain spaces or colons",
                path.display()
            ));
            return;
        }
        let name = segments.join(":");
        if is_builtin(&name) {
            self.commands.warnings.push(format!(
                "ignored {}: /{name} is a built-in command",
                path.display()
            ));
            return;
        }
        if self.commands.get(&name).is_some() {
            return;
        }
        let real = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if !real.starts_with(confine) {
            self.commands.warnings.push(format!(
                "ignored {}: it links to {}, outside {}",
                path.display(),
                real.display(),
                confine.display()
            ));
            return;
        }
        if !std::fs::metadata(&real).is_ok_and(|m| m.is_file()) {
            return;
        }
        let mut bytes = Vec::new();
        let read = std::fs::File::open(&real)
            .and_then(|f| f.take(MAX_FILE_BYTES as u64).read_to_end(&mut bytes));
        if let Err(e) = read {
            self.commands
                .warnings
                .push(format!("cannot read {}: {e}", path.display()));
            return;
        }
        let text = String::from_utf8_lossy(&bytes);
        let (front, body) = frontmatter::parse(&text);
        self.commands.custom.push(CustomCommand {
            name,
            path: path.to_path_buf(),
            description: front.description,
            argument_hint: front.argument_hint,
            model: front.model,
            allowed_tools: front.allowed_tools,
            body: body.to_string(),
        });
    }
}

/// A slash command as typed: `/name` and the rest of the input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Invocation<'a> {
    pub name: &'a str,
    pub args: &'a str,
}

/// Reads `input` as a slash command: `/`, a name of letters, digits, `-`, `_`, `.` and `:`, then
/// whitespace or the end. Anything else, such as `/usr/bin/env is missing`, is ordinary text.
pub fn parse_invocation(input: &str) -> Option<Invocation<'_>> {
    let rest = input.trim_start().strip_prefix('/')?;
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let name = &rest[..end];
    let valid = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'));
    valid.then(|| Invocation {
        name,
        args: rest[end..].trim(),
    })
}

/// Splits command arguments on whitespace. Double quotes group words and understand `\"` and
/// `\\`; single quotes group words literally.
pub fn split_args(args: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_word = false;
    let mut chars = args.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                in_word = true;
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => match chars.next() {
                            Some(next @ ('"' | '\\')) => current.push(next),
                            Some(next) => {
                                current.push('\\');
                                current.push(next);
                            }
                            None => current.push('\\'),
                        },
                        other => current.push(other),
                    }
                }
            }
            '\'' => {
                in_word = true;
                for c in chars.by_ref() {
                    if c == '\'' {
                        break;
                    }
                    current.push(c);
                }
            }
            c if c.is_whitespace() => {
                if in_word {
                    out.push(std::mem::take(&mut current));
                    in_word = false;
                }
            }
            other => {
                in_word = true;
                current.push(other);
            }
        }
    }
    if in_word {
        out.push(current);
    }
    out
}
