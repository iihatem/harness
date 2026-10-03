//! `hashline`: `read` prefixes lines with a short hash, and edits address lines by it.

use harness_core::tool::{Tool, ToolContext, ToolOutput};
use harness_tools::{
    HashlineEditTool, HashlineReadTool, ReadTool,
    hashline::{address, parse_address},
};
use serde_json::json;

fn setup() -> (tempfile::TempDir, ToolContext) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ToolContext::new(dir.path());
    (dir, ctx)
}

fn put(ctx: &ToolContext, name: &str, text: &str) {
    std::fs::write(ctx.workspace.join(name), text).unwrap();
}

fn text(ctx: &ToolContext, name: &str) -> String {
    std::fs::read_to_string(ctx.workspace.join(name)).unwrap()
}

async fn read(ctx: &ToolContext, name: &str) -> ToolOutput {
    HashlineReadTool.run(json!({"path": name}), ctx).await
}

async fn edit(ctx: &ToolContext, args: serde_json::Value) -> ToolOutput {
    HashlineEditTool.run(args, ctx).await
}

/// The address of line `n` (1-based) of `lines`.
fn at(lines: &[&str], n: usize) -> String {
    address(n, lines[n - 1])
}

#[test]
fn an_address_is_the_line_number_and_three_hex_digits_of_its_content() {
    let a = address(12, "let x = 1;");
    let (line, hash) = a.split_once('#').unwrap();
    assert_eq!(line, "12");
    assert_eq!(hash.len(), 3);
    assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    // The same line has the same hash wherever it is; another line has another (nearly always).
    assert_eq!(address(3, "let x = 1;")[2..], a[3..]);
    assert_ne!(address(12, "let x = 2;"), a);
    assert_eq!(parse_address(&a), Some((12, hash.to_string())));
    assert_eq!(parse_address("12"), None);
    assert_eq!(parse_address("x#abc"), None);
    assert_eq!(parse_address("0#abc"), None);
}

// Spec "Not selected by default": `read` has no hash prefixes.
#[tokio::test]
async fn the_plain_read_has_no_hash_prefixes_and_the_hashline_read_has() {
    let (_dir, ctx) = setup();
    put(&ctx, "a.txt", "alpha\nbeta\n");
    let plain = ReadTool.run(json!({"path": "a.txt"}), &ctx).await;
    assert_eq!(plain.content, "     1\talpha\n     2\tbeta\n");
    let hashed = read(&ctx, "a.txt").await;
    assert_eq!(
        hashed.content,
        format!(
            "{:>10}\talpha\n{:>10}\tbeta\n",
            address(1, "alpha"),
            address(2, "beta")
        )
    );
}

#[tokio::test]
async fn the_hashline_read_pages_like_the_plain_one_and_records_the_read() {
    let (_dir, ctx) = setup();
    let body: String = (1..=300).map(|i| format!("l{i}\n")).collect();
    put(&ctx, "a.txt", &body);
    let out = HashlineReadTool
        .run(json!({"path": "a.txt", "offset": 100, "limit": 2}), &ctx)
        .await;
    assert!(
        out.content.contains(&address(100, "l100")),
        "{}",
        out.content
    );
    assert!(out.content.contains("offset=102"), "{}", out.content);
    assert!(
        ctx.tracker
            .check_fresh(&ctx.workspace.join("a.txt"), body.as_bytes())
            .is_ok()
    );
}

#[tokio::test]
async fn a_range_is_replaced_by_its_addresses() {
    let (_dir, ctx) = setup();
    let lines = ["a", "b", "c", "d"];
    put(&ctx, "f.txt", "a\nb\nc\nd\n");
    read(&ctx, "f.txt").await;
    let out = edit(
        &ctx,
        json!({"path": "f.txt", "start": at(&lines, 2), "end": at(&lines, 3), "new_text": "B\nB2\nB3"}),
    )
    .await;
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(text(&ctx, "f.txt"), "a\nB\nB2\nB3\nd\n");
    // The result shows the new lines with their new addresses.
    assert!(out.content.contains(&address(4, "B3")), "{}", out.content);
}

#[tokio::test]
async fn one_line_when_end_is_left_out_and_nothing_deletes() {
    let (_dir, ctx) = setup();
    let lines = ["a", "b", "c"];
    put(&ctx, "f.txt", "a\nb\nc\n");
    read(&ctx, "f.txt").await;
    let out = edit(
        &ctx,
        json!({"path": "f.txt", "start": at(&lines, 2), "new_text": ""}),
    )
    .await;
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(text(&ctx, "f.txt"), "a\nc\n");
}

