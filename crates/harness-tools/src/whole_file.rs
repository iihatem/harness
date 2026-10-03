use async_trait::async_trait;
use harness_core::{
    message::ToolSpec,
    permission::Action,
    tool::{Tool, ToolContext, ToolOutput},
};
use serde_json::{Value, json};

/// A file of this many lines or more is not written whole, and nor is one that replaces it.
pub const MAX_LINES: usize = 400;

/// `write` for the `whole_file` edit format: the model writes complete files, small ones only.
pub struct WholeFileTool;

#[async_trait]
impl Tool for WholeFileTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write".into(),
            description: format!(
                "Create a file or replace its whole content, with the complete new text. Files of {MAX_LINES} lines or more cannot be written this way. Read an existing file before overwriting it."
            ),
            parameters: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}, "content": {"type": "string"}},
                "required": ["path", "content"],
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
        let content = args["content"].as_str().unwrap_or_default();
        let too_big = |what: &str, lines: usize| {
            ToolOutput::error(format!(
                "{what} has {lines} lines; whole-file writes are limited to fewer than {MAX_LINES}. Change it with the str_replace format (the `edit` tool) instead"
            ))
        };
        if let Ok(existing) = tokio::fs::read(&path).await {
            if let Err(message) = ctx.tracker.check_fresh(&path, &existing) {
                return ToolOutput::error(message);
            }
            let lines = String::from_utf8_lossy(&existing).lines().count();
            if lines >= MAX_LINES {
                return too_big(&path.display().to_string(), lines);
            }
        }
        let lines = content.lines().count();
        if lines >= MAX_LINES {
            return too_big("the new content", lines);
        }
        if let Some(parent) = path.parent()
            && let Err(e) = tokio::fs::create_dir_all(parent).await
        {
            return ToolOutput::error(format!("cannot create {}: {e}", parent.display()));
        }
        if let Err(e) = tokio::fs::write(&path, content).await {
            return ToolOutput::error(format!("cannot write {}: {e}", path.display()));
        }
        ctx.tracker.record(&path, content.as_bytes());
        ToolOutput::ok(format!(
            "Wrote {} bytes to {}",
            content.len(),
            path.display()
        ))
    }
}
