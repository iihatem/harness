use harness_core::event::{ErrorKind, TurnEndReason};
use harness_core::meter::WindowSource;
use harness_delegate::{
    DelegateError,
    claude::protocol::{AuthKind, ClaudeParams, ClaudeProtocol},
    driver::{Out, Protocol},
    event::{ApprovalAnswer, DelegateApprovalKind, DelegateEvent, FileChange, LimitStatus},
};
use serde_json::{Value, json};

const HAPPY: &str = include_str!("fixtures/claude/happy.jsonl");
const TOOL_EDIT: &str = include_str!("fixtures/claude/tool_edit.jsonl");
const APPROVAL: &str = include_str!("fixtures/claude/approval.jsonl");
const ERROR_RESULT: &str = include_str!("fixtures/claude/error_result.jsonl");

fn protocol() -> ClaudeProtocol {
    ClaudeProtocol::new(ClaudeParams {
        clock: || 1_000,
        ..ClaudeParams::new("fix the bug", "2.1.312")
    })
}

#[derive(Default)]
struct Played {
    events: Vec<DelegateEvent>,
    writes: Vec<String>,
    finished: bool,
    close_stdin: bool,
    stop: bool,
}

impl Played {
    fn add(&mut self, out: Out) {
        self.events.extend(out.events);
        self.writes.extend(out.writes);
        self.finished |= out.finished;
        self.close_stdin |= out.close_stdin;
        self.stop |= out.stop;
    }
}

fn feed(p: &mut ClaudeProtocol, lines: &str) -> Played {
    let mut played = Played::default();
    for line in lines.lines() {
        played.add(p.on_line(line));
    }
    played
}

fn json(line: &str) -> Value {
    serde_json::from_str(line).unwrap_or_else(|e| panic!("{line}: {e}"))
}

#[test]
fn start_writes_the_task_as_one_stream_json_user_line() {
    let mut p = protocol();
    let out = p.start();
    assert_eq!(out.writes.len(), 1);
    let v = json(&out.writes[0]);
    assert_eq!(v["type"], "user");
    assert_eq!(v["message"]["role"], "user");
    assert_eq!(v["message"]["content"][0]["type"], "text");
    assert_eq!(v["message"]["content"][0]["text"], "fix the bug");
    assert!(v["parent_tool_use_id"].is_null());
    assert!(!out.writes[0].contains('\n'));
}

#[test]
fn a_happy_run_streams_text_once_and_ends_with_usage_cost_and_finished() {
    let mut p = protocol();
    let played = feed(&mut p, HAPPY);
    let e = &played.events;
    assert!(
        matches!(&e[0], DelegateEvent::Started { session_id, model, child_version }
        if session_id == "sess-1" && model.as_deref() == Some("claude-sonnet-4-5") && child_version == "2.1.312")
    );
    assert_eq!(e[1], DelegateEvent::TextDelta { text: "Hel".into() });
    assert_eq!(e[2], DelegateEvent::TextDelta { text: "lo".into() });
    assert_eq!(
        e[3],
        DelegateEvent::AssistantMessage {
            content: "Hello".into(),
            model: Some("claude-sonnet-4-5".into())
        },
        "streamed text is not sent again as a delta"
    );
    let DelegateEvent::RateLimits {
        snapshot, status, ..
    } = &e[4]
    else {
        panic!("{:?}", e[4])
    };
    assert_eq!(status, &Some(LimitStatus::Allowed));
    assert_eq!(snapshot.windows[0].window_minutes, Some(300));
    assert_eq!(snapshot.windows[0].resets_at, Some(1_760_000_000));
    assert_eq!(
        snapshot.windows[0].used_percent, None,
        "no utilization reported: not shown as 0%"
    );
    assert_eq!(snapshot.windows[0].source, WindowSource::Stream);
    assert_eq!(snapshot.observed_at, 1_000);
    let DelegateEvent::Usage { model, usage } = &e[5] else {
        panic!("{:?}", e[5])
    };
    assert_eq!(model, "claude-sonnet-4-5");
    assert_eq!(
        usage.input_tokens, 123,
        "uncached, cache reads and cache writes"
    );
    assert_eq!(usage.cached_tokens, 100);
    assert_eq!(usage.cache_write_tokens, 20);
    assert_eq!(usage.output_tokens, 5);
    assert_eq!(usage.reasoning_tokens, 2);
    assert_eq!(
        e[6],
        DelegateEvent::Cost {
            estimated_usd: 0.0123,
            session_total_usd: 0.0123
        }
    );
    assert_eq!(
        e[7],
        DelegateEvent::Finished {
            reason: TurnEndReason::Completed,
            detail: None
        }
    );
    assert_eq!(e.len(), 8);
    assert!(played.finished && played.close_stdin && !played.stop);
    assert_eq!(p.session_id().as_deref(), Some("sess-1"));
}

