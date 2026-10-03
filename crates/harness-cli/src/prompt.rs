use harness_core::{edit_format::EditFormat, permission::Mode};

/// The base system prompt: who the agent is and what the approval mode and sandbox let it do, and
/// whether a user can answer approvals (`interactive`). Kept short on purpose, since local models
/// have small context windows. The instruction files and the environment follow it (see
/// `context::system_prompt`).
pub fn base_prompt(mode: Mode, sandboxed: bool, interactive: bool) -> String {
    let sandbox_line = if !sandboxed && mode == Mode::FullAccess {
        "No OS sandbox is active."
    } else if !sandboxed && matches!(mode, Mode::Plan | Mode::ReadOnly) {
        "No OS sandbox is active, so shell commands and file edits are refused; use the read, grep and glob tools instead."
    } else if !sandboxed && interactive {
        "No OS sandbox is active, so every shell command needs the user's approval."
    } else if !sandboxed {
        "No OS sandbox is active, so every shell command needs approval and will be refused; use the file tools instead."
    } else if matches!(mode, Mode::Plan | Mode::ReadOnly) {
        "Shell commands run in a read-only sandbox with no network access, and file edits are refused."
    } else {
        "Shell commands run in a sandbox with no network access; they can write only inside the workspace, temporary directories, and any configured writable roots."
    };
    let approval_line = if mode == Mode::FullAccess {
        "In full-access mode actions run without approval, except those a deny rule forbids or may match."
    } else if interactive {
        "The user is at the terminal: actions that need approval, such as destructive commands, commands harness cannot fully analyse, anything a rule asks to confirm, and in ask mode any unlisted command or file edit, are shown to them to approve or deny. A denied action's result says so, sometimes with the user's reason."
    } else {
        "You are running non-interactively, so actions that need approval will be refused, including destructive commands, commands harness cannot fully analyse, commands the sandbox blocks, anything a rule forbids or asks to confirm, and in ask mode any unlisted command or file edit."
    };
    let rules = format!("{sandbox_line} {approval_line}");
    format!(
        "You are harness, a coding agent working in the user's project.\n\
         Use the tools to inspect files and make changes; never guess file contents.\n\
         Read a file before editing it. Make focused changes, and verify them (for example by running the tests) when you can.\n\
         When you are done, reply with a short summary of what you changed.\n\
         Approval mode: {mode}. {rules}\n"
    )
}

/// The line of the system prompt that says how this model edits files: the tool it has for it
/// and what that tool takes. It depends on the edit format alone, so the prompt stays byte for
/// byte the same for as long as the model does.
pub fn edit_section(format: EditFormat) -> String {
    match format {
        EditFormat::StrReplace => "Edit existing files with the `edit` tool (replace an exact string; read the file first) and create files with `write`.\n",
        EditFormat::ApplyPatch => "Edit files with the `apply_patch` tool: a patch from `*** Begin Patch` to `*** End Patch` with `*** Add File:`, `*** Update File:` and `*** Delete File:` sections. Read a file before updating it.\n",
        EditFormat::WholeFile => "Change files with the `write` tool, giving the complete new content; files of 400 lines or more cannot be changed this way. Read an existing file before overwriting it.\n",
        EditFormat::Hashline => "`read` starts each line with an address (number#hash). Edit files with `hashline_edit`, naming the first and last line to replace by address, and create files with `write`.\n",
    }
    .to_string()
}

