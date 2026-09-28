//! The system prompt: a short base prompt, the instruction files, then the environment. It is
//! assembled once when a session starts and sent unchanged on every request, so providers can
//! reuse their prompt caches.

use harness_core::tokens;

use crate::{environment::Environment, instructions::InstructionFile};

/// The system prompt for a session.
pub fn assemble(base: &str, files: &[InstructionFile], environment: &Environment) -> String {
    let mut out = base.trim_end().to_string();
    out.push_str("\n\n");
    if !files.is_empty() {
        out.push_str(
            "# Instructions\n\nThese files come from the user and the project, from the most general to the most specific. Follow them; where they disagree, the more specific file wins.\n",
        );
        for file in files {
            out.push_str(&format!(
                "\n## {}\n\n{}\n",
                file.path.display(),
                file.content.trim_end()
            ));
        }
        out.push('\n');
    }
    out.push_str(&environment.render());
    out
}

/// A warning naming the instruction files and their sizes when together they take more than a
/// quarter of a `context_window`-token context window.
pub fn oversize_warning(files: &[InstructionFile], context_window: u64) -> Option<String> {
    let sizes: Vec<u64> = files.iter().map(|f| tokens::estimate(&f.content)).collect();
    let total: u64 = sizes.iter().sum();
    if total * 4 <= context_window {
        return None;
    }
    let listed: Vec<String> = files
        .iter()
        .zip(&sizes)
        .map(|(file, size)| format!("{} ({size} tokens)", file.path.display()))
        .collect();
    Some(format!(
        "the instruction files take about {total} tokens, more than a quarter of the {context_window}-token context window: {}",
        listed.join(", ")
    ))
}
