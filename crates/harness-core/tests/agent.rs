mod common;

use std::sync::Arc;

use common::*;
use harness_core::agent::NonInteractive;
use harness_core::event::{AgentEvent, TurnEndReason};
use harness_core::message::Message;
use harness_core::permission::Mode;
use harness_core::provider::ProviderError;
use harness_core::testing::{MockProvider, Script};
use serde_json::json;

#[tokio::test]
async fn multi_step_turn_runs_tools_in_order_then_completes() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call("c1", "echo", json!({"text": "one"})),
        Script::tool_call("c2", "echo", json!({"text": "two"})),
        Script::text("done"),
    ]);
    let mut agent = agent(
        provider.clone(),
        Mode::Auto,
        Arc::new(NonInteractive),
        dir.path(),
    );
    let (reason, events) = run(&mut agent, "go").await;

    assert_eq!(reason, TurnEndReason::Completed);
    assert_eq!(
        finished_outputs(&events),
        vec![("one".to_string(), false), ("two".to_string(), false)]
    );
    assert_eq!(events.first(), Some(&AgentEvent::TurnStarted));
    assert_eq!(
        events.last(),
        Some(&AgentEvent::TurnFinished {
            reason: TurnEndReason::Completed
        })
    );
    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    assert!(
        requests[2]
            .messages
            .iter()
            .any(|m| matches!(m, Message::Tool { content, .. } if content == "two"))
    );
    assert_eq!(requests[0].system, "system prompt");
    assert_eq!(requests[0].model, "m1");
}

#[tokio::test]
async fn tool_call_requested_precedes_finished_with_the_same_id() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call("c9", "echo", json!({"text": "x"})),
        Script::text("ok"),
    ]);
    let mut agent = agent(provider, Mode::Auto, Arc::new(NonInteractive), dir.path());
    let (_, events) = run(&mut agent, "go").await;
    let requested = events
        .iter()
        .position(|e| matches!(e, AgentEvent::ToolCallRequested { id, .. } if id == "c9"))
        .unwrap();
    let finished = events
        .iter()
        .position(|e| matches!(e, AgentEvent::ToolCallFinished { id, .. } if id == "c9"))
        .unwrap();
    assert!(requested < finished);
}

#[tokio::test]
async fn assistant_messages_are_attributed_to_the_model() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![Script::text("hello")]);
    let mut agent = agent(provider, Mode::Auto, Arc::new(NonInteractive), dir.path());
    let (_, events) = run(&mut agent, "hi").await;
    assert!(events.contains(&AgentEvent::AssistantMessage {
        content: "hello".into(),
        model: "mock/m1".into()
    }));
    assert!(matches!(&agent.history()[1], Message::Assistant { model, .. } if model == "mock/m1"));
}

#[tokio::test]
async fn step_limit_stops_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let script = (0..5)
        .map(|i| Script::tool_call(&format!("c{i}"), "echo", json!({"text": "again"})))
        .collect();
    let provider = MockProvider::new(script);
    let mut agent = agent(
        provider.clone(),
        Mode::Auto,
        Arc::new(NonInteractive),
        dir.path(),
    );
    agent_config_max_steps(&mut agent, 3);
    let (reason, events) = run(&mut agent, "loop").await;
    assert_eq!(reason, TurnEndReason::StepLimit);
    assert_eq!(provider.requests().len(), 3);
    assert_eq!(
        events.last(),
        Some(&AgentEvent::TurnFinished {
            reason: TurnEndReason::StepLimit
        })
    );
}

fn agent_config_max_steps(agent: &mut harness_core::agent::Agent, steps: u32) {
    agent.config_mut().max_steps = steps;
}

#[tokio::test]
async fn invalid_json_arguments_are_not_executed_and_are_counted() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::raw_tool_call("c1", "touch", "{not json"),
        Script::text("sorry"),
    ]);
    let mut agent = agent(provider, Mode::Auto, Arc::new(NonInteractive), dir.path());
    let (reason, events) = run(&mut agent, "go").await;
    assert_eq!(reason, TurnEndReason::Completed);
    let outputs = finished_outputs(&events);
    assert!(
        outputs[0].1 && outputs[0].0.contains("not valid JSON"),
        "{outputs:?}"
    );
    assert_eq!(agent.invalid_calls_this_turn(), 1);
}

