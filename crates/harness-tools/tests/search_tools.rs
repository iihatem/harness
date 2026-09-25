use std::path::Path;

use harness_core::tool::{Tool, ToolContext};
use harness_tools::{GlobTool, GrepTool};
use serde_json::json;

fn put(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn project() -> (tempfile::TempDir, ToolContext) {
    let dir = tempfile::tempdir().unwrap();
    put(dir.path(), ".gitignore", "target/\n");
    put(dir.path(), "src/main.rs", "use std::io;\nfn main() {}\n");
    put(dir.path(), "src/notes.txt", "fn main is mentioned here\n");
    put(dir.path(), "target/debug/gen.rs", "fn main() {}\n");
    put(dir.path(), ".github/ci.yml", "on: push\n");
    let ctx = ToolContext::new(dir.path());
    (dir, ctx)
}

#[tokio::test]
async fn glob_matches_paths_and_respects_gitignore() {
    let (_dir, ctx) = project();
    let out = GlobTool.run(json!({"pattern": "**/*.rs"}), &ctx).await;
    assert!(!out.is_error);
    assert_eq!(out.content.trim(), "src/main.rs");
}

#[tokio::test]
async fn glob_includes_hidden_directories() {
    let (_dir, ctx) = project();
    let out = GlobTool.run(json!({"pattern": "**/*.yml"}), &ctx).await;
    assert_eq!(out.content.trim(), ".github/ci.yml");
}

#[tokio::test]
async fn glob_reports_when_nothing_matches() {
    let (_dir, ctx) = project();
    let out = GlobTool.run(json!({"pattern": "**/*.py"}), &ctx).await;
    assert!(!out.is_error);
    assert!(out.content.contains("No files matched"));
}

#[tokio::test]
async fn grep_returns_path_line_and_text_and_skips_ignored_files() {
    let (_dir, ctx) = project();
    let out = GrepTool
        .run(json!({"pattern": "fn main\\(\\)"}), &ctx)
        .await;
    assert!(!out.is_error);
    assert_eq!(out.content.trim(), "src/main.rs:2:fn main() {}");
}

#[tokio::test]
async fn grep_filters_files_by_glob() {
    let (_dir, ctx) = project();
    let out = GrepTool
        .run(json!({"pattern": "fn main", "glob": "*.txt"}), &ctx)
        .await;
    assert_eq!(
        out.content.trim(),
        "src/notes.txt:1:fn main is mentioned here"
    );
}

#[tokio::test]
async fn grep_caps_results() {
    let dir = tempfile::tempdir().unwrap();
    put(dir.path(), "big.txt", &"match me\n".repeat(300));
    let ctx = ToolContext::new(dir.path());
    let out = GrepTool.run(json!({"pattern": "match"}), &ctx).await;
    assert!(
        out.content.contains("100 more matches not shown"),
        "{}",
        out.content
    );
}

#[tokio::test]
async fn grep_rejects_an_invalid_regex() {
    let (_dir, ctx) = project();
    let out = GrepTool.run(json!({"pattern": "("}), &ctx).await;
    assert!(out.is_error);
    assert!(out.content.contains("invalid regex"));
}

#[tokio::test]
async fn glob_reports_a_nonexistent_path_instead_of_no_matches() {
    let (_dir, ctx) = project();
    let out = GlobTool
        .run(json!({"pattern": "*.rs", "path": "nope"}), &ctx)
        .await;
    assert!(out.is_error);
    assert!(
        out.content.contains("no such file or directory"),
        "{}",
        out.content
    );
}

#[tokio::test]
async fn grep_reports_a_nonexistent_path_instead_of_no_matches() {
    let (_dir, ctx) = project();
    let out = GrepTool
        .run(json!({"pattern": "fn main", "path": "nope"}), &ctx)
        .await;
    assert!(out.is_error);
    assert!(
        out.content.contains("no such file or directory"),
        "{}",
        out.content
    );
}

#[tokio::test]
async fn glob_with_a_path_reports_workspace_relative_paths() {
    let (_dir, ctx) = project();
    let out = GlobTool
        .run(json!({"pattern": "*.rs", "path": "src"}), &ctx)
        .await;
    assert!(!out.is_error);
    assert_eq!(out.content.trim(), "src/main.rs");
}
