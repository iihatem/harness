//! `/init`: the model inspects the project and drafts an `AGENTS.md`.

use std::path::Path;

use harness_core::{
    engine::RuleSet,
    turn::{InputPart, TurnInput},
};

const DRAFT: &str = "Inspect this project and write an AGENTS.md at its top level for coding agents that work in it. First read the README, the build and test configuration, and a sample of the source. Then cover: what the project is; how to build, test and lint it, with the exact commands; how the code is laid out; and the conventions a newcomer would get wrong. Keep it short (under 150 lines) and specific to this project, with no generic advice. Write it with the write tool.";

const EXISTING: &str = "An AGENTS.md already exists. Read it first, keep what is still right, and write the improved version with the write tool; the user will see the change and decide whether to keep it.";

/// The turn `/init` runs in `workspace`, with `args` as extra instructions. Its shell commands run
/// read-only, and when `AGENTS.md` already exists, writing it needs the user's confirmation.
pub fn init_input(workspace: &Path, args: &str) -> TurnInput {
    let exists = workspace.join("AGENTS.md").exists();
    let mut prompt = DRAFT.to_string();
    if exists {
        prompt.push_str("\n\n");
        prompt.push_str(EXISTING);
    }
    let args = args.trim();
    if !args.is_empty() {
        prompt.push_str(&format!("\n\nAlso: {args}"));
    }
    TurnInput {
        parts: vec![InputPart::Text(prompt)],
        display: Some(format!("/init {args}").trim_end().to_string()),
        rules: RuleSet {
            confirm: if exists {
                vec!["write:AGENTS.md".to_string()]
            } else {
                Vec::new()
            },
            ..RuleSet::default()
        },
        read_only_shell: true,
        ..TurnInput::default()
    }
}
