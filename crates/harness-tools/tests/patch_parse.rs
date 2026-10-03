//! The V4A patch format: what parses, what is rejected, and how hunks find their place.

use harness_tools::patch::{FileOp, LineKind, apply_hunks, parse};

fn patch(body: &str) -> String {
    format!("*** Begin Patch\n{body}*** End Patch\n")
}

fn ops(body: &str) -> Vec<FileOp> {
    parse(&patch(body)).unwrap()
}

fn error(text: &str) -> String {
    parse(text).unwrap_err().to_string()
}

#[test]
fn an_add_file_hunk_gives_the_new_files_content() {
    assert_eq!(
        ops("*** Add File: src/b.rs\n+fn b() {}\n+\n+// end\n"),
        [FileOp::Add {
            path: "src/b.rs".into(),
            content: "fn b() {}\n\n// end\n".into()
        }]
    );
}

#[test]
fn a_delete_file_hunk_names_the_file() {
    assert_eq!(
        ops("*** Delete File: old.txt\n"),
        [FileOp::Delete {
            path: "old.txt".into()
        }]
    );
}

#[test]
fn an_update_hunk_keeps_context_removals_and_additions_in_order() {
    let parsed = ops(
        "*** Update File: a.rs\n@@ fn main()\n let a = 1;\n-let b = 2;\n+let b = 3;\n+let c = 4;\n",
    );
    let [
        FileOp::Update {
            path,
            move_to,
            hunks,
        },
    ] = &parsed[..]
    else {
        panic!("{parsed:?}")
    };
    assert_eq!((path.as_str(), move_to), ("a.rs", &None));
    assert_eq!(hunks.len(), 1);
    assert_eq!(hunks[0].header.as_deref(), Some("fn main()"));
    let kinds: Vec<_> = hunks[0]
        .lines
        .iter()
        .map(|(k, t)| (*k, t.as_str()))
        .collect();
    assert_eq!(
        kinds,
        [
            (LineKind::Context, "let a = 1;"),
            (LineKind::Remove, "let b = 2;"),
            (LineKind::Add, "let b = 3;"),
            (LineKind::Add, "let c = 4;"),
        ]
    );
}

#[test]
fn several_hunks_and_several_files_parse_in_order() {
    let parsed = ops(
        "*** Update File: a.rs\n@@\n-x\n+y\n@@\n-p\n+q\n*** Add File: b.rs\n+z\n*** Update File: c.rs\n*** Move to: d.rs\n@@\n-1\n+2\n",
    );
    assert_eq!(parsed.len(), 3);
    let FileOp::Update { hunks, .. } = &parsed[0] else {
        panic!()
    };
    assert_eq!(hunks.len(), 2);
    let FileOp::Update { move_to, .. } = &parsed[2] else {
        panic!()
    };
    assert_eq!(move_to.as_deref(), Some("d.rs"));
}

#[test]
fn a_hunk_may_start_without_the_at_marker_and_a_blank_line_is_blank_context() {
    let parsed = ops("*** Update File: a.rs\n a\n\n-b\n+c\n");
    let FileOp::Update { hunks, .. } = &parsed[0] else {
        panic!()
    };
    assert_eq!(hunks[0].lines[1], (LineKind::Context, String::new()));
}

#[test]
fn end_of_file_marks_the_hunk() {
    let parsed = ops("*** Update File: a.rs\n@@\n-last\n+final\n*** End of File\n");
    let FileOp::Update { hunks, .. } = &parsed[0] else {
        panic!()
    };
    assert!(hunks[0].at_eof);
}

#[test]
fn crlf_in_the_patch_is_accepted() {
    let text = "*** Begin Patch\r\n*** Add File: a\r\n+x\r\n*** End Patch\r\n";
    assert_eq!(
        parse(text).unwrap(),
        [FileOp::Add {
            path: "a".into(),
            content: "x\n".into()
        }]
    );
}

