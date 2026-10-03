//! The `hashline` edit format (experimental): `read` starts each line with an address, its number
//! and a short hash of its content (`12#a1f`), and `hashline_edit` replaces a range of lines named
//! by two addresses. An address whose hash is not the line's now is stale, and the edit is refused
//! with what the line holds.

use async_trait::async_trait;
use harness_core::{
    message::ToolSpec,
    permission::Action,
    tool::{Tool, ToolContext, ToolOutput},
};
use serde_json::{Value, json};

/// The address of line `number` (1-based) holding `line`: the number, `#`, and 3 hex digits.
pub fn address(number: usize, line: &str) -> String {
    format!("{number}#{:03x}", hash(line))
}

/// FNV-1a over the line's bytes, kept to 12 bits.
fn hash(line: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for byte in line.bytes() {
        h ^= u32::from(byte);
        h = h.wrapping_mul(0x0100_0193);
    }
    (h ^ (h >> 12) ^ (h >> 24)) & 0xfff
}

/// The line number and hash an address names; `None` if it is not one (line numbers start at 1).
pub fn parse_address(text: &str) -> Option<(usize, String)> {
    let (number, hash) = text.trim().split_once('#')?;
    let number: usize = number.parse().ok().filter(|n| *n >= 1)?;
    (hash.len() == 3 && hash.chars().all(|c| c.is_ascii_hexdigit()))
        .then(|| (number, hash.to_ascii_lowercase()))
}

pub struct HashlineEditTool;

/// At most this many lines of the edited region are shown with their new addresses.
const SHOWN: usize = 20;

#[async_trait]
impl Tool for HashlineEditTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "hashline_edit".into(),
            description: "Replace lines of a file you read. start and end are line addresses as read shows them (like 12#a1f; end defaults to start) and the lines from start to end are replaced by new_text (empty deletes them). Addresses go stale when the file changes: read it again.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "start": {"type": "string", "description": "Address of the first line, like 12#a1f"},
                    "end": {"type": "string", "description": "Address of the last line; defaults to start"},
                    "new_text": {"type": "string"}
                },
                "required": ["path", "start", "new_text"],
                "additionalProperties": false
            }),
        }
    }

    fn action(&self, args: &Value, ctx: &ToolContext) -> Action {
        Action::Write(ctx.resolve(args["path"].as_str().unwrap_or_default()))
    }

    fn changed_paths(&self, args: &Value, ctx: &ToolContext) -> Vec<std::path::PathBuf> {
        vec![ctx.resolve(args["path"].as_str().unwrap_or_default())]
    }

    async fn run(&self, args: Value, ctx: &ToolContext) -> ToolOutput {
        let path = ctx.resolve(args["path"].as_str().unwrap_or_default());
        let new_text = args["new_text"].as_str().unwrap_or_default();
        let given = |field: &str| args[field].as_str().map(str::to_string);
        let start_text = given("start").unwrap_or_default();
        let end_text = given("end").unwrap_or_else(|| start_text.clone());
        let (Some(start), Some(end)) = (parse_address(&start_text), parse_address(&end_text))
        else {
            return ToolOutput::error(
                "start and end are addresses as read shows them: a line number, `#`, and 3 hex digits, like 12#a1f",
            );
        };
        if end.0 < start.0 {
            return ToolOutput::error(format!("end ({end_text}) is before start ({start_text})"));
        }
        let bytes = match tokio::fs::read(&path).await {
            Ok(bytes) => bytes,
            Err(e) => return ToolOutput::error(format!("cannot read {}: {e}", path.display())),
        };
        // A changed file is not refused as a whole, as `edit` does: the addresses say what
        // changed, and the error shows it. A file never read has no addresses to trust.
        if !ctx.tracker.was_read(&path) {
            return ToolOutput::error(format!(
                "{} exists but has not been read in this session; read it first",
                path.display()
            ));
        }
        let Ok(text) = String::from_utf8(bytes) else {
            return ToolOutput::error(format!("{} is not valid UTF-8", path.display()));
        };
        let lines: Vec<&str> = text.lines().collect();
        for (number, wanted) in [&start, &end] {
            let Some(line) = lines.get(number - 1) else {
                return ToolOutput::error(format!(
                    "the file has {} lines, and there is no line {number}; read it again",
                    lines.len()
                ));
            };
            if hash(line) != u32::from_str_radix(wanted, 16).unwrap_or(u32::MAX) {
                let now: Vec<String> = lines
                    .iter()
                    .enumerate()
                    .skip(number - 1)
                    .take(3)
                    .map(|(i, l)| format!("{:>10}\t{l}", address(i + 1, l)))
                    .collect();
                return ToolOutput::error(format!(
                    "line {number} is not what you read ({number}#{wanted}): the file changed. It now reads:\n{}\nRead the file again, and use the addresses it shows",
                    now.join("\n")
                ));
            }
        }
        let replacement: Vec<&str> = if new_text.is_empty() {
            Vec::new()
        } else {
            new_text
                .strip_suffix('\n')
                .unwrap_or(new_text)
                .split('\n')
                .collect()
        };
        let mut result: Vec<&str> = lines[..start.0 - 1].to_vec();
        result.extend(&replacement);
        result.extend(&lines[end.0..]);
        let eol = crate::patch::line_ending(&text);
        let mut updated = result.join(eol);
        if text.ends_with('\n') && !result.is_empty() {
            updated.push_str(eol);
        }
        if let Err(e) = tokio::fs::write(&path, &updated).await {
            return ToolOutput::error(format!("cannot write {}: {e}", path.display()));
        }
        ctx.tracker.record(&path, updated.as_bytes());
        let shown: Vec<String> = replacement
            .iter()
            .enumerate()
            .take(SHOWN)
            .map(|(i, l)| format!("{:>10}\t{l}", address(start.0 + i, l)))
            .collect();
        let more = replacement.len().saturating_sub(SHOWN);
        let mut report = format!(
            "Replaced lines {}-{} with {} line(s). The lines after them moved, so read the file again for their addresses.",
            start.0,
            end.0,
            replacement.len()
        );
        if !shown.is_empty() {
            report.push_str("\nNew lines:\n");
            report.push_str(&shown.join("\n"));
            if more > 0 {
                report.push_str(&format!("\n[... {more} more lines]"));
            }
        }
        ToolOutput::ok(report)
    }
}