#[tokio::test]
async fn schema_violations_are_reported_to_the_model() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call("c1", "echo", json!({"txt": "x"})),
        Script::text("ok"),
    ]);
    let mut agent = agent(provider, Mode::Auto, Arc::new(NonInteractive), dir.path());
    let (_, events) = run(&mut agent, "go").await;
    let (output, is_error) = &finished_outputs(&events)[0];
    assert!(is_error);
    assert!(
        output.contains("invalid arguments") && output.contains("text"),
        "{output}"
    );
}

#[tokio::test]
async fn unknown_tools_are_reported_to_the_model() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call("c1", "teleport", json!({})),
        Script::text("ok"),
    ]);
    let mut agent = agent(provider, Mode::Auto, Arc::new(NonInteractive), dir.path());
    let (_, events) = run(&mut agent, "go").await;
    let (output, is_error) = &finished_outputs(&events)[0];
    assert!(*is_error && output.contains("unknown tool `teleport`"));
}

#[tokio::test]
async fn headless_writes_in_ask_mode_are_blocked() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call("c1", "touch", json!({"path": "a.txt"})),
        Script::text("ok"),
    ]);
    let mut agent = agent(provider, Mode::Ask, Arc::new(NonInteractive), dir.path());
    let (reason, events) = run(&mut agent, "go").await;
    assert_eq!(reason, TurnEndReason::Completed);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::ActionBlocked { id, .. } if id == "c1"))
    );
    assert!(!dir.path().join("a.txt").exists());
}

#[tokio::test]
async fn approved_actions_run() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call("c1", "touch", json!({"path": "a.txt"})),
        Script::text("ok"),
    ]);
    let mut agent = agent(provider, Mode::Ask, Arc::new(AlwaysApprove), dir.path());
    let (_, events) = run(&mut agent, "go").await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::ApprovalNeeded { .. }))
    );
    assert!(dir.path().join("a.txt").exists());
}

#[tokio::test]
async fn denied_actions_in_read_only_mode_never_ask() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call("c1", "touch", json!({"path": "a.txt"})),
        Script::text("ok"),
    ]);
    let mut agent = agent(
        provider,
        Mode::ReadOnly,
        Arc::new(AlwaysApprove),
        dir.path(),
    );
    let (_, events) = run(&mut agent, "go").await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::ApprovalNeeded { .. }))
    );
    assert!(finished_outputs(&events)[0].0.starts_with("denied:"));
    assert!(!dir.path().join("a.txt").exists());
}

#[tokio::test]
async fn tool_failures_do_not_abort_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call("c1", "fail", json!({})),
        Script::text("recovered"),
    ]);
    let mut agent = agent(provider, Mode::Auto, Arc::new(NonInteractive), dir.path());
    let (reason, events) = run(&mut agent, "go").await;
    assert_eq!(reason, TurnEndReason::Completed);
    assert_eq!(
        finished_outputs(&events),
        vec![("tool exploded".to_string(), true)]
    );
}

#[tokio::test]
async fn provider_errors_end_the_turn_but_the_session_stays_usable() {
    let dir = tempfile::tempdir().unwrap();
    let unauthorized = ProviderError::Http {
        status: 401,
        body: "bad key".into(),
        retry_after: None,
    };
    let provider = MockProvider::new(vec![
        Script::error(unauthorized),
        Script::text("second try"),
    ]);
    let mut agent = agent(provider, Mode::Auto, Arc::new(NonInteractive), dir.path());
    let (first, events) = run(&mut agent, "one").await;
    assert_eq!(first, TurnEndReason::Error);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Error { message, .. } if message.contains("401")))
    );
    let (second, _) = run(&mut agent, "two").await;
    assert_eq!(second, TurnEndReason::Completed);
}