#[test]
fn text_that_was_not_streamed_comes_as_a_delta_before_the_message() {
    let mut p = protocol();
    let lines: Vec<&str> = HAPPY.lines().collect();
    let played = feed(&mut p, &[lines[0], lines[4]].join("\n"));
    assert_eq!(
        played.events[1],
        DelegateEvent::TextDelta {
            text: "Hello".into()
        }
    );
    assert!(
        matches!(&played.events[2], DelegateEvent::AssistantMessage { content, .. } if content == "Hello")
    );
}

#[test]
fn an_edit_is_a_call_a_result_and_a_changed_file_and_thinking_is_reasoning() {
    let mut p = protocol();
    let played = feed(&mut p, TOOL_EDIT);
    let e = &played.events;
    let DelegateEvent::ToolCallRequested {
        id,
        name,
        arguments,
    } = &e[1]
    else {
        panic!("{:?}", e[1])
    };
    assert_eq!((id.as_str(), name.as_str()), ("toolu_1", "Edit"));
    assert_eq!(json(arguments)["file_path"], "src/a.rs");
    assert_eq!(
        e[2],
        DelegateEvent::ToolCallFinished {
            id: "toolu_1".into(),
            output: "The file src/a.rs has been updated.".into(),
            is_error: false
        }
    );
    assert_eq!(
        e[3],
        DelegateEvent::FileChanged {
            call_id: "toolu_1".into(),
            path: "src/a.rs".into(),
            change: FileChange::Update
        }
    );
    assert_eq!(
        e[4],
        DelegateEvent::ReasoningDelta {
            text: "all done".into()
        }
    );
    assert!(matches!(&e[6], DelegateEvent::AssistantMessage { content, .. } if content == "Done."));
    assert!(matches!(
        e.iter().find(|x| matches!(x, DelegateEvent::Cost { .. })),
        Some(DelegateEvent::Cost { estimated_usd, .. }) if *estimated_usd == 0.5
    ));
}

#[test]
fn a_failed_edit_changes_no_file_and_a_created_file_is_an_add() {
    let mut p = protocol();
    p.on_line(
        &json!({"type":"assistant","message":{"id":"m","model":"m","role":"assistant","content":[
        {"type":"tool_use","id":"t1","name":"Write","input":{"file_path":"new.rs","content":"x"}},
        {"type":"tool_use","id":"t2","name":"Edit","input":{"file_path":"old.rs"}}]}})
        .to_string(),
    );
    let failed = p.on_line(&json!({"type":"user","message":{"role":"user","content":[
        {"type":"tool_result","tool_use_id":"t2","content":[{"type":"text","text":"no match"}],"is_error":true}]}}).to_string());
    assert!(
        matches!(&failed.events[0], DelegateEvent::ToolCallFinished { output, is_error: true, .. } if output == "no match")
    );
    assert_eq!(failed.events.len(), 1, "no FileChanged for a failed edit");
    let created = p.on_line(
        &json!({"type":"user","message":{"role":"user","content":[
        {"type":"tool_result","tool_use_id":"t1","content":"ok","is_error":false}]},
        "tool_use_result":{"type":"create","filePath":"new.rs"}})
        .to_string(),
    );
    assert!(
        matches!(&created.events[1], DelegateEvent::FileChanged { path, change: FileChange::Add, .. } if path == "new.rs")
    );
}

fn at_approval() -> ClaudeProtocol {
    let mut p = protocol();
    feed(&mut p, APPROVAL);
    p
}

