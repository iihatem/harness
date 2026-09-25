use async_trait::async_trait;
use harness_core::{
    message::ToolSpec,
    permission::Action,
    tool::{Tool, ToolContext, ToolOutput},
};
use serde_json::{Value, json};

use crate::walk;

const MAX_RESULTS: usize = 200;

pub struct GlobTool;

#[async_trait]
impl Tool for GlobTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "glob".into(),
            description: "Find files by glob pattern (e.g. src/**/*.rs). Respects .gitignore."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string"},
                    "path": {"type": "string", "description": "Directory to search; defaults to the workspace"}
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        }
    }

    fn action(&self, args: &Value, ctx: &ToolContext) -> Action {
        Action::Read(ctx.resolve(args["path"].as_str().unwrap_or(".")))
    }

    async fn run(&self, args: Value, ctx: &ToolContext) -> ToolOutput {
        let base = ctx.resolve(args["path"].as_str().unwrap_or("."));
        if !base.exists() {
            return ToolOutput::error(format!("no such file or directory: {}", base.display()));
        }
        let pattern = args["pattern"].as_str().unwrap_or_default().to_string();
        let matcher = match globset::Glob::new(&pattern) {
            Ok(glob) => glob.compile_matcher(),
            Err(e) => return ToolOutput::error(format!("invalid glob `{pattern}`: {e}")),
        };
        let workspace = ctx.workspace.clone();
        let hits = tokio::task::spawn_blocking(move || {
            walk::files(&base)
                .into_iter()
                .filter_map(|path| {
                    let rel = path.strip_prefix(&base).ok()?.to_path_buf();
                    matcher.is_match(&rel).then(|| {
                        let ws_rel = path.strip_prefix(&workspace).unwrap_or(&path).to_path_buf();
                        ws_rel.display().to_string()
                    })
                })
                .collect::<Vec<_>>()
        })
        .await
        .map_err(|e| e.to_string());

        let hits = match hits {
            Ok(h) => h,
            Err(e) => return ToolOutput::error(format!("glob failed: {e}")),
        };

        if hits.is_empty() {
            return ToolOutput::ok(format!("No files matched `{pattern}`"));
        }
        let mut out = hits
            .iter()
            .take(MAX_RESULTS)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        if hits.len() > MAX_RESULTS {
            out.push_str(&format!(
                "\n[... {} more files not shown; narrow the pattern]",
                hits.len() - MAX_RESULTS
            ));
        }
        ToolOutput::ok(out)
    }
}
