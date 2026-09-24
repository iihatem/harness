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
    assert!(Mode::Ask.is_narrow());
    assert!(!Mode::Auto.is_narrow());
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
    let limited = ProviderError::Http {
        status: 429,
        body: String::new(),
        retry_after: Some(Duration::from_secs(3)),
    };
    assert_eq!(limited.retry_after(), Some(Duration::from_secs(3)));
}
