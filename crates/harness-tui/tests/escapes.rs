//! What the model, a tool or a config file puts in an announcement does not reach the terminal as
//! control codes.

use harness_core::{
    event::{AgentEvent, EscalationTrigger},
    role::{Role, SwitchReason},
};
use harness_tui::{style::Theme, transcript::Transcript};

fn shown(event: AgentEvent) -> String {
    let mut transcript = Transcript::new(Theme::monochrome());
    transcript.on_event(&event, 300);
    transcript
        .take_finished()
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn escape_codes_in_the_last_failure_a_notice_or_a_switch_do_not_reach_the_screen() {
    let evil = "\u{1b}]0;owned\u{7}\u{1b}[2J";
    for event in [
        AgentEvent::EscalationSuggested {
            trigger: EscalationTrigger::IdenticalFailures,
            count: 3,
            first_line: Some(format!("error {evil}")),
            to: format!("big/{evil}"),
        },
        AgentEvent::HandoffReduced {
            to: format!("build/{evil}"),
            history_tokens: 60_000,
            window: 32_768,
            forced: false,
        },
        AgentEvent::ModelSwitched {
            from: format!("a/{evil}"),
            to: "b/c".into(),
            role: Role::Main,
            reason: SwitchReason::Fallback,
            detail: Some(format!("it said {evil}")),
        },
    ] {
        let text = shown(event.clone());
        assert!(
            !text.contains('\u{1b}') && !text.contains('\u{7}'),
            "{event:?}: {text:?}"
        );
        assert!(!text.is_empty());
    }
}
