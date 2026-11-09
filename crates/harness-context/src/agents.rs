//! Agent definition files: Markdown with YAML frontmatter, from `.harness/agents` and
//! `.claude/agents` in the project, then `agents/` in the harness config directory and
//! `~/.claude/agents`. Every file found is returned, project before user and `.harness` before
//! `.claude`, so that [`Definitions::new`](harness_core::agents::Definitions::new) keeps the
//! first of a name.
//!
//! A repository controls its project files, so they are read as commands are: only regular files
//! that resolve inside the project, no linked directories. A definition can narrow what an agent
//! may do but never widen it: keys that would widen are ignored with a warning.

use std::path::{Path, PathBuf};

use harness_core::{
    agents::{AgentDefinition, DefinitionSource, normalize_tools},
    permission::Mode,
};

use crate::{
    commands::frontmatter::{self, Raw},
    read::{not_regular, read_regular},
};

/// Bytes read from one definition file.
const MAX_FILE_BYTES: usize = 256 * 1024;
/// Characters of a description the model is shown.
const DESCRIPTION_CAP: usize = 300;
/// The longest name.
const NAME_CAP: usize = 40;
/// The most steps a definition may ask for.
const STEPS_CAP: u32 = 1_000;

/// Keys that would give an agent more than its parent has: they are ignored.
const WIDENING: [&str; 10] = [
    "permissionMode",
    "permission_mode",
    "permission-mode",
    "permissions",
    "allowed-tools",
    "allowedTools",
    "allowed_tools",
    "allow",
    "sandbox_mode",
    "approval_policy",
];

/// The most definitions read from one directory, and from all of them: each one's name and
/// description go into the tool description the model is sent with every request.
const DIR_CAP: usize = 50;
const TOTAL_CAP: usize = 100;

/// The definitions found, and what went wrong finding them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Found {
    /// In order of precedence; a name can appear more than once.
    pub definitions: Vec<AgentDefinition>,
    pub warnings: Vec<String>,
}

/// Finds the definition files for a project rooted at `project_root`.
pub fn discover(project_root: &Path, config_dir: &Path, home: Option<&Path>) -> Found {
    let project = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf());
    let mut user_dirs = vec![config_dir.join("agents")];
    if let Some(home) = home {
        user_dirs.push(home.join(".claude/agents"));
    }
    let user_real: Vec<PathBuf> = user_dirs
        .iter()
        .filter_map(|dir| dir.canonicalize().ok())
        .collect();
    let mut found = Found::default();
    for sub in [".harness", ".claude"] {
        let dir = project_root.join(sub).join("agents");
        let Ok(real) = dir.canonicalize() else {
            continue;
        };
        if !real.is_dir() || user_real.contains(&real) {
            continue;
        }
        if !real.starts_with(&project) {
            found.warnings.push(format!(
                "skipped {}: it links to {}, outside the project",
                dir.display(),
                real.display()
            ));
            continue;
        }
        read_dir(&dir, Some(&project), &mut found);
    }
    for dir in &user_dirs {
        if dir.is_dir() {
            read_dir(dir, None, &mut found);
        }
    }
    found
}

/// Reads the `.md` files directly in `dir`. With `confine` (a project's directory), a linked file
/// is skipped and each must resolve inside it.
fn read_dir(dir: &Path, confine: Option<&Path>, found: &mut Found) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
    entries.sort_by_key(|e| e.file_name());
    let mut from_dir = 0;
    for entry in entries {
        let file_name = entry.file_name().to_string_lossy().into_owned();
        let Some(stem) = file_name.strip_suffix(".md") else {
            continue;
        };
        if file_name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            continue;
        }
        if meta.is_symlink() && confine.is_some() {
            found.warnings.push(format!(
                "ignored {}: it is a link, and links in a project's agents are not followed",
                path.display()
            ));
            continue;
        }
        let real = path.canonicalize().unwrap_or_else(|_| path.clone());
        if let Some(confine) = confine
            && !real.starts_with(confine)
        {
            found.warnings.push(format!(
                "ignored {}: it links to {}, outside {}",
                path.display(),
                real.display(),
                confine.display()
            ));
            continue;
        }
        let mut bytes = match read_regular(&real, MAX_FILE_BYTES as u64 + 1) {
            Ok(bytes) => bytes,
            Err(e) if not_regular(&e) => {
                found
                    .warnings
                    .push(format!("ignored {}: not a regular file", path.display()));
                continue;
            }
            Err(e) => {
                found
                    .warnings
                    .push(format!("cannot read {}: {e}", path.display()));
                continue;
            }
        };
        if bytes.len() > MAX_FILE_BYTES {
            bytes.truncate(MAX_FILE_BYTES);
            found.warnings.push(format!(
                "{} is larger than {MAX_FILE_BYTES} bytes; only the start is used",
                path.display()
            ));
        }
        let source = if confine.is_some() {
            DefinitionSource::Project(path.clone())
        } else {
            DefinitionSource::User(path.clone())
        };
        let text = String::from_utf8_lossy(&bytes);
        if let Some(def) = definition(&path, stem, &text, source, &mut found.warnings) {
            if from_dir >= DIR_CAP {
                found.warnings.push(format!(
                    "{}: more than {DIR_CAP} definitions; the rest are ignored",
                    dir.display()
                ));
                return;
            }
            if found.definitions.len() >= TOTAL_CAP {
                found.warnings.push(format!(
                    "more than {TOTAL_CAP} definitions in all; {} and the rest are ignored",
                    path.display()
                ));
                return;
            }
            from_dir += 1;
            found.definitions.push(def);
        }
    }
}

