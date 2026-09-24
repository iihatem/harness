use std::path::Path;

use harness_core::tool::{Tool, ToolContext, ToolOutput};
use harness_tools::{EditTool, ReadTool, WriteTool};
use serde_json::{Value, json};

async fn call(tool: &dyn Tool, ctx: &ToolContext, args: Value) -> ToolOutput {
    tool.run(args, ctx).await
}

fn setup() -> (tempfile::TempDir, ToolContext) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ToolContext::new(dir.path());
    (dir, ctx)
}

fn put(dir: &Path, name: &str, content: &str) {
    std::fs::write(dir.join(name), content).unwrap();
}

#[tokio::test]
async fn read_returns_numbered_lines_with_offset_and_limit() {
    let (dir, ctx) = setup();
    let text: String = (1..=200).map(|i| format!("line {i}\n")).collect();
    put(dir.path(), "a.txt", &text);
    let out = call(
        &ReadTool,
        &ctx,
        json!({"path": "a.txt", "offset": 100, "limit": 50}),
    )
    .await;
    assert!(!out.is_error);
    assert!(
        out.content.starts_with("   100\tline 100\n"),
        "{}",
        out.content
    );
    assert!(out.content.contains("   149\tline 149\n"));
    assert!(!out.content.contains("line 150\n"));
    assert!(out.content.contains("offset=150"));
}

#[tokio::test]
async fn read_refuses_binary_files() {
    let (dir, ctx) = setup();
    std::fs::write(
        dir.path().join("img.png"),
        [0x89, b'P', b'N', b'G', 0, 0, 1],
    )
    .unwrap();
    let out = call(&ReadTool, &ctx, json!({"path": "img.png"})).await;
    assert!(out.is_error);
    assert!(out.content.contains("binary"));
}

// Review Focus: huge single-line files.
#[tokio::test]
async fn read_caps_very_long_lines() {
    let (dir, ctx) = setup();
    put(dir.path(), "min.js", &"x".repeat(10_000));
    let out = call(&ReadTool, &ctx, json!({"path": "min.js"})).await;
    assert!(!out.is_error);
    assert!(out.content.contains("[line truncated]"));
    assert!(out.content.len() < 2_200, "{}", out.content.len());
}

#[tokio::test]
async fn read_reports_a_missing_file() {
    let (_dir, ctx) = setup();
    let out = call(&ReadTool, &ctx, json!({"path": "nope.txt"})).await;
    assert!(out.is_error);
    assert!(out.content.contains("nope.txt"));
}

#[tokio::test]
async fn write_creates_new_files_and_parent_directories() {
    let (dir, ctx) = setup();
    let out = call(
        &WriteTool,
        &ctx,
        json!({"path": "src/deep/new.rs", "content": "fn main() {}\n"}),
    )
    .await;
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("src/deep/new.rs")).unwrap(),
        "fn main() {}\n"
    );
}

#[tokio::test]
async fn write_refuses_to_overwrite_an_unread_file() {
    let (dir, ctx) = setup();
    put(dir.path(), "a.txt", "original");
    let out = call(&WriteTool, &ctx, json!({"path": "a.txt", "content": "new"})).await;
    assert!(out.is_error);
    assert!(out.content.contains("read it first"));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "original"
    );
}

#[tokio::test]
async fn write_refuses_a_file_changed_since_it_was_read() {
    let (dir, ctx) = setup();
    put(dir.path(), "a.txt", "v1");
    call(&ReadTool, &ctx, json!({"path": "a.txt"})).await;
    put(dir.path(), "a.txt", "edited in an editor");
    let out = call(&WriteTool, &ctx, json!({"path": "a.txt", "content": "v2"})).await;
    assert!(out.is_error);
    assert!(out.content.contains("changed on disk"));
}

#[tokio::test]
async fn write_after_read_succeeds_and_allows_a_second_write() {
    let (dir, ctx) = setup();
    put(dir.path(), "a.txt", "v1");
    call(&ReadTool, &ctx, json!({"path": "a.txt"})).await;
    assert!(
        !call(&WriteTool, &ctx, json!({"path": "a.txt", "content": "v2"}))
            .await
            .is_error
    );
    assert!(
        !call(&WriteTool, &ctx, json!({"path": "a.txt", "content": "v3"}))
            .await
            .is_error
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "v3"
    );
}