// Review Focus: providers/models that don't send a tool-call id (or reuse one) across steps.
// Two spilled outputs sharing a fallback id like `call_0` would overwrite the same spill file.
#[tokio::test]
async fn duplicate_tool_call_ids_are_rewritten_to_stay_unique() {
    let dir = tempfile::tempdir().unwrap();
    let one = "one".repeat(7000);
    let two = "two".repeat(7000);
    let provider = MockProvider::new(vec![
        Script::tool_call("call_0", "echo", json!({"text": one})),
        Script::tool_call("call_0", "echo", json!({"text": two})),
        Script::text("done"),
    ]);
    let mut agent = agent(provider, Mode::Auto, Arc::new(NonInteractive), dir.path());
    let (_, events) = run(&mut agent, "go").await;

    let finished: Vec<(String, String)> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolCallFinished { id, output, .. } => Some((id.clone(), output.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(finished.len(), 2, "{finished:?}");
    assert_ne!(
        finished[0].0, finished[1].0,
        "ids must differ: {finished:?}"
    );

    fn spill_path(output: &str) -> std::path::PathBuf {
        let marker = "full output saved to ";
        let start = output.find(marker).expect("spill marker") + marker.len();
        let rest = &output[start..];
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        std::path::PathBuf::from(&rest[..end])
    }
    let p0 = spill_path(&finished[0].1);
    let p1 = spill_path(&finished[1].1);
    assert_ne!(p0, p1, "spill paths must differ");
    let c0 = std::fs::read_to_string(&p0).unwrap();
    let c1 = std::fs::read_to_string(&p1).unwrap();
    assert!(c0.contains("one"));
    assert!(c1.contains("two"));
    assert!(!c0.contains("two"));
    assert!(!c1.contains("one"));
}

#[tokio::test]
async fn large_tool_output_is_spilled_to_a_file() {
    let dir = tempfile::tempdir().unwrap();
    let big = "x".repeat(20_000);
    let provider = MockProvider::new(vec![
        Script::tool_call("c1", "echo", json!({"text": big})),
        Script::text("ok"),
    ]);
    let mut agent = agent(provider, Mode::Auto, Arc::new(NonInteractive), dir.path());
    let (_, events) = run(&mut agent, "go").await;
    let (output, _) = &finished_outputs(&events)[0];
    assert!(
        output.contains("full output saved to"),
        "{}",
        &output[..200.min(output.len())]
    );
    assert!(output.len() < 12_000);
}

use common::{ApproveForSession, DenyWith, agent_with_sandbox};

#[tokio::test]
async fn an_approved_rerun_runs_outside_the_sandbox() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call("c1", "boxed", json!({})),
        Script::text("ok"),
    ]);
    let mut agent = agent_with_sandbox(provider, Mode::Auto, Arc::new(AlwaysApprove), dir.path());
    let (_, events) = run(&mut agent, "go").await;
    // The denial is a heuristic guess, so the prompt must not state it as fact.
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::ApprovalNeeded { reason, .. }
                if reason == "the sandbox may have blocked this command; run it again without the sandbox?"
        )),
        "{events:?}"
    );
    assert_eq!(
        finished_outputs(&events),
        vec![("ran without the sandbox".to_string(), false)]
    );
}

#[tokio::test]
async fn headless_sandbox_denials_are_blocked_not_rerun() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call("c1", "boxed", json!({})),
        Script::text("ok"),
    ]);
    let mut agent = agent_with_sandbox(provider, Mode::Auto, Arc::new(NonInteractive), dir.path());
    let (reason, events) = run(&mut agent, "go").await;
    assert_eq!(reason, TurnEndReason::Completed);
    const BLOCKED: &str = "the sandbox may have blocked this command and no user is available to approve running it without the sandbox";
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::ActionBlocked { id, reason } if id == "c1" && reason == BLOCKED
        )),
        "{events:?}"
    );
    let (output, is_error) = &finished_outputs(&events)[0];
    assert!(
        *is_error && output.contains("Could not resolve host") && output.contains(BLOCKED),
        "{output}"
    );
}

#[tokio::test]
async fn a_declined_rerun_keeps_the_sandboxed_result() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call("c1", "boxed", json!({})),
        Script::text("ok"),
    ]);
    let mut agent = agent_with_sandbox(
        provider,
        Mode::Auto,
        Arc::new(DenyWith("keep it sandboxed")),
        dir.path(),
    );
    let (_, events) = run(&mut agent, "go").await;
    let (output, _) = &finished_outputs(&events)[0];
    assert!(
        output.contains("declined") && output.contains("keep it sandboxed"),
        "{output}"
    );
}

#[tokio::test]
async fn approve_for_session_skips_later_prompts_for_the_same_action() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call("c1", "touch", json!({"path": "a.txt"})),
        Script::tool_call("c2", "touch", json!({"path": "a.txt"})),
        Script::text("ok"),
    ]);
    let mut agent = agent(provider, Mode::Ask, Arc::new(ApproveForSession), dir.path());
    let (_, events) = run(&mut agent, "go").await;
    let prompts = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::ApprovalNeeded { .. }))
        .count();
    assert_eq!(prompts, 1, "the second identical write must not prompt");
}