/// The definition `text` (the file at `path`, named `stem`) makes, with what to warn about added
/// to `warnings`.
fn definition(
    path: &Path,
    stem: &str,
    text: &str,
    source: DefinitionSource,
    warnings: &mut Vec<String>,
) -> Option<AgentDefinition> {
    let (pairs, body) = frontmatter::parse_map(text);
    let get = |key: &str| pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v);
    let text_of = |key: &str| get(key).and_then(|raw: &Raw| raw.text.clone());
    let name = text_of("name").unwrap_or_else(|| stem.to_string());
    let valid = !name.is_empty()
        && name.chars().count() <= NAME_CAP
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_'));
    if !valid {
        warnings.push(format!(
            "ignored {}: an agent's name is lowercase letters, digits, - and _ (up to {NAME_CAP}), not `{name}`",
            path.display()
        ));
        return None;
    }
    let widening: Vec<&str> = WIDENING
        .iter()
        .copied()
        .filter(|key| get(key).is_some())
        .collect();
    if !widening.is_empty() {
        warnings.push(format!(
            "{}: ignored {}; an agent definition cannot widen what the agent may do",
            path.display(),
            widening
                .iter()
                .map(|k| format!("`{k}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let description = one_line(
        &text_of("description")
            .or_else(|| {
                body.lines()
                    .map(str::trim)
                    .find(|line| !line.is_empty())
                    .map(String::from)
            })
            .unwrap_or_else(|| "(no description)".into()),
    );
    let tools = get("tools").map(|raw| {
        if raw.list.is_empty() {
            warnings.push(format!(
                "{}: `tools` lists no tools, so the agent gets none; leave the key out for all of its parent's",
                path.display()
            ));
        }
        let (tools, unknown) = normalize_tools(&raw.list);
        if !unknown.is_empty() {
            warnings.push(format!(
                "{}: ignored the tools {}: they are not tools harness has (read, grep, glob, bash, write, edit)",
                path.display(),
                unknown.join(", ")
            ));
        }
        tools
    });
    let mode = text_of("mode").and_then(|mode| match mode.parse::<Mode>() {
        Ok(mode) => Some(mode),
        Err(why) => {
            warnings.push(format!("{}: ignored the mode: {why}", path.display()));
            None
        }
    });
    let max_steps = ["max_steps", "max-steps", "maxTurns", "max_turns"]
        .iter()
        .find_map(|key| text_of(key))
        .and_then(|steps| steps.parse::<u32>().ok())
        .filter(|steps| *steps > 0)
        .map(|steps| steps.min(STEPS_CAP));
    let (model, model_inherit) = model_of(text_of("model"), path, warnings);
    Some(AgentDefinition {
        name,
        description,
        tools,
        model,
        model_inherit,
        mode,
        max_steps,
        isolation: text_of("isolation"),
        background: text_of("background").map(|b| matches!(b.as_str(), "true" | "yes")),
        body: body.to_string(),
        source,
        model_ignored: None,
    })
}

/// What a `model:` value means: `inherit` is the parent's model; `<provider>/<model>` is used as
/// written; anything else (Claude's `sonnet`, `opus`, `haiku`, ...) is not a harness model, and is
/// ignored with a note, never mapped to a model that costs money.
fn model_of(
    value: Option<String>,
    path: &Path,
    warnings: &mut Vec<String>,
) -> (Option<String>, bool) {
    let Some(value) = value else {
        return (None, false);
    };
    if value.eq_ignore_ascii_case("inherit") {
        return (None, true);
    }
    if value.contains('/') && !value.chars().any(is_unsafe) {
        return (Some(value), false);
    }
    warnings.push(format!(
        "{}: ignored `model: {}`: harness models are `<provider>/<model>` (`inherit` is the parent's), so the agent runs on the default model",
        path.display(),
        clean(&value)
    ));
    (None, false)
}

/// Control characters and the bidirectional overrides and isolates.
fn is_unsafe(c: char) -> bool {
    c.is_control()
        || ('\u{200b}'..='\u{200f}').contains(&c)
        || ('\u{202a}'..='\u{202e}').contains(&c)
        || ('\u{2066}'..='\u{2069}').contains(&c)
        || c == '\u{feff}'
}

/// `text` without the characters a terminal or a reader could be fooled by.
fn clean(text: &str) -> String {
    text.chars().filter(|c| !is_unsafe(*c)).collect()
}

/// `text` on one line, cut to [`DESCRIPTION_CAP`] characters, without control or bidirectional
/// characters (they would reach the terminal in `/agents` and the model's tool description).
fn one_line(text: &str) -> String {
    let joined = clean(text).split_whitespace().collect::<Vec<_>>().join(" ");
    match joined.char_indices().nth(DESCRIPTION_CAP) {
        Some((end, _)) => format!("{}…", &joined[..end]),
        None => joined,
    }
}
