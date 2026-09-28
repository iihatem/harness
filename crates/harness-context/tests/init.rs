use harness_context::commands::init::init_input;
use harness_core::turn::InputPart;

fn prompt(parts: &[InputPart]) -> &str {
    match parts {
        [InputPart::Text(text)] => text,
        other => panic!("{other:?}"),
    }
}

#[test]
fn init_asks_for_an_agents_md_and_runs_commands_read_only() {
    let dir = tempfile::tempdir().unwrap();
    let input = init_input(dir.path(), "");
    assert!(prompt(&input.parts).contains("write an AGENTS.md"));
    assert!(!prompt(&input.parts).contains("already exists"));
    assert!(input.read_only_shell);
    assert!(input.rules.confirm.is_empty());
    assert_eq!(input.display.as_deref(), Some("/init"));
}

#[test]
fn an_existing_agents_md_needs_confirmation_to_change() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("AGENTS.md"), "old rules\n").unwrap();
    let input = init_input(dir.path(), " mention the API ");
    assert!(prompt(&input.parts).contains("already exists"));
    assert!(prompt(&input.parts).ends_with("Also: mention the API"));
    assert_eq!(input.rules.confirm, ["write:AGENTS.md"]);
    assert_eq!(input.display.as_deref(), Some("/init mention the API"));
}
