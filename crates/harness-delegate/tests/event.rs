use harness_core::{event::TurnEndReason, message::Usage};
use harness_delegate::event::{ApprovalAnswer, DelegateEvent, FileChange};

#[test]
fn events_round_trip_through_json_with_snake_case_tags() {
    let events = vec![
        DelegateEvent::Started {
            session_id: "s".into(),
            model: Some("m".into()),
            child_version: "2.1.312".into(),
        },
        DelegateEvent::TextDelta { text: "hi".into() },
        DelegateEvent::FileChanged {
            call_id: "c".into(),
            path: "a.rs".into(),
            change: FileChange::Update,
        },
        DelegateEvent::Usage {
            model: "m".into(),
            usage: Usage {
                input_tokens: 3,
                output_tokens: 4,
                ..Usage::default()
            },
        },
        DelegateEvent::Cost {
            estimated_usd: 0.5,
            session_total_usd: 1.5,
        },
        DelegateEvent::Finished {
            reason: TurnEndReason::Interrupted,
            detail: Some("x".into()),
        },
    ];
    for event in events {
        let json = serde_json::to_string(&event).unwrap();
        let back: DelegateEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back, event);
    }
    let json = serde_json::to_value(DelegateEvent::TextDelta { text: "a".into() }).unwrap();
    assert_eq!(json["type"], "text_delta");
}

#[test]
fn a_deny_answer_may_carry_a_reason() {
    let a = ApprovalAnswer::Deny {
        reason: Some("no".into()),
    };
    assert_ne!(a, ApprovalAnswer::Cancel);
}
