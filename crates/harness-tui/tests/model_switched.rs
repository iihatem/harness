//! A model switch is a line in the scrollback: which model, for which role, from which model, and
//! why; and `/model` shows it.

use harness_core::{
    event::AgentEvent,
    role::{Role, SwitchReason},
};
use harness_tui::{style::Theme, transcript::Transcript};

fn line_for(event: AgentEvent) -> String {
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
        .filter(|text| !text.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn switched(role: Role, reason: SwitchReason, detail: Option<&str>) -> AgentEvent {
    AgentEvent::ModelSwitched {
        from: "chatgpt/gpt-5".into(),
        to: "openai/gpt-5".into(),
        role,
        reason,
        detail: detail.map(String::from),
    }
}

// Spec "User switch": a `ModelSwitched` event with reason `user` is shown as a line.
#[test]
fn a_user_switch_is_a_line_naming_both_models_and_the_role() {
    let line = line_for(switched(Role::Main, SwitchReason::User, None));
    assert_eq!(
        line,
        "switched to openai/gpt-5 (main role, from chatgpt/gpt-5; you chose it)"
    );
}

// Spec "Subscription limit with a chain": the announcement says why and whether it is billed.
#[test]
fn a_fallback_says_what_failed_and_whether_the_model_is_billed() {
    let line = line_for(switched(
        Role::Plan,
        SwitchReason::Fallback,
        Some("chatgpt/gpt-5 reported its usage limit; openai/gpt-5 is billed"),
    ));
    assert_eq!(
        line,
        "switched to openai/gpt-5 (plan role, from chatgpt/gpt-5; fallback): chatgpt/gpt-5 reported its usage limit; openai/gpt-5 is billed"
    );
}

#[test]
fn an_escalation_is_named() {
    let line = line_for(switched(Role::Main, SwitchReason::Escalation, None));
    assert!(line.ends_with("; escalation)"), "{line}");
}

#[test]
fn a_model_name_cannot_carry_an_escape_to_the_screen() {
    let line = line_for(AgentEvent::ModelSwitched {
        from: "a/b".into(),
        to: "x/\u{1b}[2Jevil".into(),
        role: Role::Main,
        reason: SwitchReason::User,
        detail: None,
    });
    assert!(!line.contains('\u{1b}'), "{line:?}");
}