#[test]
fn a_can_use_tool_request_becomes_an_approval() {
    let mut p = protocol();
    let played = feed(&mut p, APPROVAL);
    let DelegateEvent::ApprovalNeeded(a) = played.events.last().unwrap() else {
        panic!()
    };
    assert_eq!(
        (a.id.as_str(), a.call_id.as_str(), a.tool.as_str()),
        ("req_1", "toolu_2", "Bash")
    );
    assert_eq!(a.arguments["command"], "cargo test");
    assert_eq!(
        a.kind,
        DelegateApprovalKind::Command {
            command: "cargo test".into(),
            cwd: None
        }
    );
    assert_eq!(a.reason, "Bash needs approval");
    assert!(a.can_remember);
}

#[test]
fn a_file_tool_approval_names_the_path() {
    let mut p = protocol();
    let out = p.on_line(
        &json!({"type":"control_request","request_id":"r2","request":{"subtype":"can_use_tool",
        "tool_name":"Write","input":{"file_path":"/work/x.rs","content":"1"},"tool_use_id":"t9"}})
        .to_string(),
    );
    let DelegateEvent::ApprovalNeeded(a) = &out.events[0] else {
        panic!()
    };
    assert_eq!(
        a.kind,
        DelegateApprovalKind::FileChange {
            paths: vec!["/work/x.rs".into()]
        }
    );
    assert_eq!(
        a.reason, "Write needs approval",
        "a default reason when claude gives none"
    );
    let out = p.on_line(
        &json!({"type":"control_request","request_id":"r3","request":{"subtype":"can_use_tool",
        "tool_name":"WebFetch","input":{"url":"https://example.com"},"tool_use_id":"t10"}})
        .to_string(),
    );
    let DelegateEvent::ApprovalNeeded(a) = &out.events[0] else {
        panic!()
    };
    assert_eq!(a.kind, DelegateApprovalKind::Tool);
}

fn answer(p: &mut ClaudeProtocol, id: &str, a: ApprovalAnswer) -> Out {
    p.on_answer(id, a).unwrap()
}

#[test]
fn allow_once_answers_allow_with_the_original_input_and_nothing_else() {
    let mut p = at_approval();
    let out = answer(&mut p, "req_1", ApprovalAnswer::AllowOnce);
    assert_eq!(out.writes.len(), 1);
    let v = json(&out.writes[0]);
    assert_eq!(v["type"], "control_response");
    assert_eq!(v["response"]["subtype"], "success");
    assert_eq!(v["response"]["request_id"], "req_1");
    assert_eq!(v["response"]["response"]["behavior"], "allow");
    assert_eq!(
        v["response"]["response"]["updatedInput"]["command"],
        "cargo test"
    );
    assert!(
        v["response"]["response"]
            .get("updatedPermissions")
            .is_none()
    );
    assert!(!out.stop);
}

#[test]
fn allow_for_the_run_passes_only_session_scoped_suggestions_never_a_settings_file() {
    let mut p = at_approval();
    let out = answer(&mut p, "req_1", ApprovalAnswer::AllowForRun);
    let v = json(&out.writes[0]);
    let perms = v["response"]["response"]["updatedPermissions"]
        .as_array()
        .unwrap();
    assert_eq!(perms.len(), 1);
    assert_eq!(perms[0]["destination"], "session");

    // With no session-scoped suggestion there is nothing to pass: a plain allow.
    let mut p = protocol();
    p.on_line(&json!({"type":"control_request","request_id":"r","request":{"subtype":"can_use_tool",
        "tool_name":"Edit","input":{"file_path":"a"},"tool_use_id":"t",
        "permission_suggestions":[{"type":"setMode","mode":"acceptEdits","destination":"userSettings"}]}}).to_string());
    let v = json(&answer(&mut p, "r", ApprovalAnswer::AllowForRun).writes[0]);
    assert!(
        v["response"]["response"]
            .get("updatedPermissions")
            .is_none()
    );
    assert_eq!(v["response"]["response"]["behavior"], "allow");
}

