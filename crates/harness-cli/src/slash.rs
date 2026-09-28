//! Slash commands in `harness ask`: custom commands and `/init` run as the turn's input; the other
//! built-ins need the interactive terminal.

use harness_context::{
    commands::{self, Commands, expand::expand, init::init_input, is_builtin, parse_invocation},
    project::project_root,
};
use harness_core::{
    permission::PermissionPolicy,
    turn::{InputPart, TurnInput, TurnModel},
};
use harness_providers::registry;

use crate::{context::home, setup, setup::Setup, term::terminal_safe};

/// The project's custom commands when `prompt` is a slash command, printing a warning for each
/// command file that was ignored. `None` for ordinary prompts.
pub fn discover(setup: &Setup, prompt: &str) -> Option<Commands> {
    parse_invocation(prompt)?;
    let found = commands::discover(
        &project_root(&setup.workspace),
        &setup.paths.config_dir,
        home().as_deref(),
    );
    for warning in &found.warnings {
        eprintln!("warning: {}", terminal_safe(warning));
    }
    Some(found)
}

/// Whether `prompt` can run headless: ordinary text, a custom command, or `/init`. The message
/// says why not (exit code 2).
pub fn check(prompt: &str, commands: Option<&Commands>) -> Result<(), String> {
    let (Some(invocation), Some(commands)) = (parse_invocation(prompt), commands) else {
        return Ok(());
    };
    if invocation.name == "init" {
        Ok(())
    } else if is_builtin(invocation.name) {
        Err(format!(
            "/{} works only in interactive mode",
            invocation.name
        ))
    } else if commands.get(invocation.name).is_none() {
        Err(format!(
            "unknown command /{}; custom commands are Markdown files in .harness/commands, .claude/commands or .opencode/commands; to send text that starts with / as a prompt, put a word before it (for example \"Note: /tmp is full\")",
            invocation.name
        ))
    } else {
        Ok(())
    }
}

/// The turn for `prompt` followed by `piped` (the piped-stdin text appended to it, possibly
/// empty). `/init` and custom commands are expanded and `piped` added after them; anything else is
/// sent as it is.
pub fn turn_input(
    prompt: &str,
    piped: &str,
    commands: Option<&Commands>,
    setup: &Setup,
    policy: &dyn PermissionPolicy,
) -> TurnInput {
    let whole = || TurnInput::from(format!("{prompt}{piped}"));
    let (Some(invocation), Some(commands)) = (parse_invocation(prompt), commands) else {
        return whole();
    };
    if invocation.name == "init" {
        let mut input = init_input(&setup.workspace, invocation.args);
        if !piped.is_empty() {
            input.parts.push(InputPart::Text(piped.to_string()));
        }
        return input;
    }
    let Some(command) = commands.get(invocation.name) else {
        return whole();
    };
    let expansion = expand(
        command,
        invocation.args,
        &setup.workspace,
        policy,
        setup.config.trusted,
    );
    for warning in &expansion.warnings {
        eprintln!("warning: {}", terminal_safe(warning));
    }
    for note in &expansion.notes {
        eprintln!("note: {}", terminal_safe(note));
    }
    let mut input = expansion.input;
    if !piped.is_empty() {
        input.parts.push(InputPart::Text(piped.to_string()));
    }
    if let Some(model) = expansion.model {
        match registry::resolve(&model, &setup.config.providers, setup::env) {
            Ok(resolved) => {
                eprintln!("{}", runs_on(&command.name, &resolved.id));
                input.model = Some(TurnModel {
                    provider: resolved.provider,
                    id: resolved.id,
                    name: resolved.model,
                });
            }
            Err(e) => eprintln!("{}", cannot_use(&command.name, &model, &e.to_string())),
        }
    }
    input
}

/// The note that command `name` runs on `model`.
fn runs_on(name: &str, model: &str) -> String {
    format!(
        "note: /{} runs on {}, as its command file asks",
        terminal_safe(name),
        terminal_safe(model)
    )
}

/// The warning that command `name` asks for `model`, which `error` keeps from being used.
fn cannot_use(name: &str, model: &str, error: &str) -> String {
    format!(
        "warning: /{} asks for model {}, which cannot be used ({}); using the session's model",
        terminal_safe(name),
        terminal_safe(model),
        terminal_safe(error)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // Review C, minor 7: a command's name is printed like any other text from a file.
    #[test]
    fn model_messages_print_the_command_name_safely() {
        let name = "x\u{1b}[2J";
        for message in [
            runs_on(name, "mock/m"),
            cannot_use(name, "mock/m", "unknown provider"),
        ] {
            assert!(!message.contains('\u{1b}'), "{message:?}");
            assert!(message.contains("/x\\u{1b}[2J"), "{message:?}");
        }
    }
}
