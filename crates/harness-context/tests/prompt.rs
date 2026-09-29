use std::path::PathBuf;

use harness_context::{
    environment::Environment,
    instructions::InstructionFile,
    prompt::{assemble, oversize_warning},
};

fn file(path: &str, content: &str) -> InstructionFile {
    InstructionFile {
        path: PathBuf::from(path),
        content: content.to_string(),
    }
}

fn environment() -> Environment {
    Environment {
        cwd: PathBuf::from("/work/app"),
        os: "linux".into(),
        date: "2026-09-26".into(),
        git: None,
    }
}

#[test]
fn the_prompt_is_the_base_then_the_instructions_then_the_environment() {
    let prompt = assemble(
        "You are harness.\n",
        &[
            file("/home/u/.config/harness/AGENTS.md", "global rules\n"),
            file("/work/app/AGENTS.md", "project rules\n"),
        ],
        &environment(),
    );
    let base = prompt.find("You are harness.").unwrap();
    let global = prompt
        .find("## /home/u/.config/harness/AGENTS.md\n\nglobal rules")
        .unwrap();
    let project = prompt
        .find("## /work/app/AGENTS.md\n\nproject rules")
        .unwrap();
    let env = prompt.find("# Environment").unwrap();
    assert!(
        base < global && global < project && project < env,
        "{prompt}"
    );
    assert!(prompt.contains("Date: 2026-09-26\n"));
}

#[test]
fn without_instruction_files_there_is_no_instructions_section() {
    let prompt = assemble("You are harness.", &[], &environment());
    assert!(!prompt.contains("# Instructions"), "{prompt}");
    assert!(
        prompt.starts_with("You are harness.\n\n# Environment\n"),
        "{prompt}"
    );
}

#[test]
fn instructions_over_a_quarter_of_the_window_are_flagged() {
    let files = [
        file("/work/app/AGENTS.md", &"x".repeat(8_000)),
        file("/work/app/sub/CLAUDE.md", &"y".repeat(4_000)),
    ];
    let warning = oversize_warning(&files, 8_192).unwrap();
    assert!(warning.contains("3000 tokens"), "{warning}");
    assert!(
        warning.contains("/work/app/AGENTS.md (2000 tokens)"),
        "{warning}"
    );
    assert!(
        warning.contains("/work/app/sub/CLAUDE.md (1000 tokens)"),
        "{warning}"
    );
    assert!(warning.contains("8192-token"), "{warning}");
}

#[test]
fn instructions_within_a_quarter_of_the_window_are_not_flagged() {
    let files = [file("/work/app/AGENTS.md", &"x".repeat(8_192))];
    assert_eq!(oversize_warning(&files, 8_192), None);
    assert_eq!(oversize_warning(&[], 8_192), None);
}