#[tokio::test]
async fn the_last_line_keeps_the_files_trailing_newline_state() {
    let (_dir, ctx) = setup();
    put(&ctx, "f.txt", "a\nb");
    read(&ctx, "f.txt").await;
    edit(
        &ctx,
        json!({"path": "f.txt", "start": address(2, "b"), "new_text": "B"}),
    )
    .await;
    assert_eq!(text(&ctx, "f.txt"), "a\nB");
}

// Spec "Stale hash": rejected, and the error includes the line's current content.
#[tokio::test]
async fn a_stale_hash_is_rejected_with_the_current_content() {
    let (_dir, ctx) = setup();
    put(&ctx, "f.txt", "one\ntwo\nthree\n");
    read(&ctx, "f.txt").await;
    let stale = address(2, "two");
    // The file changed after the read.
    put(&ctx, "f.txt", "one\nTWO CHANGED\nthree\n");
    let out = edit(
        &ctx,
        json!({"path": "f.txt", "start": stale, "new_text": "x"}),
    )
    .await;
    assert!(out.is_error);
    assert!(out.content.contains("TWO CHANGED"), "{}", out.content);
    assert!(
        out.content.contains(&address(2, "TWO CHANGED")),
        "{}",
        out.content
    );
    assert_eq!(text(&ctx, "f.txt"), "one\nTWO CHANGED\nthree\n");
}

#[tokio::test]
async fn an_address_past_the_end_or_a_range_backwards_is_rejected() {
    let (_dir, ctx) = setup();
    put(&ctx, "f.txt", "a\nb\n");
    read(&ctx, "f.txt").await;
    let out = edit(
        &ctx,
        json!({"path": "f.txt", "start": address(9, "x"), "new_text": ""}),
    )
    .await;
    assert!(
        out.is_error && out.content.contains("2 lines"),
        "{}",
        out.content
    );
    let out = edit(
        &ctx,
        json!({"path": "f.txt", "start": address(2, "b"), "end": address(1, "a"), "new_text": ""}),
    )
    .await;
    assert!(
        out.is_error && out.content.contains("before"),
        "{}",
        out.content
    );
    let out = edit(
        &ctx,
        json!({"path": "f.txt", "start": "banana", "new_text": ""}),
    )
    .await;
    assert!(
        out.is_error && out.content.contains("12#"),
        "{}",
        out.content
    );
    assert_eq!(text(&ctx, "f.txt"), "a\nb\n");
}

#[tokio::test]
async fn a_file_that_was_never_read_cannot_be_edited() {
    let (_dir, ctx) = setup();
    put(&ctx, "f.txt", "a\n");
    let out = edit(
        &ctx,
        json!({"path": "f.txt", "start": address(1, "a"), "new_text": "b"}),
    )
    .await;
    assert!(
        out.is_error && out.content.contains("has not been read"),
        "{}",
        out.content
    );
    assert_eq!(text(&ctx, "f.txt"), "a\n");
}

#[tokio::test]
async fn a_second_edit_needs_the_new_addresses_but_no_new_read() {
    let (_dir, ctx) = setup();
    put(&ctx, "f.txt", "a\nb\n");
    read(&ctx, "f.txt").await;
    assert!(
        !edit(
            &ctx,
            json!({"path": "f.txt", "start": address(1, "a"), "new_text": "a1\na2"})
        )
        .await
        .is_error
    );
    // Line 2 is now `a2`, and `b` moved to line 3: the old address of `b` is stale.
    let stale = edit(
        &ctx,
        json!({"path": "f.txt", "start": address(2, "b"), "new_text": "x"}),
    )
    .await;
    assert!(stale.is_error);
    assert!(
        !edit(
            &ctx,
            json!({"path": "f.txt", "start": address(3, "b"), "new_text": "B"})
        )
        .await
        .is_error
    );
    assert_eq!(text(&ctx, "f.txt"), "a1\na2\nB\n");
}

#[test]
fn the_edit_tool_is_a_write_and_names_its_file() {
    let ctx = ToolContext::new(std::path::Path::new("/tmp"));
    let args = json!({"path": "f.txt", "start": "1#abc", "new_text": ""});
    assert_eq!(
        HashlineEditTool.changed_paths(&args, &ctx),
        [ctx.resolve("f.txt")]
    );
    assert!(matches!(
        HashlineEditTool.action(&args, &ctx),
        harness_core::permission::Action::Write(_)
    ));
    assert_eq!(HashlineEditTool.spec().name, "hashline_edit");
    assert_eq!(HashlineReadTool.spec().name, "read");
}
