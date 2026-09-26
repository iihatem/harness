use std::{
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use harness_core::permission::Mode;

/// The base system prompt plus environment facts captured once per run. Kept short on purpose:
/// local models have small context windows. P3 replaces this with full context assembly.
pub fn system_prompt(workspace: &Path, date: &str, mode: Mode, sandboxed: bool) -> String {
    let sandbox_line = if !sandboxed && mode == Mode::FullAccess {
        "No OS sandbox is active."
    } else if !sandboxed {
        "No OS sandbox is active, so every shell command needs approval and will be refused; use the file tools instead."
    } else if matches!(mode, Mode::Plan | Mode::ReadOnly) {
        "Shell commands run in a read-only sandbox with no network access, and file edits are refused."
    } else {
        "Shell commands run in a sandbox with no network access; they can write only inside the workspace, temporary directories, and any configured writable roots."
    };
    let approval_line = if mode == Mode::FullAccess {
        "In full-access mode actions run without approval, except those a deny rule forbids or may match."
    } else {
        "You are running non-interactively, so actions that need approval will be refused, including destructive commands, commands harness cannot fully analyse, commands the sandbox blocks, anything a rule forbids or asks to confirm, and in ask mode any unlisted command or file edit."
    };
    let rules = format!("{sandbox_line} {approval_line}");
    format!(
        "You are harness, a coding agent working in the user's project.\n\
         Use the tools to inspect files and make changes; never guess file contents.\n\
         Read a file before editing it. Make focused changes, and verify them (for example by running the tests) when you can.\n\
         When you are done, reply with a short summary of what you changed.\n\
         Approval mode: {mode}. {rules}\n\
         \n\
         Working directory: {}\n\
         Operating system: {}\n\
         Date: {date}\n",
        workspace.display(),
        std::env::consts::OS
    )
}

/// Today's date in UTC as `YYYY-MM-DD`.
pub fn today_utc() -> String {
    civil_date(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    )
}

/// Converts Unix seconds to a UTC calendar date (Howard Hinnant's days-to-civil algorithm).
pub fn civil_date(unix_secs: u64) -> String {
    let z = (unix_secs / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates_are_correct() {
        assert_eq!(civil_date(0), "1970-01-01");
        assert_eq!(civil_date(1_709_164_800), "2024-02-29");
        assert_eq!(civil_date(1_790_208_000), "2026-09-24");
    }

    #[test]
    fn base_prompt_is_under_1000_tokens() {
        let prompt = system_prompt(Path::new("/some/project"), "2026-09-24", Mode::Auto, true);
        assert!(prompt.len() / 4 < 1000, "~{} tokens", prompt.len() / 4);
        assert!(prompt.contains("Working directory: /some/project"));
    }

    // Review Focus: headless runs (e.g. `harness ask`) block on approval, so the model needs to
    // know its mode won't let it ask.
    #[test]
    fn approval_mode_line_names_the_mode() {
        let prompt = system_prompt(Path::new("/some/project"), "2026-09-24", Mode::Ask, true);
        assert!(prompt.contains("Approval mode: ask"), "{prompt}");
    }

    #[test]
    fn the_prompt_says_whether_commands_are_sandboxed() {
        let yes = system_prompt(Path::new("/p"), "2026-09-26", Mode::Auto, true);
        assert!(yes.contains("run in a sandbox"), "{yes}");
        let no = system_prompt(Path::new("/p"), "2026-09-26", Mode::Auto, false);
        assert!(no.contains("No OS sandbox is active"), "{no}");
        assert!(!no.contains("run in a sandbox"), "{no}");
    }

    // Review Focus: without a sandbox, `check_bash` asks for every shell command outside
    // full-access, so a headless run refuses them all — the prompt must say so plainly instead of
    // implying ordinary commands still run.
    #[test]
    fn without_a_sandbox_the_prompt_says_every_command_needs_approval_unless_full_access() {
        let auto = system_prompt(Path::new("/p"), "2026-09-26", Mode::Auto, false);
        assert!(
            auto.contains("every shell command needs approval"),
            "{auto}"
        );
        let full_access = system_prompt(Path::new("/p"), "2026-09-26", Mode::FullAccess, false);
        assert!(!full_access.contains("every shell command needs approval"));
    }

    #[test]
    fn plan_and_read_only_get_a_read_only_sandbox_line() {
        for mode in [Mode::Plan, Mode::ReadOnly] {
            let prompt = system_prompt(Path::new("/p"), "2026-09-26", mode, true);
            assert!(prompt.contains("read-only sandbox"), "{prompt}");
        }
    }

    #[test]
    fn plan_mode_sandboxed_says_file_edits_are_refused() {
        let prompt = system_prompt(Path::new("/p"), "2026-09-26", Mode::Plan, true);
        assert!(prompt.contains("file edits are refused"), "{prompt}");
    }

    // Review Focus: a headless run refuses actions it can't get approval for; the model must be
    // told this plainly, in every mode that can actually ask (sandboxed or not).
    #[test]
    fn ask_and_auto_state_the_refusal_rule_sandboxed_or_not() {
        for mode in [Mode::Ask, Mode::Auto] {
            for sandboxed in [true, false] {
                let prompt = system_prompt(Path::new("/p"), "2026-09-26", mode, sandboxed);
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
        let prompt = system_prompt(Path::new("/p"), "2026-09-26", Mode::FullAccess, false);
        assert!(
            prompt.contains("full-access mode actions run without approval"),
            "{prompt}"
        );
        assert!(!prompt.contains("actions that need approval will be refused"));
    }
}