// Spec: a malformed patch is rejected, naming the file and the hunk.
#[test]
fn malformed_patches_are_rejected_with_the_file_and_hunk() {
    assert!(error("*** Update File: a\n").contains("*** Begin Patch"));
    assert!(error("*** Begin Patch\n*** Add File: a\n+x\n").contains("*** End Patch"));
    let e = error(&patch("*** Update File: src/a.rs\n@@\n-x\nplain text\n"));
    assert!(e.contains("src/a.rs") && e.contains("hunk 1"), "{e}");
    let e = error(&patch("*** Add File: n.txt\nnot a plus line\n"));
    assert!(e.contains("n.txt"), "{e}");
    let e = error(&patch("*** Update File: u.rs\n"));
    assert!(e.contains("u.rs") && e.contains("no hunks"), "{e}");
    let e = error(&patch("*** Frobnicate File: x\n"));
    assert!(e.contains("Frobnicate"), "{e}");
    let e = error(&patch("*** Delete File: d\n+surprise\n"));
    assert!(e.contains("d"), "{e}");
    let e = error(&patch("*** Add File: a\n+1\n*** Delete File: a\n"));
    assert!(e.contains("twice") && e.contains('a'), "{e}");
    let e = error(&patch("*** Add File: \n+1\n"));
    assert!(e.contains("path"), "{e}");
}

#[test]
fn an_empty_patch_is_rejected() {
    assert!(error("*** Begin Patch\n*** End Patch\n").contains("no changes"));
    assert!(error("").contains("*** Begin Patch"));
}

fn update(body: &str) -> Vec<harness_tools::patch::Hunk> {
    let FileOp::Update { hunks, .. } = ops(&format!("*** Update File: f\n{body}")).remove(0) else {
        panic!()
    };
    hunks
}

fn applied(original: &str, body: &str) -> Result<String, String> {
    apply_hunks("f", original, &update(body)).map_err(|e| e.to_string())
}

#[test]
fn a_hunk_replaces_its_removed_lines_where_its_context_matches() {
    assert_eq!(
        applied("a\nb\nc\nd\n", "@@\n b\n-c\n+C\n+C2\n d\n").unwrap(),
        "a\nb\nC\nC2\nd\n"
    );
}

#[test]
fn hunks_apply_in_file_order_from_where_the_last_one_ended() {
    // `x` occurs twice; the second hunk's `x` is the one after the first hunk.
    assert_eq!(
        applied("x\nmid\nx\n", "@@\n-x\n+X1\n@@\n-x\n+X2\n").unwrap(),
        "X1\nmid\nX2\n"
    );
}

#[test]
fn a_header_line_positions_the_hunk() {
    let original = "fn a() {\n    x\n}\nfn b() {\n    x\n}\n";
    assert_eq!(
        applied(original, "@@ fn b() {\n-    x\n+    y\n").unwrap(),
        "fn a() {\n    x\n}\nfn b() {\n    y\n}\n"
    );
}

#[test]
fn trailing_whitespace_and_indentation_differences_in_the_patch_still_match_and_context_keeps_the_files_text()
 {
    // The patch's context drops the file's trailing space; the file's line stays as it was.
    assert_eq!(
        applied("keep \nold\n", "@@\n keep\n-old\n+new\n").unwrap(),
        "keep \nnew\n"
    );
    // The model lost the indentation of a removed line.
    assert_eq!(
        applied("fn f() {\n    old();\n}\n", "@@\n-old();\n+new();\n").unwrap(),
        "fn f() {\n    new();\n}\n"
    );
}

#[test]
fn a_file_without_a_final_newline_keeps_none() {
    assert_eq!(applied("a\nb", "@@\n-b\n+B\n").unwrap(), "a\nB");
    assert_eq!(applied("a\nb\n", "@@\n-b\n+B\n").unwrap(), "a\nB\n");
}

#[test]
fn additions_only_are_inserted_after_the_header_or_at_the_end() {
    assert_eq!(
        applied("fn a() {\n}\n", "@@ fn a() {\n+    // hi\n").unwrap(),
        "fn a() {\n    // hi\n}\n"
    );
    assert_eq!(applied("a\n", "@@\n+tail\n").unwrap(), "a\ntail\n");
}