#[tokio::test]
async fn edit_replaces_a_unique_match_and_returns_a_diff() {
    let (dir, ctx) = setup();
    put(dir.path(), "a.rs", "let x = old();\nlet y = 2;\n");
    call(&ReadTool, &ctx, json!({"path": "a.rs"})).await;
    let out = call(
        &EditTool,
        &ctx,
        json!({"path": "a.rs", "old_string": "old()", "new_string": "new()"}),
    )
    .await;
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("-let x = old();"));
    assert!(out.content.contains("+let x = new();"));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.rs")).unwrap(),
        "let x = new();\nlet y = 2;\n"
    );
}

#[tokio::test]
async fn edit_rejects_ambiguous_and_missing_matches() {
    let (dir, ctx) = setup();
    put(dir.path(), "a.txt", "x\nx\nx\n");
    call(&ReadTool, &ctx, json!({"path": "a.txt"})).await;
    let ambiguous = call(
        &EditTool,
        &ctx,
        json!({"path": "a.txt", "old_string": "x", "new_string": "y"}),
    )
    .await;
    assert!(ambiguous.is_error);
    assert!(
        ambiguous.content.contains("matches 3 times"),
        "{}",
        ambiguous.content
    );
    let missing = call(
        &EditTool,
        &ctx,
        json!({"path": "a.txt", "old_string": "zzz", "new_string": "y"}),
    )
    .await;
    assert!(missing.is_error);
    assert!(missing.content.contains("0 matches"));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "x\nx\nx\n"
    );
}

#[tokio::test]
async fn edit_replace_all_changes_every_match() {
    let (dir, ctx) = setup();
    put(dir.path(), "a.txt", "x\nx\n");
    call(&ReadTool, &ctx, json!({"path": "a.txt"})).await;
    let out = call(
        &EditTool,
        &ctx,
        json!({"path": "a.txt", "old_string": "x", "new_string": "y", "replace_all": true}),
    )
    .await;
    assert!(!out.is_error);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "y\ny\n"
    );
}

#[tokio::test]
async fn edit_requires_a_prior_read() {
    let (dir, ctx) = setup();
    put(dir.path(), "a.txt", "x");
    let out = call(
        &EditTool,
        &ctx,
        json!({"path": "a.txt", "old_string": "x", "new_string": "y"}),
    )
    .await;
    assert!(out.is_error);
    assert!(out.content.contains("read it first"));
}

#[tokio::test]
async fn read_reports_an_empty_file() {
    let (dir, ctx) = setup();
    put(dir.path(), "empty.txt", "");
    let out = call(&ReadTool, &ctx, json!({"path": "empty.txt"})).await;
    assert!(!out.is_error);
    assert!(out.content.contains("[empty file]"));
}

#[tokio::test]
async fn read_reports_an_offset_past_the_end() {
    let (dir, ctx) = setup();
    put(dir.path(), "short.txt", "line 1\nline 2\nline 3\n");
    let out = call(&ReadTool, &ctx, json!({"path": "short.txt", "offset": 10})).await;
    assert!(!out.is_error);
    assert!(out.content.contains("past the end"));
    assert!(out.content.contains("3 lines"));
}

#[tokio::test]
async fn read_flags_non_utf8_content() {
    let (dir, ctx) = setup();
    std::fs::write(dir.path().join("latin1.txt"), b"caf\xe9\n").unwrap();
    let out = call(&ReadTool, &ctx, json!({"path": "latin1.txt"})).await;
    assert!(!out.is_error);
    assert!(out.content.contains("not valid UTF-8"));
    assert!(out.content.contains("U+FFFD"));
}

#[tokio::test]
async fn read_of_valid_utf8_has_no_encoding_note() {
    let (dir, ctx) = setup();
    put(dir.path(), "utf8.txt", "héllo\n");
    let out = call(&ReadTool, &ctx, json!({"path": "utf8.txt"})).await;
    assert!(!out.is_error);
    assert!(!out.content.contains("not valid UTF-8"));
}
