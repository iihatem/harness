//! `whole_file`: complete files, limited to small ones, with `write`'s overwrite guards.

use harness_core::tool::{Tool, ToolContext, ToolOutput};
use harness_tools::{WholeFileTool, whole_file::MAX_LINES};
use serde_json::json;

fn setup() -> (tempfile::TempDir, ToolContext) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ToolContext::new(dir.path());
    (dir, ctx)
}

fn lines(n: usize) -> String {
    (1..=n).map(|i| format!("line {i}\n")).collect()
}

async fn write(ctx: &ToolContext, path: &str, content: &str) -> ToolOutput {
    WholeFileTool
        .run(json!({"path": path, "content": content}), ctx)
        .await
}

fn mark_read(ctx: &ToolContext, name: &str) {
    let path = ctx.workspace.join(name);
    ctx.tracker.record(&path, &std::fs::read(&path).unwrap());
}

// Spec "Small file".
#[tokio::test]
async fn a_120_line_file_is_written() {
    let (dir, ctx) = setup();
    let out = write(&ctx, "a.txt", &lines(120)).await;
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        lines(120)
    );
}

// Spec "Large file": an existing 500-line file is refused, suggesting `str_replace`.
#[tokio::test]
async fn an_existing_500_line_file_is_refused_and_unchanged() {
    let (dir, ctx) = setup();
    std::fs::write(dir.path().join("big.txt"), lines(500)).unwrap();
    mark_read(&ctx, "big.txt");
    let out = write(&ctx, "big.txt", "short\n").await;
    assert!(out.is_error);
    assert!(out.content.contains("str_replace"), "{}", out.content);
    assert!(out.content.contains("500 lines"), "{}", out.content);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("big.txt")).unwrap(),
        lines(500)
    );
}

#[tokio::test]
async fn content_of_400_lines_or_more_is_refused_but_399_is_not() {
    let (dir, ctx) = setup();
    assert_eq!(MAX_LINES, 400);
    let out = write(&ctx, "ok.txt", &lines(399)).await;
    assert!(!out.is_error, "{}", out.content);
    let out = write(&ctx, "big.txt", &lines(400)).await;
    assert!(
        out.is_error && out.content.contains("str_replace"),
        "{}",
        out.content
    );
    assert!(!dir.path().join("big.txt").exists());
}

// The overwrite guards of `write`.
#[tokio::test]
async fn overwriting_a_file_that_was_not_read_or_changed_since_is_refused() {
    let (dir, ctx) = setup();
    std::fs::write(dir.path().join("a.txt"), "old\n").unwrap();
    let out = write(&ctx, "a.txt", "new\n").await;
    assert!(
        out.is_error && out.content.contains("has not been read"),
        "{}",
        out.content
    );
    mark_read(&ctx, "a.txt");
    std::fs::write(dir.path().join("a.txt"), "changed\n").unwrap();
    let out = write(&ctx, "a.txt", "new\n").await;
    assert!(
        out.is_error && out.content.contains("changed on disk"),
        "{}",
        out.content
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "changed\n"
    );
}

#[tokio::test]
async fn a_read_file_is_overwritten_and_directories_are_made() {
    let (dir, ctx) = setup();
    std::fs::write(dir.path().join("a.txt"), "old\n").unwrap();
    mark_read(&ctx, "a.txt");
    assert!(!write(&ctx, "a.txt", "new\n").await.is_error);
    assert!(!write(&ctx, "x/y/z.txt", "deep\n").await.is_error);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("x/y/z.txt")).unwrap(),
        "deep\n"
    );
}

#[test]
fn it_is_the_write_tool_of_the_format_and_says_what_it_takes() {
    let spec = WholeFileTool.spec();
    assert_eq!(spec.name, "write");
    assert!(spec.description.contains("400"), "{}", spec.description);
    let args = json!({"path": "a.txt", "content": "x"});
    let ctx = ToolContext::new(std::path::Path::new("/tmp"));
    assert_eq!(
        WholeFileTool.changed_paths(&args, &ctx),
        [ctx.resolve("a.txt")]
    );
    assert!(matches!(
        WholeFileTool.action(&args, &ctx),
        harness_core::permission::Action::Write(_)
    ));
}