#[test]
fn deny_answers_deny_with_the_message_and_does_not_interrupt() {
    let mut p = at_approval();
    let out = answer(
        &mut p,
        "req_1",
        ApprovalAnswer::Deny {
            reason: Some("not in this repo".into()),
        },
    );
    let v = json(&out.writes[0]);
    assert_eq!(v["response"]["response"]["behavior"], "deny");
    assert_eq!(v["response"]["response"]["message"], "not in this repo");
    assert!(v["response"]["response"].get("interrupt").is_none());
    assert!(!out.stop);

    let mut p = at_approval();
    let v = json(&answer(&mut p, "req_1", ApprovalAnswer::Deny { reason: None }).writes[0]);
    assert!(
        !v["response"]["response"]["message"]
            .as_str()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn cancel_denies_with_interrupt_and_asks_the_driver_to_stop() {
    let mut p = at_approval();
    let out = answer(&mut p, "req_1", ApprovalAnswer::Cancel);
    let v = json(&out.writes[0]);
    assert_eq!(v["response"]["response"]["behavior"], "deny");
    assert_eq!(v["response"]["response"]["interrupt"], true);
    assert!(out.stop, "the escalation clock starts");
    let result = p.on_line(&json!({"type":"result","subtype":"error_during_execution","is_error":true,
        "total_cost_usd":0.1,"result":"","errors":[],"usage":{},"modelUsage":{},"permission_denials":[],"session_id":"s"}).to_string());
    assert!(result.events.iter().any(|e| matches!(
        e,
        DelegateEvent::Finished {
            reason: TurnEndReason::Interrupted,
            ..
        }
    )));
}

#[test]
fn an_answer_is_given_once_and_an_unknown_id_is_refused() {
    let mut p = at_approval();
    answer(&mut p, "req_1", ApprovalAnswer::AllowOnce);
    assert!(matches!(
        p.on_answer("req_1", ApprovalAnswer::AllowOnce),
        Err(DelegateError::UnknownApproval(_))
    ));
    assert!(matches!(
        p.on_answer("nope", ApprovalAnswer::AllowOnce),
        Err(DelegateError::UnknownApproval(_))
    ));
}

#[test]
fn a_withdrawn_request_is_dismissed_and_a_late_answer_writes_nothing() {
    let mut p = at_approval();
    let out = p.on_line(&json!({"type":"control_cancel_request","request_id":"req_1"}).to_string());
    assert_eq!(
        out.events,
        vec![DelegateEvent::ApprovalWithdrawn { id: "req_1".into() }]
    );
    let late = p.on_answer("req_1", ApprovalAnswer::AllowOnce).unwrap();
    assert!(late.writes.is_empty() && late.events.is_empty());
}

#[test]
fn an_interrupt_is_a_control_request_and_the_result_after_it_is_interrupted() {
    let mut p = protocol();
    feed(&mut p, HAPPY.lines().next().unwrap());
    let out = p.on_interrupt();
    let v = json(&out.writes[0]);
    assert_eq!(v["type"], "control_request");
    assert_eq!(v["request"]["subtype"], "interrupt");
    assert!(v["request_id"].as_str().is_some_and(|s| !s.is_empty()));
    let result = p.on_line(HAPPY.lines().last().unwrap());
    assert!(result.events.iter().any(|e| matches!(
        e,
        DelegateEvent::Finished {
            reason: TurnEndReason::Interrupted,
            ..
        }
    )));
    assert!(result.finished);
}

#[test]
fn the_answer_to_our_interrupt_is_swallowed_and_an_error_one_is_a_warning() {
    let mut p = protocol();
    let out = p.on_line(&json!({"type":"control_response","response":{"subtype":"success","request_id":"harness_1","response":{}}}).to_string());
    assert!(out.events.is_empty());
    let out = p.on_line(&json!({"type":"control_response","response":{"subtype":"error","request_id":"harness_1","error":"nope"}}).to_string());
    assert!(
        matches!(&out.events[0], DelegateEvent::Warning { message } if message.contains("nope"))
    );
}

#[test]
fn results_map_to_the_turn_end_reasons() {
    for (subtype, reason) in [
        ("error_max_turns", TurnEndReason::StepLimit),
        ("error_max_budget_usd", TurnEndReason::Budget),
        ("error_during_execution", TurnEndReason::Error),
    ] {
        let mut p = protocol();
        let out = p.on_line(&json!({"type":"result","subtype":subtype,"is_error":true,"total_cost_usd":0.0,
            "result":"","errors":["it broke"],"usage":{},"modelUsage":{},"permission_denials":[],"session_id":"s"}).to_string());
        assert!(
            out.events
                .iter()
                .any(|e| matches!(e, DelegateEvent::Finished { reason: r, .. } if *r == reason)),
            "{subtype}: {:?}",
            out.events
        );
        if reason == TurnEndReason::Error {
            assert!(out.events.iter().any(|e| matches!(e, DelegateEvent::Error { message, .. } if message.contains("it broke"))));
        }
    }
}

#[test]
fn an_error_result_that_says_success_is_still_an_error_and_names_the_denials() {
    let mut p = protocol();
    let played = feed(&mut p, ERROR_RESULT);
    assert!(played.events.iter().any(|e| matches!(e,
        DelegateEvent::Error { kind: ErrorKind::Provider, message } if message.contains("Credit balance is too low"))));
    assert!(played.events.iter().any(|e| matches!(e,
        DelegateEvent::Warning { message } if message.contains("Bash"))));
    assert!(matches!(
        played.events.last(),
        Some(DelegateEvent::Finished {
            reason: TurnEndReason::Error,
            ..
        })
    ));
}

#[test]
fn a_resumed_session_reports_only_what_this_run_added() {
    let mut p = ClaudeProtocol::new(ClaudeParams {
        cost_baseline_usd: 1.0,
        ..ClaudeParams::new("t", "2.1.312")
    });
    let out = p.on_line(
        &json!({"type":"result","subtype":"success","is_error":false,"total_cost_usd":1.25,
        "result":"","usage":{},"modelUsage":{},"permission_denials":[],"session_id":"s"})
        .to_string(),
    );
    assert!(out.events.contains(&DelegateEvent::Cost {
        estimated_usd: 0.25,
        session_total_usd: 1.25
    }));
}

#[test]
fn the_since_last_result_usage_wins_over_the_cumulative_one() {
    let mut p = protocol();
    let out = p.on_line(&json!({"type":"result","subtype":"success","is_error":false,"total_cost_usd":2.0,
        "result":"","usage":{},"permission_denials":[],"session_id":"s",
        "modelUsage":{"m":{"inputTokens":1000,"outputTokens":1000,"cacheReadInputTokens":0,"cacheCreationInputTokens":0}},
        "model_usage_since_last_result":{"m":{"inputTokens":7,"outputTokens":9,"cacheReadInputTokens":0,"cacheCreationInputTokens":0}}}).to_string());
    let DelegateEvent::Usage { usage, .. } = &out.events[0] else {
        panic!("{:?}", out.events)
    };
    assert_eq!((usage.input_tokens, usage.output_tokens), (7, 9));
}

#[test]
fn a_malformed_line_is_a_warning_and_an_unknown_type_is_ignored() {
    let mut p = protocol();
    let out = p.on_line("{not json at all");
    assert!(
        matches!(&out.events[0], DelegateEvent::Warning { message } if message.contains("not JSON"))
    );
    assert!(!out.finished);
    let out = p.on_line(&json!({"type":"some_future_event","x":1}).to_string());
    assert!(out.events.is_empty());
    let out = p.on_line(&json!({"type":"keep_alive"}).to_string());
    assert!(out.events.is_empty());
}

#[test]
fn an_init_that_used_an_api_key_in_subscription_mode_ends_the_run_with_an_error() {
    let mut p = protocol();
    let out = p.on_line(
        &json!({"type":"system","subtype":"init","apiKeySource":"ANTHROPIC_API_KEY",
        "claude_code_version":"2.1.312","model":"m","session_id":"s"})
        .to_string(),
    );
    assert!(
        out.events.iter().any(
            |e| matches!(e, DelegateEvent::Error { message, .. } if message.contains("API key"))
        )
    );
    assert!(matches!(
        out.events.last(),
        Some(DelegateEvent::Finished {
            reason: TurnEndReason::Error,
            ..
        })
    ));
    assert!(out.finished && out.stop && out.close_stdin);

    let mut p = ClaudeProtocol::new(ClaudeParams {
        auth: AuthKind::ApiKey,
        ..ClaudeParams::new("t", "v")
    });
    let out = p.on_line(&json!({"type":"system","subtype":"init","apiKeySource":"none","claude_code_version":"2.1.312","model":"m","session_id":"s"}).to_string());
    assert!(
        out.finished && out.stop,
        "an API key was asked for and not used"
    );
    let ok = p.on_line(&json!({"type":"system","subtype":"init","apiKeySource":"ANTHROPIC_API_KEY","claude_code_version":"2.1.312","model":"m","session_id":"s"}).to_string());
    assert!(!ok.finished);
}

#[test]
fn api_retries_and_model_fallbacks_are_reported() {
    let mut p = protocol();
    let out = p.on_line(&json!({"type":"system","subtype":"api_retry","attempt":2,"max_retries":10,"retry_delay_ms":4000,"error_status":429,"error":"rate_limit"}).to_string());
    assert!(
        matches!(&out.events[0], DelegateEvent::Retrying { attempt: 2, reason, delay_ms: 4000 } if reason.contains("rate_limit") && reason.contains("429"))
    );
    let out = p.on_line(&json!({"type":"system","subtype":"model_fallback","trigger":"overloaded","original_model":"a","fallback_model":"b","content":"x"}).to_string());
    assert!(
        matches!(&out.events[0], DelegateEvent::Warning { message } if message.contains('a') && message.contains('b'))
    );
}

#[test]
fn a_subagents_text_is_not_relayed() {
    let mut p = protocol();
    let out = p.on_line(&json!({"type":"assistant","parent_tool_use_id":"toolu_x","message":{"id":"m","model":"m","role":"assistant","content":[{"type":"text","text":"inner"}]}}).to_string());
    assert!(out.events.is_empty());
    let out = p.on_line(&json!({"type":"stream_event","parent_tool_use_id":"toolu_x","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"inner"}}}).to_string());
    assert!(out.events.is_empty());
}

#[test]
fn rate_limit_windows_are_read_in_percent_and_the_status_is_kept() {
    let mut p = protocol();
    let out = p.on_line(&json!({"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","resetsAt":2000,
        "unifiedWindows":{"five_hour":{"utilization":0.82,"resetsAt":2000,"status":"allowed_warning"},
                          "seven_day":{"utilization":41.0,"resetsAt":900000}}}}).to_string());
    let DelegateEvent::RateLimits {
        snapshot, status, ..
    } = &out.events[0]
    else {
        panic!()
    };
    assert_eq!(*status, Some(LimitStatus::Warning));
    let five = snapshot
        .windows
        .iter()
        .find(|w| w.window_minutes == Some(300))
        .unwrap();
    assert!((five.used_percent.unwrap() - 82.0).abs() < 1e-9);
    let week = snapshot
        .windows
        .iter()
        .find(|w| w.window_minutes == Some(10_080))
        .unwrap();
    assert!((week.used_percent.unwrap() - 41.0).abs() < 1e-9);
    let out = p.on_line(&json!({"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":3000,"rateLimitType":"seven_day"}}).to_string());
    let DelegateEvent::RateLimits {
        status, snapshot, ..
    } = &out.events[0]
    else {
        panic!()
    };
    assert_eq!(*status, Some(LimitStatus::Rejected));
    assert_eq!(snapshot.windows[0].window_minutes, Some(10_080));
}

#[test]
fn a_question_for_the_user_is_denied_with_a_message_and_a_request_harness_cannot_serve_gets_an_error()
 {
    let mut p = protocol();
    let out = p.on_line(
        &json!({"type":"control_request","request_id":"q1","request":{"subtype":"can_use_tool",
        "tool_name":"AskUserQuestion","input":{"questions":[]},"tool_use_id":"t"}})
        .to_string(),
    );
    assert!(
        matches!(&out.events[0], DelegateEvent::Warning { .. }),
        "no approval is offered"
    );
    let v = json(&out.writes[0]);
    assert_eq!(v["response"]["response"]["behavior"], "deny");

    let out = p.on_line(
        &json!({"type":"control_request","request_id":"h1","request":{"subtype":"hook_callback"}})
            .to_string(),
    );
    let v = json(&out.writes[0]);
    assert_eq!(v["response"]["subtype"], "error");
    assert_eq!(v["response"]["request_id"], "h1");
}

fn init_line(extra: Value) -> String {
    let mut v = json!({"type":"system","subtype":"init","apiKeySource":"none",
        "claude_code_version":"2.1.312","model":"m","session_id":"s"});
    for (k, val) in extra.as_object().unwrap() {
        v[k] = val.clone();
    }
    v.to_string()
}

#[test]
fn in_subscription_mode_any_api_key_source_but_none_ends_the_run() {
    for source in [
        "ANTHROPIC_API_KEY",
        "apiKeyHelper",
        "/login managed key",
        "user",
        "project",
        "org",
        "temporary",
        "something-new",
    ] {
        let mut p = protocol();
        let out = p.on_line(&init_line(json!({"apiKeySource": source})));
        assert!(out.finished && out.stop, "{source} is a key");
    }
    let mut p = protocol();
    assert!(!p.on_line(&init_line(json!({}))).finished);
}

#[test]
fn in_subscription_mode_a_provider_other_than_first_party_ends_the_run() {
    for provider in [
        "bedrock",
        "vertex",
        "foundry",
        "anthropicAws",
        "mantle",
        "gateway",
    ] {
        for place in [
            json!({"apiProvider": provider}),
            json!({"account": {"apiProvider": provider}}),
        ] {
            let mut p = protocol();
            let out = p.on_line(&init_line(place));
            assert!(out.finished && out.stop, "{provider}");
            assert!(out.events.iter().any(
                |e| matches!(e, DelegateEvent::Error { message, .. } if message.contains(provider))
            ));
        }
    }
    let mut p = protocol();
    assert!(
        !p.on_line(&init_line(json!({"apiProvider": "firstParty"})))
            .finished
    );
}

#[test]
fn several_text_blocks_of_one_message_are_each_shown_once() {
    let mut p = protocol();
    let mut played = Played::default();
    for text in ["A", "B"] {
        played.add(
            p.on_line(
                &json!({"type":"stream_event","event":{"type":"content_block_delta","index":0,
                "delta":{"type":"text_delta","text":text}}})
                .to_string(),
            ),
        );
    }
    played.add(
        p.on_line(
            &json!({"type":"assistant","message":{"id":"m","model":"x","role":"assistant",
        "content":[{"type":"text","text":"A"},{"type":"text","text":"B"}]}})
            .to_string(),
        ),
    );
    let shown: Vec<&DelegateEvent> = played
        .events
        .iter()
        .filter(|e| {
            matches!(
                e,
                DelegateEvent::TextDelta { .. } | DelegateEvent::AssistantMessage { .. }
            )
        })
        .collect();
    assert_eq!(
        shown,
        [
            &DelegateEvent::TextDelta { text: "A".into() },
            &DelegateEvent::TextDelta { text: "B".into() },
            &DelegateEvent::AssistantMessage {
                content: "A".into(),
                model: Some("x".into())
            },
            &DelegateEvent::AssistantMessage {
                content: "B".into(),
                model: Some("x".into())
            },
        ]
    );
}

#[test]
fn a_streamed_block_that_never_arrives_does_not_hide_the_next_unstreamed_one() {
    let mut p = protocol();
    let mut played = Played::default();
    played.add(
        p.on_line(
            &json!({"type":"stream_event","event":{"type":"content_block_delta","index":0,
            "delta":{"type":"text_delta","text":"lost"}}})
            .to_string(),
        ),
    );
    played.add(
        p.on_line(
            &json!({"type":"assistant","message":{"id":"m","model":"x","role":"assistant",
        "content":[{"type":"text","text":"later"}]}})
            .to_string(),
        ),
    );
    assert!(played.events.contains(&DelegateEvent::TextDelta {
        text: "later".into()
    }));
}

#[test]
fn a_control_request_without_a_request_id_gets_an_error_and_no_approval() {
    let mut p = protocol();
    let out = p.on_line(
        &json!({"type":"control_request","request":{"subtype":"can_use_tool",
        "tool_name":"Bash","input":{"command":"ls"},"tool_use_id":"t"}})
        .to_string(),
    );
    assert!(
        !out.events
            .iter()
            .any(|e| matches!(e, DelegateEvent::ApprovalNeeded(_)))
    );
    assert_eq!(out.writes.len(), 1);
    let v = json(&out.writes[0]);
    assert_eq!(v["type"], "control_response");
    assert_ne!(v["response"]["subtype"], "success");
}
