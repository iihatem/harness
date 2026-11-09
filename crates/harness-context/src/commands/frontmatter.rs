//! The YAML frontmatter at the top of a command file. Only the fields harness uses are read, with
//! a small parser that understands the YAML command files use in practice: `key: value` lines,
//! quoted values, `|` and `>` block scalars, and lists written as `[a, b]`, as `- item` lines, or
//! (for `allowed-tools`) as a comma-separated string.

/// The fields harness honours. Everything else in the frontmatter is ignored.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Frontmatter {
    pub description: Option<String>,
    pub argument_hint: Option<String>,
    pub model: Option<String>,
    pub allowed_tools: Vec<String>,
}

/// Splits a command file into its frontmatter and its body. A file that does not start with a
/// `---` line, or whose frontmatter is never closed, is all body.
pub fn parse(text: &str) -> (Frontmatter, &str) {
    let (yaml, body) = split(text);
    (yaml.map(fields).unwrap_or_default(), body)
}

/// The YAML between the `---` lines at the top of `text`, when there is any, and the rest.
fn split(text: &str) -> (Option<&str>, &str) {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let Some(rest) = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
    else {
        return (None, text);
    };
    let mut offset = 0;
    let mut end = None;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "---" {
            end = Some((offset, offset + line.len()));
            break;
        }
        offset += line.len();
    }
    let Some((yaml_end, body_start)) = end else {
        return (None, text);
    };
    (Some(&rest[..yaml_end]), &rest[body_start..])
}

/// One key of the frontmatter as written: the key, its value on the same line, and the indented
/// lines (or `- item` lines) that follow it.
type Entry<'a> = (String, &'a str, &'a [&'a str]);

/// Calls `each` with every top-level `key: value` line of `yaml` and the lines that belong to it.
fn entries(yaml: &str, mut each: impl FnMut(Entry<'_>)) {
    let lines: Vec<&str> = yaml.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        i += 1;
        if line.starts_with([' ', '\t', '#', '-']) {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        // Indented lines that follow belong to this key: a block scalar or a block list.
        let start = i;
        while i < lines.len()
            && (lines[i].starts_with([' ', '\t'])
                || lines[i].trim().is_empty()
                || (value.is_empty() && lines[i].starts_with('-')))
        {
            i += 1;
        }
        each((key.trim().to_string(), value, &lines[start..i]));
    }
}

fn fields(yaml: &str) -> Frontmatter {
    let mut out = Frontmatter::default();
    entries(yaml, |(key, value, block)| match key.as_str() {
        "description" => out.description = scalar(value, block),
        "argument-hint" => out.argument_hint = scalar(value, block),
        "model" => out.model = scalar(value, block),
        "allowed-tools" => out.allowed_tools = list(value, block),
        _ => {}
    });
    out
}

/// A key's value read both ways, for files whose keys are not known beforehand.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Raw {
    /// As a scalar: plain, quoted, or a block scalar; `None` when empty.
    pub text: Option<String>,
    /// As a list: `[a, b]`, `- item` lines, or a comma-separated string.
    pub list: Vec<String>,
}

/// Splits `text` like [`parse`], and gives every key of its frontmatter, in order, with its value.
pub fn parse_map(text: &str) -> (Vec<(String, Raw)>, &str) {
    let (yaml, body) = split(text);
    let mut pairs = Vec::new();
    if let Some(yaml) = yaml {
        entries(yaml, |(key, value, block)| {
            pairs.push((
                key,
                Raw {
                    text: scalar(value, block),
                    list: list(value, block),
                },
            ));
        });
    }
    (pairs, body)
}

/// A scalar value: plain, quoted, or a `|`/`>` block scalar. Empty values are `None`.
fn scalar(value: &str, block: &[&str]) -> Option<String> {
    let text = match value {
        "|" | "|-" | ">" | ">-" => {
            let lines: Vec<&str> = block.iter().map(|l| l.trim()).collect();
            let joined = if value.starts_with('|') {
                lines.join("\n")
            } else {
                lines.join(" ")
            };
            joined.trim().to_string()
        }
        _ => unquote(value),
    };
    (!text.is_empty()).then_some(text)
}

/// A list value: `[a, b]`, `- item` lines, or a comma-separated string.
fn list(value: &str, block: &[&str]) -> Vec<String> {
    let items: Vec<String> = if value.is_empty() {
        block
            .iter()
            .filter_map(|l| l.trim().strip_prefix('-'))
            .map(|item| unquote(item.trim()))
            .collect()
    } else if let Some(inner) = value.strip_prefix('[').and_then(|v| v.strip_suffix(']')) {
        split_top_level(inner)
            .map(|item| unquote(item.trim()))
            .collect()
    } else {
        split_top_level(&unquote(value))
            .map(|item| item.trim().to_string())
            .collect()
    };
    items.into_iter().filter(|item| !item.is_empty()).collect()
}

/// Splits on commas that are not inside parentheses, so `Bash(a, b), Read` is two items.
fn split_top_level(text: &str) -> impl Iterator<Item = &str> {
    let mut depth = 0i32;
    let mut start = 0;
    let mut parts = Vec::new();
    for (i, c) in text.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth <= 0 => {
                parts.push(&text[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&text[start..]);
    parts.into_iter()
}

/// `"text"` or `'text'` without its quotes and with YAML's escapes undone; anything else as is.
fn unquote(value: &str) -> String {
    if let Some(inner) = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
        let mut out = String::new();
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            match (c, chars.clone().next()) {
                ('\\', Some(next)) => {
                    chars.next();
                    out.push(match next {
                        'n' => '\n',
                        't' => '\t',
                        other => other,
                    });
                }
                _ => out.push(c),
            }
        }
        out
    } else if let Some(inner) = value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')) {
        inner.replace("''", "'")
    } else {
        value.to_string()
    }
}