#[test]
fn an_end_of_file_hunk_matches_at_the_end() {
    assert_eq!(
        applied("x\nend\nx\nend\n", "@@\n-end\n+END\n*** End of File\n").unwrap(),
        "x\nend\nx\nEND\n"
    );
}

// Spec "Context does not match": the error names the file and the hunk.
#[test]
fn context_that_does_not_occur_is_an_error_naming_the_file_and_the_hunk() {
    let e = applied("a\nb\n", "@@\n-a\n+A\n@@\n-zzz\n+Z\n").unwrap_err();
    assert!(e.contains('f') && e.contains("hunk 2"), "{e}");
    assert!(e.contains("zzz"), "{e}");
}

#[test]
fn a_header_that_does_not_occur_is_an_error() {
    let e = applied("a\n", "@@ fn nope()\n-a\n+b\n").unwrap_err();
    assert!(e.contains("hunk 1") && e.contains("fn nope()"), "{e}");
}

#[test]
fn removing_lines_to_nothing_and_an_empty_result_work() {
    assert_eq!(applied("a\n", "@@\n-a\n").unwrap(), "");
}

// Review Focus: a file with Windows line endings keeps them.
#[test]
fn a_crlf_file_keeps_its_line_endings() {
    assert_eq!(
        applied("a\r\nb\r\nc\r\n", "@@\n a\n-b\n+B\n+B2\n c\n").unwrap(),
        "a\r\nB\r\nB2\r\nc\r\n"
    );
    // Without a final line ending too.
    assert_eq!(applied("a\r\nb", "@@\n-b\n+B\n").unwrap(), "a\r\nB");
}

// Lines the patch did not touch keep the ending they had, mixed or not; an added line ends like
// the line before it (the first line of a file, like the file's if it is uniform).
#[test]
fn a_file_of_mixed_line_endings_keeps_the_endings_of_the_lines_it_does_not_change() {
    assert_eq!(
        applied("a\r\nb\nc\r\nd\n", "@@\n-b\n+B\n").unwrap(),
        "a\r\nB\r\nc\r\nd\n"
    );
    assert_eq!(
        applied("a\r\nb\nc\r\nd\n", "@@\n c\n-d\n+D\n").unwrap(),
        "a\r\nb\nc\r\nD\r\n"
    );
    // Nothing else is rewritten.
    assert_eq!(
        applied("a\nb\r\nc\n", "@@\n-a\n+A\n").unwrap(),
        "A\nb\r\nc\n"
    );
}

// A match below the exact level is said so, for the model to check.
#[test]
fn a_hunk_placed_by_ignoring_whitespace_is_noted() {
    let hunks = update("@@\n-  b\n+  B\n");
    let (text, notes) = harness_tools::patch::apply_hunks_noted("f", "a\n    b\n", &hunks).unwrap();
    assert_eq!(text, "a\n    B\n");
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(
        notes[0].contains("hunk 1") && notes[0].contains("indentation"),
        "{notes:?}"
    );
    let exact = harness_tools::patch::apply_hunks_noted("f", "a\n  b\n", &hunks).unwrap();
    assert!(exact.1.is_empty(), "{:?}", exact.1);
}

// A blank line between two file sections is the model's spacing, not a context line.
#[test]
fn blank_lines_before_the_next_file_are_not_part_of_the_section() {
    let ops = parse(
        "*** Begin Patch\n*** Update File: a.rs\n@@\n-x\n+y\n\n*** Add File: b.rs\n+z\n\n*** Delete File: c.rs\n*** End Patch\n",
    )
    .unwrap();
    let FileOp::Update { hunks, .. } = &ops[0] else {
        panic!("{ops:?}")
    };
    assert_eq!(hunks[0].lines.len(), 2, "{hunks:?}");
    assert_eq!(ops.len(), 3);
}
