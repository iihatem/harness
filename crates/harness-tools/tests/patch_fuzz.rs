//! The `apply_patch` parser under random input. `cargo-fuzz` needs a nightly toolchain and a
//! separate crate, so this is a property test (proptest) that runs in `cargo test`: whatever the
//! input, the parser and the applier do not panic, and a patch that is refused changes no file.

use harness_core::tool::{Tool, ToolContext};
use harness_tools::{
    ApplyPatchTool,
    patch::{FileOp, apply_hunks, parse},
};
use proptest::prelude::*;
use serde_json::json;

/// Text made of the pieces a patch is made of, in any order and any damage.
fn patchish() -> impl Strategy<Value = String> {
    let piece = prop_oneof![
        Just("*** Begin Patch\n".to_string()),
        Just("*** End Patch\n".to_string()),
        Just("*** Add File: a.txt\n".to_string()),
        Just("*** Update File: a.txt\n".to_string()),
        Just("*** Delete File: a.txt\n".to_string()),
        Just("*** Move to: b.txt\n".to_string()),
        Just("*** End of File\n".to_string()),
        Just("@@\n".to_string()),
        Just("@@ fn x()\n".to_string()),
        "[ +-][a-z ]{0,8}\n",
        "\\PC{0,20}\n",
        "[\\x00-\\x7f]{0,12}",
    ];
    prop::collection::vec(piece, 0..24).prop_map(|pieces| pieces.concat())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    #[test]
    fn the_parser_never_panics_on_any_text(text in "\\PC{0,200}") {
        let _ = parse(&text);
    }

    #[test]
    fn the_parser_and_applier_never_panic_on_patch_like_text(text in patchish(), file in "[a-z\n ]{0,40}") {
        if let Ok(ops) = parse(&text) {
            for op in ops {
                if let FileOp::Update { hunks, .. } = op {
                    let _ = apply_hunks("a.txt", &file, &hunks);
                }
            }
        }
    }

    // A patch built from a change to a file gives that change back.
    #[test]
    fn a_patch_made_from_a_change_applies_to_exactly_that_change(
        lines in prop::collection::vec("[a-z]{1,6}", 1..12),
        start in 0usize..12,
        removed in 0usize..4,
        added in prop::collection::vec("[A-Z]{1,6}", 0..4),
    ) {
        let start = start % lines.len();
        let removed = removed.min(lines.len() - start);
        // Make every line distinct so the context finds one place.
        let lines: Vec<String> = lines.iter().enumerate().map(|(i, l)| format!("{l}{i}")).collect();
        let original: String = lines.iter().map(|l| format!("{l}\n")).collect();
        let mut body = String::from("*** Update File: f\n@@\n");
        for l in &lines[start..start + removed] { body.push_str(&format!("-{l}\n")); }
        for l in &added { body.push_str(&format!("+{l}\n")); }
        // One line of context on each side, where there is one.
        if start > 0 { body = body.replace("@@\n", &format!("@@\n {}\n", lines[start - 1])); }
        if start + removed < lines.len() { body.push_str(&format!(" {}\n", lines[start + removed])); }
        prop_assume!(removed > 0 || !added.is_empty());
        let ops = parse(&format!("*** Begin Patch\n{body}*** End Patch\n")).unwrap();
        let FileOp::Update { hunks, .. } = &ops[0] else { panic!() };
        let result = apply_hunks("f", &original, hunks).unwrap();
        let mut expected: Vec<String> = lines[..start].to_vec();
        expected.extend(added.iter().cloned());
        expected.extend(lines[start + removed..].iter().cloned());
        let expected: String = expected.iter().map(|l| format!("{l}\n")).collect();
        prop_assert_eq!(result, expected);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    // Spec: malformed patches leave files untouched. Whatever the tool is given, a file it was
    // not validly told to change is as it was, and a refused patch changes none.
    #[test]
    fn a_refused_patch_leaves_the_file_untouched(text in patchish()) {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path());
        let path = ctx.workspace.join("a.txt");
        std::fs::write(&path, "one\ntwo\n").unwrap();
        ctx.tracker.record(&path, b"one\ntwo\n");
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let out = runtime.block_on(ApplyPatchTool.run(json!({"input": text}), &ctx));
        if out.is_error {
            prop_assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\ntwo\n");
            let entries: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
            prop_assert_eq!(entries.len(), 1);
        }
    }
}
