use std::time::Duration;

use harness_core::event::{AgentEvent, TurnEndReason};
use harness_core::permission::Mode;
use harness_core::provider::ProviderError;

#[test]
fn events_serialize_as_tagged_objects() {
    let ev = AgentEvent::ToolCallFinished {
        id: "c1".into(),
        output: "ok".into(),
        is_error: false,
    };
    assert_eq!(
        serde_json::to_string(&ev).unwrap(),
        r#"{"type":"tool_call_finished","id":"c1","output":"ok","is_error":false}"#
    );
    let end = AgentEvent::TurnFinished {
        reason: TurnEndReason::StepLimit,
    };
    assert_eq!(
        serde_json::to_string(&end).unwrap(),
        r#"{"type":"turn_finished","reason":"step_limit"}"#
    );
}

#[test]
fn modes_parse_from_kebab_case() {
    assert_eq!("full-access".parse::<Mode>().unwrap(), Mode::FullAccess);
    assert_eq!("read-only".parse::<Mode>().unwrap(), Mode::ReadOnly);
    assert_eq!(Mode::ReadOnly.to_string(), "read-only");
    assert!("yolo".parse::<Mode>().is_err());
}

#[test]
fn modes_rank_from_plan_and_read_only_up_to_full_access() {
    use Mode::*;
    assert!(Plan.grants_at_most(ReadOnly) && ReadOnly.grants_at_most(Plan));
    assert!(ReadOnly.grants_at_most(Ask) && !Ask.grants_at_most(ReadOnly));
    assert!(!Ask.grants_at_most(Plan));
    assert!(Ask.grants_at_most(Auto) && !Auto.grants_at_most(Ask));
    assert!(Auto.grants_at_most(FullAccess) && !FullAccess.grants_at_most(Auto));
    for mode in [Plan, ReadOnly, Ask, Auto, FullAccess] {
        assert!(mode.grants_at_most(mode), "{mode}");
        assert!(mode.grants_at_most(FullAccess), "{mode}");
    }
}

#[test]
fn only_network_429_and_5xx_are_retryable() {
    let http = |status| ProviderError::Http {
        status,
        body: String::new(),
        retry_after: None,
    };
    assert!(ProviderError::Network("reset".into()).is_retryable());
    assert!(http(429).is_retryable());
    assert!(http(503).is_retryable());
    assert!(!http(401).is_retryable());
    assert!(!ProviderError::Protocol("bad json".into()).is_retryable());
    // Final review, I-1: a hosted provider's first-data timeout is retried, a local server's not.
    let no_start = |local| ProviderError::NoStart {
        message: "the server did not start its reply".into(),
        local,
    };
    assert!(no_start(false).is_retryable());
    assert!(!no_start(true).is_retryable());
    let limited = ProviderError::Http {
        status: 429,
        body: String::new(),
        retry_after: Some(Duration::from_secs(3)),
    };
    assert_eq!(limited.retry_after(), Some(Duration::from_secs(3)));
}
