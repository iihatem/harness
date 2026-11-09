use std::sync::Arc;

use harness_core::{event::TurnEndReason, redact::Redactor};
use harness_delegate::{
    event::{DelegateApproval, DelegateApprovalKind, DelegateEvent},
    redact::EventFilter,
};
use serde_json::json;

const SECRET: &str = "sk-live-0123456789abcdef";

fn filter() -> EventFilter {
    let redactor = Arc::new(Redactor::default());
    redactor.add(SECRET);
    EventFilter::new(redactor)
}

fn text(events: &[DelegateEvent]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            DelegateEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[test]
fn a_secret_split_across_two_text_deltas_is_replaced_whole() {
    let mut f = filter();
    let mut shown = f.push(DelegateEvent::TextDelta {
        text: "key is sk-live-0123".into(),
    });
    shown.extend(f.push(DelegateEvent::TextDelta {
        text: "456789abcdef, ok".into(),
    }));
    shown.extend(f.finish());
    let all = text(&shown);
    assert!(!all.contains("sk-live"), "{all}");
    assert!(all.contains("[redacted]"), "{all}");
    assert!(all.starts_with("key is "), "{all}");
    assert!(all.ends_with(", ok"), "{all}");
}

#[test]
fn held_back_text_comes_out_before_an_event_of_another_kind() {
    let mut f = filter();
    let mut shown = f.push(DelegateEvent::TextDelta {
        text: "done sk-liv".into(),
    });
    shown.extend(f.push(DelegateEvent::Finished {
        reason: TurnEndReason::Completed,
        detail: None,
    }));
    assert!(matches!(shown.last(), Some(DelegateEvent::Finished { .. })));
    assert!(text(&shown).starts_with("done "), "{shown:?}");
}

#[test]
fn every_free_text_field_is_redacted() {
    let mut f = filter();
    let secret = SECRET;
    let events = vec![
        DelegateEvent::ToolCallRequested {
            id: "1".into(),
            name: "Bash".into(),
            arguments: format!("{{\"command\":\"curl -H {secret}\"}}"),
        },
        DelegateEvent::ToolCallFinished {
            id: "1".into(),
            output: format!("got {secret}"),
            is_error: false,
        },
        DelegateEvent::Warning {
            message: format!("w {secret}"),
        },
        DelegateEvent::Error {
            kind: harness_core::event::ErrorKind::Provider,
            message: format!("e {secret}"),
        },
        DelegateEvent::AssistantMessage {
            content: format!("said {secret}"),
            model: Some("m".into()),
        },
    ];
    for event in events {
        for shown in f.push(event) {
            let json = serde_json::to_string(&shown).unwrap();
            assert!(!json.contains(SECRET), "{json}");
        }
    }
}

#[test]
fn approval_arguments_are_passed_raw_but_its_reason_is_redacted() {
    let mut f = filter();
    let approval = DelegateApproval {
        id: "a1".into(),
        call_id: "c1".into(),
        tool: "Bash".into(),
        arguments: json!({"command": format!("echo {SECRET}")}),
        kind: DelegateApprovalKind::Command {
            command: format!("echo {SECRET}"),
            cwd: None,
        },
        reason: format!("needs {SECRET}"),
        can_remember: true,
    };
    let shown = f.push(DelegateEvent::ApprovalNeeded(approval));
    let [DelegateEvent::ApprovalNeeded(out)] = shown.as_slice() else {
        panic!("{shown:?}")
    };
    assert_eq!(out.arguments["command"], format!("echo {SECRET}"));
    assert!(!out.reason.contains(SECRET));
    let DelegateApprovalKind::Command { command, .. } = &out.kind else {
        panic!()
    };
    assert!(!command.contains(SECRET), "the shown command is redacted");
}
