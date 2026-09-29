//! The session's system prompt: the base prompt, the instruction files, and the environment,
//! captured once when the session starts.

use std::path::PathBuf;

use harness_context::{environment, instructions, prompt};
use harness_core::{time::today_utc, tokens::DEFAULT_CONTEXT_WINDOW};

use crate::{setup::Setup, term::terminal_safe};

/// `$HOME`, resolved through symlinks.
pub fn home() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    Some(home.canonicalize().unwrap_or(home))
}

/// Builds the system prompt for a session in `setup.workspace` from `base` (see
/// `prompt::base_prompt`), printing a warning for each instruction file or import that could not
/// be used, and when the instruction files are large.
pub fn system_prompt(setup: &Setup, base: &str) -> String {
    let loaded =
        instructions::discover(&setup.workspace, &setup.paths.config_dir, home().as_deref());
    for warning in &loaded.warnings {
        eprintln!("warning: {}", terminal_safe(warning));
    }
    if let Some(warning) = prompt::oversize_warning(&loaded.files, DEFAULT_CONTEXT_WINDOW) {
        eprintln!("warning: {}", terminal_safe(&warning));
    }
    let environment = environment::capture(&setup.workspace, &today_utc());
    prompt::assemble(base, &loaded.files, &environment)
}
