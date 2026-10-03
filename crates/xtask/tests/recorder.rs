//! Recording a model's edit as tool calls in each format, and replaying them.

use harness_core::edit_format::EditFormat;
use xtask::{
    record::{Call, calls},
    replay::apply,
    task::Tree,
};

fn tree(files: &[(&str, &str)]) -> Tree {
    files
        .iter()
        .map(|(p, t)| (p.to_string(), t.to_string()))
        .collect()
}

/// What recording `before` to `after` in `format` and replaying it gives.
fn round_trip(format: EditFormat, before: &Tree, after: &Tree) -> Tree {
    let recorded = calls(format, before, after);
    apply(format, before, &recorded).unwrap_or_else(|e| panic!("{format}: {e}\n{recorded:#?}"))
}

fn check(before: &[(&str, &str)], after: &[(&str, &str)]) {
    let (before, after) = (tree(before), tree(after));
    for format in EditFormat::ALL {
        assert_eq!(round_trip(format, &before, &after), after, "{format}");
    }
}

const NINE: &str = "1\n2\n3\n4\n5\n6\n7\n8\n9\n";

#[test]
fn a_change_in_the_middle() {
    check(
        &[("f.txt", NINE)],
        &[("f.txt", "1\n2\n3\n4\nFIVE\n6\n7\n8\n9\n")],
    );
}

#[test]
fn a_change_of_the_first_and_of_the_last_line() {
    check(
        &[("f.txt", NINE)],
        &[("f.txt", "ONE\n2\n3\n4\n5\n6\n7\n8\n9\n")],
    );
    check(
        &[("f.txt", NINE)],
        &[("f.txt", "1\n2\n3\n4\n5\n6\n7\n8\nNINE\n")],
    );
}

#[test]
fn an_insertion_at_the_start_the_middle_and_the_end() {
    check(
        &[("f.txt", NINE)],
        &[("f.txt", "0\n1\n2\n3\n4\n5\n6\n7\n8\n9\n")],
    );
    check(
        &[("f.txt", NINE)],
        &[("f.txt", "1\n2\n3\n4\nnew\n5\n6\n7\n8\n9\n")],
    );
    check(
        &[("f.txt", NINE)],
        &[("f.txt", "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n")],
    );
}

#[test]
fn a_deletion_at_the_start_the_middle_and_the_end() {
    check(&[("f.txt", NINE)], &[("f.txt", "2\n3\n4\n5\n6\n7\n8\n9\n")]);
    check(&[("f.txt", NINE)], &[("f.txt", "1\n2\n3\n4\n7\n8\n9\n")]);
    check(&[("f.txt", NINE)], &[("f.txt", "1\n2\n3\n4\n5\n6\n7\n8\n")]);
}

#[test]
fn a_small_file_changed_entirely() {
    check(&[("f.txt", "a\n")], &[("f.txt", "b\nc\n")]);
}

// The line that changes occurs more than once: the context decides which.
#[test]
fn a_change_to_the_second_of_two_identical_blocks() {
    let before = "fn a() {\n    x\n}\nfn b() {\n    x\n}\n";
    let after = "fn a() {\n    x\n}\nfn b() {\n    y\n}\n";
    check(&[("f.rs", before)], &[("f.rs", after)]);
}

#[test]
fn a_new_file_and_a_changed_one_and_a_second_changed_one() {
    check(
        &[
            ("a.txt", "one\ntwo\n"),
            ("b.txt", "x\ny\n"),
            ("keep.txt", "same\n"),
        ],
        &[
            ("a.txt", "one\nTWO\n"),
            ("b.txt", "x\ny\nz\n"),
            ("keep.txt", "same\n"),
            ("src/new.txt", "fresh\n"),
        ],
    );
}

#[test]
fn files_the_change_leaves_alone_are_not_touched() {
    let before = tree(&[("a.txt", "1\n"), ("b.txt", "2\n")]);
    let after = tree(&[("a.txt", "1\n"), ("b.txt", "3\n")]);
    for format in EditFormat::ALL {
        let recorded = calls(format, &before, &after);
        let text = serde_json::to_string(&recorded).unwrap();
        assert!(!text.contains("a.txt"), "{format}: {text}");
    }
}

