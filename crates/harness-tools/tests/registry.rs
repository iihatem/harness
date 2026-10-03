#[test]
fn builtin_tools_are_ordered_stable_and_compact() {
    let names: Vec<String> = harness_tools::builtin()
        .specs()
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert_eq!(names, ["read", "write", "edit", "bash", "grep", "glob"]);

    let first = serde_json::to_string(&harness_tools::builtin().specs()).unwrap();
    let second = serde_json::to_string(&harness_tools::builtin().specs()).unwrap();
    assert_eq!(
        first, second,
        "tool definitions must be byte-identical across requests"
    );
    assert!(
        first.len() / 4 <= 1500,
        "tool definitions are ~{} tokens",
        first.len() / 4
    );
}

mod formats {
    use harness_core::edit_format::EditFormat;
    use harness_tools::builtin_for;

    fn names(format: EditFormat) -> Vec<String> {
        builtin_for(format)
            .specs()
            .into_iter()
            .map(|s| s.name)
            .collect()
    }

    // Spec "Default format": `edit`, taking old_string and new_string.
    #[test]
    fn str_replace_is_the_builtin_set_with_the_edit_tool() {
        assert_eq!(
            names(EditFormat::StrReplace),
            ["read", "write", "edit", "bash", "grep", "glob"]
        );
        assert_eq!(
            serde_json::to_string(&builtin_for(EditFormat::StrReplace).specs()).unwrap(),
            serde_json::to_string(&harness_tools::builtin().specs()).unwrap()
        );
        let edit = builtin_for(EditFormat::StrReplace)
            .get("edit")
            .unwrap()
            .spec();
        assert!(edit.parameters["properties"]["old_string"].is_object());
        assert!(edit.parameters["properties"]["new_string"].is_object());
    }

    // Spec "Profile selects a format": offered `apply_patch`, and not offered `edit`.
    #[test]
    fn apply_patch_replaces_edit_and_write() {
        assert_eq!(
            names(EditFormat::ApplyPatch),
            ["read", "apply_patch", "bash", "grep", "glob"]
        );
    }

    #[test]
    fn whole_file_replaces_write_with_the_limited_one() {
        assert_eq!(
            names(EditFormat::WholeFile),
            ["read", "write", "bash", "grep", "glob"]
        );
        let write = builtin_for(EditFormat::WholeFile)
            .get("write")
            .unwrap()
            .spec();
        assert!(write.description.contains("400"));
    }

    #[test]
    fn hashline_has_its_own_read_and_edit() {
        assert_eq!(
            names(EditFormat::Hashline),
            ["read", "write", "hashline_edit", "bash", "grep", "glob"]
        );
        let read = builtin_for(EditFormat::Hashline)
            .get("read")
            .unwrap()
            .spec();
        assert!(read.description.contains("address"));
    }

    // Exactly one edit tool at a time, and small definitions, in every format.
    #[test]
    fn one_edit_tool_each_and_compact_definitions() {
        for format in [
            EditFormat::StrReplace,
            EditFormat::ApplyPatch,
            EditFormat::WholeFile,
            EditFormat::Hashline,
        ] {
            let set = names(format);
            // `whole_file`'s edit tool is `write` itself, limited to small files.
            let editing = ["edit", "apply_patch", "hashline_edit"];
            let expected = usize::from(format != EditFormat::WholeFile);
            assert_eq!(
                set.iter().filter(|n| editing.contains(&n.as_str())).count(),
                expected,
                "{format}: {set:?}"
            );
            let first = serde_json::to_string(&builtin_for(format).specs()).unwrap();
            let second = serde_json::to_string(&builtin_for(format).specs()).unwrap();
            assert_eq!(first, second, "{format}");
            assert!(
                first.len() / 4 <= 1700,
                "{format}: ~{} tokens",
                first.len() / 4
            );
        }
    }
}
