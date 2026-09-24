use async_trait::async_trait;
use harness_core::{
    message::ToolSpec,
    permission::Action,
    tool::{Tool, ToolContext, ToolOutput},
};
use serde_json::{Value, json};

pub struct WriteTool;

#[async_trait]
impl Tool for WriteTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write".into(),
            description: "Create a file or replace its whole content. Read an existing file before overwriting it.".into(),
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

    async fn run(&self, args: Value, ctx: &ToolContext) -> ToolOutput {
        let path = ctx.resolve(args["path"].as_str().unwrap_or_default());
        let content = args["content"].as_str().unwrap_or_default();
        if let Ok(existing) = tokio::fs::read(&path).await
            && let Err(message) = ctx.tracker.check_fresh(&path, &existing)
        {
            return ToolOutput::error(message);
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
