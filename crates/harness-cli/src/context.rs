//! The session's system prompt: the base prompt, the instruction files, and the environment,
//! captured once when the session starts.

use std::path::PathBuf;

use harness_context::{environment, instructions, prompt};
use harness_core::time::today_utc;

use crate::{notices::Notices, setup::Setup};

/// `$HOME`, resolved through symlinks.
pub fn home() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    Some(home.canonicalize().unwrap_or(home))
}

/// Builds the system prompt for a session in `setup.workspace` from `base` (see
/// `prompt::base_prompt`), printing a warning for each instruction file or import that could not
/// be used, and when the instruction files take more than a quarter of `context_window`.
pub fn system_prompt(
    setup: &Setup,
    base: &str,
    context_window: u64,
    notices: &mut Notices,
) -> String {
    let loaded =
        instructions::discover(&setup.workspace, &setup.paths.config_dir, home().as_deref());
    for warning in &loaded.warnings {
        notices.warn(warning);
    }
    if let Some(warning) = prompt::oversize_warning(&loaded.files, context_window) {
        notices.warn(&warning);
    }
    let environment = environment::capture(&setup.workspace, &today_utc());
    prompt::assemble(base, &loaded.files, &environment)
}

/// The instruction files in the session's system prompt and their estimated tokens, for
/// `/context`: named relative to the workspace when inside it.
pub fn instruction_files(setup: &Setup) -> Vec<(String, u64)> {
    instructions::discover(&setup.workspace, &setup.paths.config_dir, home().as_deref())
        .files
        .into_iter()
        .map(|file| {
            let name = file
                .path
                .strip_prefix(&setup.workspace)
                .unwrap_or(&file.path)
                .display()
                .to_string();
            (name, harness_core::tokens::estimate(&file.content))
        })
        .collect()
}