// The guards: a file is read before it is changed, in every format.
#[test]
fn every_update_is_preceded_by_a_read_of_the_file() {
    let before = tree(&[("a.txt", "1\n2\n3\n")]);
    let after = tree(&[("a.txt", "1\nTWO\n3\n")]);
    for format in EditFormat::ALL {
        let recorded = calls(format, &before, &after);
        assert_eq!(recorded[0].name, "read", "{format}");
        assert_eq!(recorded[0].arguments["path"], "a.txt", "{format}");
        assert!(recorded.len() >= 2, "{format}");
    }
}

#[test]
fn each_format_uses_its_own_tool() {
    let before = tree(&[("a.txt", "1\n2\n3\n")]);
    let after = tree(&[("a.txt", "1\nTWO\n3\n"), ("n.txt", "new\n")]);
    let names = |format| -> Vec<String> {
        calls(format, &before, &after)
            .into_iter()
            .map(|c| c.name)
            .collect()
    };
    assert_eq!(names(EditFormat::StrReplace), ["read", "edit", "write"]);
    assert_eq!(names(EditFormat::ApplyPatch), ["read", "apply_patch"]);
    assert_eq!(names(EditFormat::WholeFile), ["read", "write", "write"]);
    assert_eq!(
        names(EditFormat::Hashline),
        ["read", "hashline_edit", "write"]
    );
}

#[test]
fn an_apply_patch_recording_is_one_patch_for_all_the_files() {
    let before = tree(&[("a.txt", "1\n"), ("b.txt", "2\n")]);
    let after = tree(&[("a.txt", "x\n"), ("b.txt", "y\n"), ("c.txt", "z\n")]);
    let recorded = calls(EditFormat::ApplyPatch, &before, &after);
    let patches: Vec<&Call> = recorded
        .iter()
        .filter(|c| c.name == "apply_patch")
        .collect();
    assert_eq!(patches.len(), 1);
    let input = patches[0].arguments["input"].as_str().unwrap();
    assert!(input.starts_with("*** Begin Patch\n") && input.trim_end().ends_with("*** End Patch"));
    assert_eq!(input.matches("*** Update File:").count(), 2);
    assert_eq!(input.matches("*** Add File:").count(), 1);
}

#[test]
fn recording_is_deterministic() {
    let before = tree(&[("a.txt", NINE)]);
    let after = tree(&[("a.txt", "1\n2\nx\n4\n5\n6\n7\n8\n9\n")]);
    for format in EditFormat::ALL {
        assert_eq!(
            calls(format, &before, &after),
            calls(format, &before, &after)
        );
    }
}

// A replay that fails says which call, and why.
#[test]
fn a_call_that_the_tool_refuses_fails_the_replay_naming_it() {
    let before = tree(&[("a.txt", "1\n2\n")]);
    let bad = vec![
        Call {
            name: "read".into(),
            arguments: serde_json::json!({"path": "a.txt"}),
        },
        Call {
            name: "edit".into(),
            arguments: serde_json::json!({"path": "a.txt", "old_string": "nope", "new_string": "x"}),
        },
    ];
    let error = apply(EditFormat::StrReplace, &before, &bad).unwrap_err();
    assert!(
        error.contains("call 2") && error.contains("edit"),
        "{error}"
    );
}

#[test]
fn a_call_to_a_tool_the_format_does_not_offer_fails_the_replay() {
    let before = tree(&[("a.txt", "1\n")]);
    let calls = vec![Call {
        name: "apply_patch".into(),
        arguments: serde_json::json!({"input": "x"}),
    }];
    let error = apply(EditFormat::StrReplace, &before, &calls).unwrap_err();
    assert!(
        error.contains("apply_patch") && error.contains("not offered"),
        "{error}"
    );
}

// An empty file has no line to address: recording an edit of it must not panic in any format.
#[test]
fn an_empty_file_gets_content() {
    check(&[("f.txt", "")], &[("f.txt", "new\n")]);
}
