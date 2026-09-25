use async_trait::async_trait;
use harness_core::{
    message::ToolSpec,
    permission::Action,
    tool::{Tool, ToolContext, ToolOutput},
};
use serde_json::{Value, json};

const DEFAULT_LIMIT: usize = 2000;
const MAX_LINE_CHARS: usize = 2000;
/// Rendered output (numbered lines) stops once it would cross this many bytes, so a page never
/// approaches the tool-output spill limit and gets truncated with a head/tail hole in the middle.
const BYTE_BUDGET: usize = 8_000;

pub struct ReadTool;

/// A file is treated as binary when its first 8 KB contain a NUL byte.
pub fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|b| *b == 0)
}

#[async_trait]
impl Tool for ReadTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read".into(),
            description: "Read a text file. Returns numbered lines; use offset (1-based) and limit to page through large files.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path, relative to the workspace or absolute"},
                    "offset": {"type": "integer", "minimum": 1},
                    "limit": {"type": "integer", "minimum": 1}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        }
    }

    fn action(&self, args: &Value, ctx: &ToolContext) -> Action {
        Action::Read(ctx.resolve(args["path"].as_str().unwrap_or_default()))
    }

    async fn run(&self, args: Value, ctx: &ToolContext) -> ToolOutput {
        let path = ctx.resolve(args["path"].as_str().unwrap_or_default());
        let bytes = match tokio::fs::read(&path).await {
            Ok(bytes) => bytes,
            Err(e) => return ToolOutput::error(format!("cannot read {}: {e}", path.display())),
        };
        if is_binary(&bytes) {
            return ToolOutput::error(format!("{} is a binary file", path.display()));
        }
        ctx.tracker.record(&path, &bytes);

        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = text.lines().collect();
        let offset = args["offset"].as_u64().unwrap_or(1).max(1) as usize;
        let limit = args["limit"]
            .as_u64()
            .map(|l| l as usize)
            .unwrap_or(DEFAULT_LIMIT);
        if lines.is_empty() {
            return ToolOutput::ok("[empty file]\n");
        }
        let mut out = String::new();
        let mut shown = 0usize;
        for (index, line) in lines.iter().enumerate().skip(offset - 1).take(limit) {
            let formatted = if line.chars().count() > MAX_LINE_CHARS {
                let truncated: String = line.chars().take(MAX_LINE_CHARS).collect();
                format!("{:>6}\t{truncated} [line truncated]\n", index + 1)
            } else {
                format!("{:>6}\t{line}\n", index + 1)
            };
            // Always show at least one line, even if that line alone exceeds the budget.
            if shown > 0 && out.len() + formatted.len() > BYTE_BUDGET {
                break;
            }
            out.push_str(&formatted);
            shown += 1;
        }
        let end = offset - 1 + shown;
        if end < lines.len() {
            out.push_str(&format!(
                "[... {} more lines; call read with offset={} to continue]\n",
                lines.len() - end,
                end + 1
            ));
        }
        if out.is_empty() {
            out = format!(
                "[offset {offset} is past the end of the file ({} lines)]\n",
                lines.len()
            );
        }
        // Check if the file is valid UTF-8; if not, append a note
        if std::str::from_utf8(&bytes).is_err() {
            out.push_str(
                "[note: file is not valid UTF-8; invalid bytes are shown as U+FFFD, so rewriting it would change those bytes]\n",
            );
        }
        ToolOutput::ok(out)
    }
}
