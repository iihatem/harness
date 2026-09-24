use async_trait::async_trait;
use harness_core::{
    message::ToolSpec,
    permission::Action,
    tool::{Tool, ToolContext, ToolOutput},
};
use serde_json::{Value, json};
use similar::TextDiff;

pub struct EditTool;

#[async_trait]
impl Tool for EditTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit".into(),
            description: "Replace an exact string in a file. old_string must match exactly once unless replace_all is true. Read the file first.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "old_string": {"type": "string"},
                    "new_string": {"type": "string"},
                    "replace_all": {"type": "boolean"}
                },
                "required": ["path", "old_string", "new_string"],
                "additionalProperties": false
            }),
        }
    }

    fn action(&self, args: &Value, ctx: &ToolContext) -> Action {
        Action::Write(ctx.resolve(args["path"].as_str().unwrap_or_default()))
    }

    async fn run(&self, args: Value, ctx: &ToolContext) -> ToolOutput {
        let path = ctx.resolve(args["path"].as_str().unwrap_or_default());
        let old = args["old_string"].as_str().unwrap_or_default();
        let new = args["new_string"].as_str().unwrap_or_default();
        let replace_all = args["replace_all"].as_bool().unwrap_or(false);
        if old.is_empty() {
            return ToolOutput::error("old_string must not be empty");
        }
        let bytes = match tokio::fs::read(&path).await {
            Ok(bytes) => bytes,
            Err(e) => return ToolOutput::error(format!("cannot read {}: {e}", path.display())),
        };
        if let Err(message) = ctx.tracker.check_fresh(&path, &bytes) {
            return ToolOutput::error(message);
        }
        let Ok(text) = String::from_utf8(bytes) else {
            return ToolOutput::error(format!("{} is not valid UTF-8", path.display()));
        };
        let display = path
            .strip_prefix(&ctx.workspace)
            .unwrap_or(&path)
            .display()
            .to_string();
        match text.matches(old).count() {
            0 => {
                return ToolOutput::error(format!("old_string not found in {display} (0 matches)"));
            }
            n if n > 1 && !replace_all => {
                return ToolOutput::error(format!(
                    "old_string matches {n} times in {display}; include more surrounding text or set replace_all"
                ));
            }
            _ => {}
        }
        let updated = if replace_all {
            text.replace(old, new)
        } else {
            text.replacen(old, new, 1)
        };
        if let Err(e) = tokio::fs::write(&path, &updated).await {
            return ToolOutput::error(format!("cannot write {}: {e}", path.display()));
        }
        ctx.tracker.record(&path, updated.as_bytes());
        let diff = TextDiff::from_lines(&text, &updated)
            .unified_diff()
            .header(&display, &display)
            .to_string();
        ToolOutput::ok(diff)
    }
}
