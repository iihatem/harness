//! The V4A patch format `apply_patch` takes: a parser, and the matching of a file's hunks to its
//! lines. Nothing here touches the disk.
//!
//! ```text
//! *** Begin Patch
//! *** Add File: src/b.rs
//! +fn b() {}
//! *** Update File: src/a.rs
//! @@ fn a()
//!  context
//! -removed
//! +added
//! *** Delete File: old.rs
//! *** End Patch
//! ```

use std::fmt;

/// What a hunk line does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Remove,
    Add,
}

/// One change to a file: the lines it expects (context and removals) and what replaces them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// The text after `@@`: a line that comes before the change, to find its place by.
    pub header: Option<String>,
    pub lines: Vec<(LineKind, String)>,
    /// The hunk is at the end of the file (`*** End of File`).
    pub at_eof: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileOp {
    Add {
        path: String,
        content: String,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        move_to: Option<String>,
        hunks: Vec<Hunk>,
    },
}

impl FileOp {
    pub fn path(&self) -> &str {
        match self {
            FileOp::Add { path, .. } | FileOp::Delete { path } | FileOp::Update { path, .. } => {
                path
            }
        }
    }
}

/// Why a patch was refused: what is wrong, and where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchError(pub String);

impl fmt::Display for PatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PatchError {}

fn fail<T>(message: String) -> Result<T, PatchError> {
    Err(PatchError(message))
}

const BEGIN: &str = "*** Begin Patch";
const END: &str = "*** End Patch";

/// Parses a patch into the changes to files it asks for.
pub fn parse(patch: &str) -> Result<Vec<FileOp>, PatchError> {
    let lines: Vec<&str> = patch
        .lines()
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();
    let mut at = 0;
    while lines.get(at).is_some_and(|l| l.trim().is_empty()) {
        at += 1;
    }
    if lines.get(at).map(|l| l.trim()) != Some(BEGIN) {
        return fail(format!("the patch must start with `{BEGIN}`"));
    }
    let mut end = lines.len();
    while end > at + 1 && lines[end - 1].trim().is_empty() {
        end -= 1;
    }
    if end <= at + 1 || lines[end - 1].trim() != END {
        return fail(format!("the patch must end with `{END}`"));
    }
    let body = &lines[at + 1..end - 1];
    let mut ops: Vec<FileOp> = Vec::new();
    let mut i = 0;
    while i < body.len() {
        let line = body[i];
        i += 1;
        if line.trim().is_empty() {
            continue;
        }
        let Some(directive) = line.strip_prefix("*** ") else {
            return fail(format!(
                "unexpected line outside a file section: `{}`",
                shorten(line)
            ));
        };
        let (kind, path) = match directive.split_once(':') {
            Some((kind, path)) => (kind, path.trim()),
            None => (directive, ""),
        };
        match kind {
            "Add File" | "Update File" | "Delete File" => {}
            other => return fail(format!("unknown directive `*** {}`", shorten(other))),
        }
        if path.is_empty() {
            return fail(format!("`*** {kind}` needs a path"));
        }
        if ops.iter().any(|op| op.path() == path) {
            return fail(format!(
                "{path} appears twice in the patch; put its changes in one section"
            ));
        }
        // The section runs to the next directive that starts a file.
        let section_end = body[i..]
            .iter()
            .position(|l| {
                ["*** Add File:", "*** Update File:", "*** Delete File:"]
                    .iter()
                    .any(|d| l.starts_with(d))
            })
            .map_or(body.len(), |n| i + n);
        let section = &body[i..section_end];
        i = section_end;
        ops.push(match kind {
            "Add File" => add_file(path, section)?,
            "Delete File" => {
                if let Some(extra) = section.iter().find(|l| !l.trim().is_empty()) {
                    return fail(format!(
                        "{path}: a deleted file takes no lines, found `{}`",
                        shorten(extra)
                    ));
                }
                FileOp::Delete { path: path.into() }
            }
            _ => update_file(path, section)?,
        });
    }
    if ops.is_empty() {
        return fail("the patch makes no changes".into());
    }
    Ok(ops)
}

fn shorten(text: &str) -> String {
    let mut shown: String = text.chars().take(60).collect();
    if shown.len() < text.len() {
        shown.push('…');
    }
    shown
}

fn add_file(path: &str, section: &[&str]) -> Result<FileOp, PatchError> {
    let mut content = String::new();
    for line in section {
        match line.strip_prefix('+') {
            Some(text) => {
                content.push_str(text);
                content.push('\n');
            }
            None => {
                return fail(format!(
                    "{path}: every line of an added file starts with `+`, found `{}`",
                    shorten(line)
                ));
            }
        }
    }
    Ok(FileOp::Add {
        path: path.into(),
        content,
    })
}

