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