#[tokio::test]
async fn read_only_modes_report_a_sandbox_denial_instead_of_offering_a_rerun() {
    for mode in [Mode::Plan, Mode::ReadOnly] {
        let dir = tempfile::tempdir().unwrap();
        let provider = MockProvider::new(vec![
            Script::tool_call("c1", "boxed", json!({})),
            Script::text("ok"),
        ]);
        let mut agent = agent_with_sandbox(provider, mode, Arc::new(AlwaysApprove), dir.path());
        let (reason, events) = run(&mut agent, "go").await;
        assert_eq!(reason, TurnEndReason::Completed, "{mode}");
        assert!(
            !events.iter().any(|e| matches!(
                e,
                AgentEvent::ApprovalNeeded { .. } | AgentEvent::ActionBlocked { .. }
            )),
            "{mode}: {events:?}"
        );
        let (output, is_error) = &finished_outputs(&events)[0];
        assert!(
            *is_error
                && output.contains("Could not resolve host")
                && output.contains("read-only sandbox"),
            "{mode}: {output}"
        );
    }
}

#[tokio::test]
async fn a_guard_blocked_command_is_reported_blocked_and_never_rerun() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call("c1", "guard_blocked", json!({})),
        Script::text("ok"),
    ]);
    // An approver that would approve anything: it must never be asked, because a guard-blocked
    // result offers no re-run to approve.
    let mut agent = agent_with_sandbox(provider, Mode::Auto, Arc::new(AlwaysApprove), dir.path());
    let (reason, events) = run(&mut agent, "go").await;
    assert_eq!(reason, TurnEndReason::Completed);

    const REASON: &str = "the sandbox's git-metadata guard undid changes this command made";
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::ActionBlocked { id, reason } if id == "c1" && reason == REASON
        )),
        "{events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::ApprovalNeeded { .. })),
        "an approver must never be asked about a guard-blocked result: {events:?}"
    );

    let (output, is_error) = &finished_outputs(&events)[0];
    assert!(
        *is_error && output.contains("the sandbox's git-metadata guard undid changes"),
        "{output}"
    );

    let log = std::fs::read_to_string(dir.path().join("guard_blocked_calls.log")).unwrap();
    assert_eq!(
        log, "sandboxed\n",
        "the tool must run exactly once, and never with ctx.unsandboxed"
    );
}

#[tokio::test]
async fn the_system_prompt_and_tools_are_byte_identical_across_turns() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call("c1", "echo", json!({"text": "x"})),
        Script::text("one"),
        Script::text("two"),
    ]);
    let mut agent = agent(
        provider.clone(),
        Mode::Auto,
        Arc::new(NonInteractive),
        dir.path(),
    );
    run(&mut agent, "first").await;
    run(&mut agent, "second").await;
    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    for request in &requests[1..] {
        assert_eq!(request.system, requests[0].system);
        assert_eq!(
            serde_json::to_string(&request.tools).unwrap(),
            serde_json::to_string(&requests[0].tools).unwrap()
        );
    }
}

#[tokio::test]
async fn a_mode_change_is_appended_as_a_message_and_the_prompt_stays() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::text("planned nothing yet"),
        Script::tool_call("c1", "touch", json!({"path": "new.txt"})),
        Script::text("could not write"),
    ]);
    let mut agent = agent(
        provider.clone(),
        Mode::Auto,
        Arc::new(NonInteractive),
        dir.path(),
    );
    run(&mut agent, "first").await;
    agent.set_mode(Mode::Plan);
    let (_, events) = run(&mut agent, "second").await;

    let requests = provider.requests();
    assert_eq!(requests[1].system, requests[0].system);
    let note = requests[1]
        .messages
        .iter()
        .find_map(|m| match m {
            Message::User { content } if content.contains("approval mode is now plan") => {
                Some(content.clone())
            }
            _ => None,
        })
        .expect("the mode change is in the conversation");
    assert!(note.starts_with("[harness]"), "{note}");
    // The new mode is enforced: plan mode refuses the write.
    assert!(!dir.path().join("new.txt").exists());
    let (output, is_error) = &finished_outputs(&events)[0];
    assert!(*is_error && output.contains("plan mode"), "{output}");
}
