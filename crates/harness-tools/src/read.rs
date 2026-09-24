use async_trait::async_trait;
use harness_core::{
    message::ToolSpec,
    permission::Action,
    tool::{Tool, ToolContext, ToolOutput},
};
use serde_json::{Value, json};

const DEFAULT_LIMIT: usize = 2000;
const MAX_LINE_CHARS: usize = 2000;

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
        for (index, line) in lines.iter().enumerate().skip(offset - 1).take(limit) {
            if line.chars().count() > MAX_LINE_CHARS {
                let shown: String = line.chars().take(MAX_LINE_CHARS).collect();
                out.push_str(&format!("{:>6}\t{shown} [line truncated]\n", index + 1));
            } else {
                out.push_str(&format!("{:>6}\t{line}\n", index + 1));
            }
        }
        let end = (offset - 1 + limit).min(lines.len());
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
