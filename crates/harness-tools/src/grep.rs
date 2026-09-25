use std::path::{Path, PathBuf};

use async_trait::async_trait;
use harness_core::{
    message::ToolSpec,
    permission::Action,
    tool::{Tool, ToolContext, ToolOutput},
};
use regex::Regex;
use serde_json::{Value, json};

use crate::{read::is_binary, walk};

const MAX_MATCHES: usize = 200;
const MAX_LINE_CHARS: usize = 500;

pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "grep".into(),
            description: "Search file contents with a regular expression. Returns path:line:text. Respects .gitignore.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string"},
                    "path": {"type": "string", "description": "File or directory; defaults to the workspace"},
                    "glob": {"type": "string", "description": "Only search files matching this glob, e.g. *.rs"}
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
        let pattern = args["pattern"].as_str().unwrap_or_default();
        let regex = match Regex::new(pattern) {
            Ok(regex) => regex,
            Err(e) => return ToolOutput::error(format!("invalid regex `{pattern}`: {e}")),
        };
        let filter = match args["glob"].as_str().map(globset::Glob::new) {
            None => None,
            Some(Ok(glob)) => Some(glob.compile_matcher()),
            Some(Err(e)) => return ToolOutput::error(format!("invalid glob: {e}")),
        };
        let base = ctx.resolve(args["path"].as_str().unwrap_or("."));
        if !base.exists() {
            return ToolOutput::error(format!("no such file or directory: {}", base.display()));
        }
        let workspace = ctx.workspace.clone();
        tokio::task::spawn_blocking(move || search(&regex, filter.as_ref(), &base, &workspace))
            .await
            .unwrap_or_else(|e| ToolOutput::error(format!("grep failed: {e}")))
    }
}

fn search(
    regex: &Regex,
    filter: Option<&globset::GlobMatcher>,
    base: &Path,
    workspace: &Path,
) -> ToolOutput {
    let files: Vec<PathBuf> = if base.is_file() {
        vec![base.to_path_buf()]
    } else {
        walk::files(base)
    };
    let mut shown = Vec::new();
    let mut total = 0usize;
    for file in files {
        let rel = file.strip_prefix(workspace).unwrap_or(&file).to_path_buf();
        if let Some(filter) = filter {
            let name_matches = file.file_name().is_some_and(|n| filter.is_match(n));
            if !name_matches && !filter.is_match(&rel) {
                continue;
            }
        }
        let Ok(bytes) = std::fs::read(&file) else {
            continue;
        };
        if is_binary(&bytes) {
            continue;
        }
        for (index, line) in String::from_utf8_lossy(&bytes).lines().enumerate() {
            if regex.is_match(line) {
                total += 1;
                if shown.len() < MAX_MATCHES {
                    let text: String = line.chars().take(MAX_LINE_CHARS).collect();
                    shown.push(format!("{}:{}:{}", rel.display(), index + 1, text));
                }
            }
        }
    }
    if total == 0 {
        return ToolOutput::ok("No matches");
    }
    let mut out = shown.join("\n");
    if total > MAX_MATCHES {
        out.push_str(&format!(
            "\n[... {} more matches not shown; narrow the pattern or path]",
            total - MAX_MATCHES
        ));
    }
    ToolOutput::ok(out)
}
