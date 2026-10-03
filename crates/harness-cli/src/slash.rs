//! Slash commands in `harness ask`: custom commands and `/init` run as the turn's input; the other
//! built-ins need the interactive terminal.

use std::collections::BTreeMap;

use harness_config::config::{self, ProfileSettings};
use harness_context::{
    commands::{
        self, Commands,
        expand::{ProjectTrust, expand},
        init::init_input,
        is_builtin, parse_invocation,
    },
    project::project_root,
};
use harness_core::{
    permission::PermissionPolicy,
    turn::{InputPart, TurnInput, TurnModel},
};
use harness_providers::{
    profiles,
    registry::{self, Resolved},
};

use crate::{context::home, notices::Notices, setup::Setup};

/// The project's custom commands when `prompt` is a slash command, printing a warning for each
/// command file that was ignored. `None` for ordinary prompts.
pub fn discover(setup: &Setup, prompt: &str, notices: &mut Notices) -> Option<Commands> {
    parse_invocation(prompt)?;
    let found = commands::discover(
        &project_root(&setup.workspace),
        &setup.paths.config_dir,
        home().as_deref(),
    );
    for warning in &found.warnings {
        notices.warn(warning);
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

/// Something to tell the user about expanding a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Warning(String),
    Note(String),
}

/// A turn's input, and what to tell the user about how it was expanded, in order.
pub struct Expanded {
    pub input: TurnInput,
    pub messages: Vec<Message>,
}

/// Prints `messages` to stderr, escaped and redacted, and keeps them in `notices`.
pub fn print_messages(messages: &[Message], notices: &mut Notices) {
    for message in messages {
        match message {
            Message::Warning(text) => notices.warn(text),
            Message::Note(text) => notices.note(text),
        }
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
) -> Expanded {
    let whole = || Expanded {
        input: TurnInput::from(format!("{prompt}{piped}")),
        messages: Vec::new(),
    };
    let (Some(invocation), Some(commands)) = (parse_invocation(prompt), commands) else {
        return whole();
    };
    if invocation.name == "init" {
        let mut input = init_input(&setup.workspace, invocation.args);
        if !piped.is_empty() {
            input.parts.push(InputPart::Text(piped.to_string()));
        }
        return Expanded {
            input,
            messages: Vec::new(),
        };
    }
    let Some(command) = commands.get(invocation.name) else {
        return whole();
    };
    // Project command files come from the project root, so trust for that directory decides
    // whether they choose their model, wherever in the project harness runs.
    let root = project_root(&setup.workspace);
    let trust = ProjectTrust {
        dir: &root,
        trusted: config::is_trusted(&setup.paths.global_config_file(), &root, &setup.trust),
    };
    let expansion = expand(command, invocation.args, &setup.workspace, policy, trust);
    let mut messages: Vec<Message> = expansion
        .warnings
        .iter()
        .cloned()
        .map(Message::Warning)
        .collect();
    messages.extend(expansion.notes.iter().cloned().map(Message::Note));
    let mut input = expansion.input;
    if !piped.is_empty() {
        input.parts.push(InputPart::Text(piped.to_string()));
    }
    if let Some(model) = expansion.model {
        match registry::resolve(&model, &setup.config.providers, setup.keys()) {
            Ok(resolved) => {
                messages.push(Message::Note(runs_on(&command.name, &resolved.id)));
                input.model = Some(turn_model(resolved, &setup.config.profiles));
            }
            Err(e) => messages.push(Message::Warning(cannot_use(
                &command.name,
                &model,
                &e.to_string(),
            ))),
        }
    }
    Expanded { input, messages }
}

/// The model for one turn on `resolved`, as a command's `model:` asks for it and as a role will:
/// local when its profile (from `user`) says so, with the edit tool set and the prompt's edit
/// section of the profile's `edit_format`, as the session has them after `/model`.
pub(crate) fn turn_model(
    resolved: Resolved,
    user: &BTreeMap<String, ProfileSettings>,
) -> TurnModel {
    let local = profiles::is_local(&resolved.id, &resolved.base_url);
    let profile = profiles::resolve(&resolved.id, local, user);
    TurnModel {
        local: profile.local,
        tools: Some(harness_tools::builtin_for(profile.edit_format)),
        edit_section: Some(crate::prompt::edit_section(profile.edit_format)),
        provider: resolved.provider,
        id: resolved.id,
        name: resolved.model,
    }
}

/// The note that command `name` runs on `model`.
fn runs_on(name: &str, model: &str) -> String {
    format!("/{name} runs on {model}, as its command file asks")
}

/// The warning that command `name` asks for `model`, which `error` keeps from being used.
fn cannot_use(name: &str, model: &str, error: &str) -> String {
    format!(
        "/{name} asks for model {model}, which cannot be used ({error}); using the session's model"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::term::terminal_safe;

    // Final review, I-1: a command's model on a local server gets a local server's wait, and its
    // rule that a reply that never starts is not asked for again; a profile may say otherwise.
    #[test]
    fn a_commands_model_is_local_as_its_profile_says() {
        use std::collections::BTreeMap;

        use harness_config::config::ProfileSettings;

        let providers = BTreeMap::new();
        let resolved = |id: &str| {
            registry::resolve(id, &providers, |_: &str| Some("sk-test-key".to_string())).unwrap()
        };
        let none = BTreeMap::new();
        assert!(turn_model(resolved("ollama/qwen3"), &none).local);
        assert!(!turn_model(resolved("openrouter/qwen3"), &none).local);
        let hosted = BTreeMap::from([(
            "ollama/*".to_string(),
            ProfileSettings {
                local: Some(false),
                ..ProfileSettings::default()
            },
        )]);
        let model = turn_model(resolved("ollama/qwen3"), &hosted);
        assert!(!model.local);
        assert_eq!(
            (model.id.as_str(), model.name.as_str()),
            ("ollama/qwen3", "qwen3")
        );
    }

    // Final review, M5: a turn on another model has that model's edit format: its tools and the
    // prompt's edit section, whatever the session's model uses.
    #[test]
    fn a_turn_model_has_the_edit_format_of_its_profile() {
        use std::collections::BTreeMap;

        use harness_config::config::ProfileSettings;
        use harness_core::edit_format::EditFormat;

        let providers = BTreeMap::new();
        let resolved = |id: &str| {
            registry::resolve(id, &providers, |_: &str| Some("sk-test-key".to_string())).unwrap()
        };
        let profile = |format| ProfileSettings {
            edit_format: Some(format),
            ..ProfileSettings::default()
        };
        let user = BTreeMap::from([
            (
                "openrouter/patcher".to_string(),
                profile(EditFormat::ApplyPatch),
            ),
            (
                "openrouter/replacer".to_string(),
                profile(EditFormat::StrReplace),
            ),
        ]);
        let names = |m: &TurnModel| -> Vec<String> {
            m.tools
                .as_ref()
                .unwrap()
                .specs()
                .into_iter()
                .map(|s| s.name)
                .collect()
        };
        let patcher = turn_model(resolved("openrouter/patcher"), &user);
        let replacer = turn_model(resolved("openrouter/replacer"), &user);
        assert!(names(&patcher).contains(&"apply_patch".to_string()));
        assert!(!names(&patcher).contains(&"edit".to_string()));
        assert!(names(&replacer).contains(&"edit".to_string()));
        assert!(!names(&replacer).contains(&"apply_patch".to_string()));
        assert_eq!(
            patcher.edit_section.as_deref(),
            Some(crate::prompt::edit_section(EditFormat::ApplyPatch).as_str())
        );
        assert_eq!(
            replacer.edit_section.as_deref(),
            Some(crate::prompt::edit_section(EditFormat::StrReplace).as_str())
        );
    }

    // Review C, minor 7: a command's name is printed like any other text from a file: the
    // messages carry it as it is, and are escaped where they are printed.
    #[test]
    fn model_messages_name_the_command_and_are_printed_safely() {
        let name = "x\u{1b}[2J";
        for message in [
            runs_on(name, "mock/m"),
            cannot_use(name, "mock/m", "unknown provider"),
        ] {
            assert!(message.contains("/x\u{1b}[2J"), "{message:?}");
            let printed = terminal_safe(&message);
            assert!(!printed.contains('\u{1b}'), "{printed:?}");
            assert!(printed.contains("/x\\u{1b}[2J"), "{printed:?}");
        }
    }
}