/// `base` with the edit section for `format` after it.
pub fn with_edit_section(base: &str, format: EditFormat) -> String {
    format!("{base}{}", edit_section(format))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_prompt_is_under_1000_tokens() {
        for mode in [
            Mode::Plan,
            Mode::ReadOnly,
            Mode::Ask,
            Mode::Auto,
            Mode::FullAccess,
        ] {
            for sandboxed in [true, false] {
                for interactive in [true, false] {
                    let prompt = base_prompt(mode, sandboxed, interactive);
                    assert!(prompt.len() / 4 < 1000, "~{} tokens", prompt.len() / 4);
                }
            }
        }
    }

    // Review Focus: headless runs (e.g. `harness ask`) block on approval, so the model needs to
    // know its mode won't let it ask.
    #[test]
    fn approval_mode_line_names_the_mode() {
        let prompt = base_prompt(Mode::Ask, true, false);
        assert!(prompt.contains("Approval mode: ask"), "{prompt}");
    }

    #[test]
    fn the_prompt_says_whether_commands_are_sandboxed() {
        let yes = base_prompt(Mode::Auto, true, false);
        assert!(yes.contains("run in a sandbox"), "{yes}");
        let no = base_prompt(Mode::Auto, false, false);
        assert!(no.contains("No OS sandbox is active"), "{no}");
        assert!(!no.contains("run in a sandbox"), "{no}");
    }

    // Review Focus: without a sandbox, `check_bash` asks for every shell command outside
    // full-access, so a headless run refuses them all — the prompt must say so plainly instead of
    // implying ordinary commands still run.
    #[test]
    fn without_a_sandbox_the_prompt_says_every_command_needs_approval_unless_full_access() {
        let auto = base_prompt(Mode::Auto, false, false);
        assert!(
            auto.contains("every shell command needs approval"),
            "{auto}"
        );
        let full_access = base_prompt(Mode::FullAccess, false, false);
        assert!(!full_access.contains("every shell command needs approval"));
    }

    #[test]
    fn plan_and_read_only_without_a_sandbox_say_commands_are_refused() {
        for mode in [Mode::Plan, Mode::ReadOnly] {
            let prompt = base_prompt(mode, false, false);
            assert!(
                prompt.contains("shell commands and file edits are refused"),
                "{prompt}"
            );
            assert!(!prompt.contains("needs approval"), "{prompt}");
        }
    }

    #[test]
    fn plan_and_read_only_get_a_read_only_sandbox_line() {
        for mode in [Mode::Plan, Mode::ReadOnly] {
            let prompt = base_prompt(mode, true, false);
            assert!(prompt.contains("read-only sandbox"), "{prompt}");
        }
    }

    #[test]
    fn plan_mode_sandboxed_says_file_edits_are_refused() {
        let prompt = base_prompt(Mode::Plan, true, false);
        assert!(prompt.contains("file edits are refused"), "{prompt}");
    }

    // Review Focus: a headless run refuses actions it can't get approval for; the model must be
    // told this plainly, in every mode that can actually ask (sandboxed or not).
    #[test]
    fn ask_and_auto_state_the_refusal_rule_sandboxed_or_not() {
        for mode in [Mode::Ask, Mode::Auto] {
            for sandboxed in [true, false] {
                let prompt = base_prompt(mode, sandboxed, false);
                assert!(prompt.contains("non-interactively"), "{prompt}");
                assert!(
                    prompt.contains("actions that need approval will be refused"),
                    "{prompt}"
                );
            }
        }
    }

    #[test]
    fn full_access_states_the_full_access_rule_instead_of_the_refusal_rule() {
        let prompt = base_prompt(Mode::FullAccess, false, false);
        assert!(
            prompt.contains("full-access mode actions run without approval"),
            "{prompt}"
        );
        assert!(!prompt.contains("actions that need approval will be refused"));
    }

    #[test]
    fn the_interactive_prompt_says_the_user_answers_approvals() {
        for mode in [Mode::Ask, Mode::Auto] {
            let prompt = base_prompt(mode, true, true);
            assert!(prompt.contains("The user is at the terminal"), "{prompt}");
            assert!(!prompt.contains("non-interactively"), "{prompt}");
            let unsandboxed = base_prompt(mode, false, true);
            assert!(
                unsandboxed.contains("every shell command needs the user's approval"),
                "{unsandboxed}"
            );
            assert!(!unsandboxed.contains("will be refused"), "{unsandboxed}");
        }
        // Full-access asks nobody, interactive or not.
        assert_eq!(
            base_prompt(Mode::FullAccess, false, true),
            base_prompt(Mode::FullAccess, false, false)
        );
    }

    mod edit_formats {
        use super::super::*;
        use harness_core::edit_format::EditFormat;

        const ALL: [EditFormat; 4] = [
            EditFormat::StrReplace,
            EditFormat::ApplyPatch,
            EditFormat::WholeFile,
            EditFormat::Hashline,
        ];

        // Spec: "the system prompt's tool section MUST describe the selected format".
        #[test]
        fn each_section_names_its_edit_tool_and_not_the_others() {
            let tools = |format| match format {
                EditFormat::StrReplace => vec!["`edit`", "`write`"],
                EditFormat::ApplyPatch => vec!["`apply_patch`"],
                EditFormat::WholeFile => vec!["`write`"],
                EditFormat::Hashline => vec!["`hashline_edit`", "`write`"],
            };
            for format in ALL {
                let section = edit_section(format);
                for name in tools(format) {
                    assert!(section.contains(name), "{format}: {section}");
                }
                for other in ["`edit`", "`apply_patch`", "`hashline_edit`"] {
                    if !tools(format).contains(&other) {
                        assert!(!section.contains(other), "{format}: {section}");
                    }
                }
            }
            assert!(edit_section(EditFormat::ApplyPatch).contains("*** Begin Patch"));
            assert!(edit_section(EditFormat::WholeFile).contains("400"));
            assert!(edit_section(EditFormat::Hashline).contains("number#hash"));
        }

        #[test]
        fn the_section_is_one_line_added_after_the_base_prompt() {
            for format in ALL {
                let section = edit_section(format);
                assert!(!section.trim_end().contains('\n'), "{section}");
                let base = base_prompt(Mode::Auto, true, true);
                let with = with_edit_section(&base, format);
                assert_eq!(with, format!("{base}{section}"));
                assert!(with.len() / 4 < 1000);
            }
        }

        #[test]
        fn the_same_format_gives_the_same_bytes() {
            assert_eq!(
                edit_section(EditFormat::ApplyPatch),
                edit_section(EditFormat::ApplyPatch)
            );
            assert_ne!(
                edit_section(EditFormat::ApplyPatch),
                edit_section(EditFormat::StrReplace)
            );
        }
    }
}