fn update_file(path: &str, section: &[&str]) -> Result<FileOp, PatchError> {
    let mut move_to = None;
    let mut hunks: Vec<Hunk> = Vec::new();
    let mut current: Option<Hunk> = None;
    for (index, line) in section.iter().enumerate() {
        if index == 0
            && let Some(target) = line.strip_prefix("*** Move to:")
        {
            let target = target.trim();
            if target.is_empty() {
                return fail(format!("{path}: `*** Move to:` needs a path"));
            }
            move_to = Some(target.to_string());
            continue;
        }
        if *line == "*** End of File" {
            match current.as_mut() {
                Some(hunk) => hunk.at_eof = true,
                None => return fail(format!("{path}: `*** End of File` outside a hunk")),
            }
            continue;
        }
        if line.starts_with("***") {
            return fail(format!("{path}: unexpected `{}` in a hunk", shorten(line)));
        }
        if let Some(rest) = line.strip_prefix("@@") {
            hunks.extend(current.take());
            let header = rest.trim();
            current = Some(Hunk {
                header: (!header.is_empty()).then(|| header.to_string()),
                lines: Vec::new(),
                at_eof: false,
            });
            continue;
        }
        let (kind, text) = match line.chars().next() {
            Some(' ') => (LineKind::Context, &line[1..]),
            Some('-') => (LineKind::Remove, &line[1..]),
            Some('+') => (LineKind::Add, &line[1..]),
            // A model drops the space of a blank context line.
            None => (LineKind::Context, ""),
            Some(_) => {
                let number = hunks.len() + 1;
                return fail(format!(
                    "{path}, hunk {number}: a line must start with ` `, `-` or `+`, found `{}`",
                    shorten(line)
                ));
            }
        };
        current
            .get_or_insert_with(|| Hunk {
                header: None,
                lines: Vec::new(),
                at_eof: false,
            })
            .lines
            .push((kind, text.to_string()));
    }
    hunks.extend(current);
    if hunks.is_empty() && move_to.is_none() {
        return fail(format!("{path}: an update has no hunks"));
    }
    if let Some(number) = hunks.iter().position(|h| h.lines.is_empty()) {
        return fail(format!("{path}, hunk {}: the hunk is empty", number + 1));
    }
    Ok(FileOp::Update {
        path: path.into(),
        move_to,
        hunks,
    })
}

/// How strictly two lines are compared, from the strictest.
const COMPARE: [fn(&str, &str) -> bool; 3] = [
    |a, b| a == b,
    |a, b| a.trim_end() == b.trim_end(),
    |a, b| a.trim() == b.trim(),
];

/// Where `wanted` first occurs in `lines` at or after `from`, comparing as strictly as it takes;
/// and which comparison found it (0 is exact).
fn find(lines: &[&str], wanted: &[&str], from: usize, at_end: bool) -> Option<(usize, usize)> {
    if wanted.len() > lines.len() {
        return None;
    }
    let last = lines.len() - wanted.len();
    for (level, same) in COMPARE.iter().enumerate() {
        let matches = |at: usize| wanted.iter().zip(&lines[at..]).all(|(w, l)| same(w, l));
        if at_end {
            if last >= from && matches(last) {
                return Some((last, level));
            }
        } else if let Some(found) = (from..=last).find(|&at| matches(at)) {
            return Some((found, level));
        }
    }
    None
}

/// The line ending a file is written back with: `\r\n` when every line break in it is one, else
/// `\n` (a file of mixed endings gets plain ones).
pub fn line_ending(text: &str) -> &'static str {
    let breaks = text.matches('\n').count();
    if breaks > 0 && text.matches("\r\n").count() == breaks {
        "\r\n"
    } else {
        "\n"
    }
}

fn indent(line: &str) -> &str {
    &line[..line.len() - line.trim_start().len()]
}

/// `original` with `hunks` applied. Each hunk is looked for after the one before it; its context
/// lines keep the file's own text. An error names `path` and the hunk, and nothing is changed.
pub fn apply_hunks(path: &str, original: &str, hunks: &[Hunk]) -> Result<String, PatchError> {
    let lines: Vec<&str> = original.lines().collect();
    let ends_with_newline = original.is_empty() || original.ends_with('\n');
    let mut out: Vec<String> = Vec::new();
    let mut pos = 0;
    for (index, hunk) in hunks.iter().enumerate() {
        let number = index + 1;
        let place = |what: String| PatchError(format!("{path}, hunk {number}: {what}"));
        if let Some(header) = &hunk.header {
            let wanted = [header.as_str()];
            let Some((at, _)) = find(&lines, &wanted, pos, false) else {
                return Err(place(format!(
                    "the line `{}` was not found",
                    shorten(header)
                )));
            };
            // The header line stays; the hunk starts after it.
            out.extend(lines[pos..=at].iter().map(|l| l.to_string()));
            pos = at + 1;
        }
        let wanted: Vec<&str> = hunk
            .lines
            .iter()
            .filter(|(kind, _)| *kind != LineKind::Add)
            .map(|(_, text)| text.as_str())
            .collect();
        let mut added_indent = String::new();
        let start = if wanted.is_empty() {
            // Only additions: after the header, or else at the end.
            if hunk.header.is_some() {
                pos
            } else {
                lines.len().max(pos)
            }
        } else {
            match find(&lines, &wanted, pos, hunk.at_eof) {
                Some((at, level)) => {
                    // Matched only by ignoring indentation: the patch lost the file's, which
                    // its added lines get back.
                    if level == 2 {
                        let (file, patch) = (indent(lines[at]), indent(wanted[0]));
                        if let Some(lost) = file.strip_suffix(patch) {
                            added_indent = lost.to_string();
                        }
                    }
                    at
                }
                None => {
                    return Err(place(format!(
                        "the lines to change were not found; looked for `{}`{}",
                        shorten(wanted[0]),
                        if wanted.len() > 1 {
                            format!(" and {} more", wanted.len() - 1)
                        } else {
                            String::new()
                        }
                    )));
                }
            }
        };
        out.extend(lines[pos..start].iter().map(|l| l.to_string()));
        let mut at = start;
        for (kind, text) in &hunk.lines {
            match kind {
                LineKind::Context => {
                    out.push(lines[at].to_string());
                    at += 1;
                }
                LineKind::Remove => at += 1,
                LineKind::Add if text.is_empty() => out.push(String::new()),
                LineKind::Add => out.push(format!("{added_indent}{text}")),
            }
        }
        pos = at;
    }
    out.extend(lines[pos..].iter().map(|l| l.to_string()));
    let eol = line_ending(original);
    let mut text = out.join(eol);
    if ends_with_newline && !out.is_empty() {
        text.push_str(eol);
    }
    Ok(text)
}
