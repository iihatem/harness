mod common;

use std::{ffi::OsString, time::Duration};

use common::{Fake, collect, finished, texts};
use harness_core::{
    event::{ErrorKind, TurnEndReason},
    permission::Mode,
    redact::Redactor,
};
use harness_delegate::{
    DelegateBackend, DelegateError, StartRequest,
    claude::{ClaudeBackend, argv},
    env::{ApiKey, AuthMode},
    event::{ApprovalAnswer, DelegateEvent},
};
use serde_json::Value;
use std::sync::Arc;

const LIMIT: Duration = Duration::from_secs(10);
const INIT: &str = r#"{"type":"system","subtype":"init","apiKeySource":"none","claude_code_version":"2.1.312","model":"claude-x","session_id":"sess-9"}"#;
const RESULT_OK: &str = r#"{"type":"result","subtype":"success","is_error":false,"total_cost_usd":0.25,"result":"ok","usage":{},"modelUsage":{},"permission_denials":[],"session_id":"sess-9"}"#;
const VERSION: &str = "2.1.312 (Claude Code)";

fn req(fake: &Fake) -> StartRequest {
    let mut r = StartRequest::new("fix the bug", fake.path());
    r.binary = Some(fake.binary.clone());
    r.grace = Duration::from_millis(150);
    r.parent_env = Some(vec![(
        "PATH".into(),
        std::env::var_os("PATH").unwrap_or_default(),
    )]);
    r
}

fn args(r: &StartRequest) -> Vec<String> {
    argv::build(r)
        .unwrap()
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

fn has_pair(args: &[String], flag: &str, value: &str) -> bool {
    args.windows(2).any(|w| w[0] == flag && w[1] == value)
}

#[test]
fn the_fixed_part_of_the_command_line() {
    let r = StartRequest::new("t", "/w");
    let a = args(&r);
    assert_eq!(a[0], "-p");
    assert!(has_pair(&a, "--input-format", "stream-json"));
    assert!(has_pair(&a, "--output-format", "stream-json"));
    assert!(a.contains(&"--verbose".to_string()));
    assert!(a.contains(&"--include-partial-messages".to_string()));
    assert!(has_pair(&a, "--permission-prompt-tool", "stdio"));
    assert!(
        a.contains(&"--setting-sources=".to_string()),
        "no user, project or local settings: {a:?}"
    );
    assert!(a.contains(&"--strict-mcp-config".to_string()));
    assert!(
        !a.contains(&"--bare".to_string()),
        "--bare cannot read a subscription login"
    );
    let settings = a
        .iter()
        .position(|x| x == "--settings")
        .map(|i| &a[i + 1])
        .unwrap();
    let v: Value = serde_json::from_str(settings).unwrap();
    assert_eq!(v["permissions"]["allow"], serde_json::json!([]));
    assert_eq!(v["permissions"]["deny"], serde_json::json!([]));
    assert_eq!(v["permissions"]["ask"], serde_json::json!([]));
    assert!(
        !a.iter().any(|x| x == "t"),
        "the task is never on the command line"
    );
}

#[test]
fn each_mode_has_its_flags() {
    let mut r = StartRequest::new("t", "/w");
    for (mode, flags) in [
        (Mode::Plan, vec!["--permission-mode", "plan"]),
        (Mode::Ask, vec!["--permission-mode", "manual"]),
        (Mode::Auto, vec!["--permission-mode", "acceptEdits"]),
    ] {
        r.mode = mode;
        let a = args(&r);
        assert!(has_pair(&a, flags[0], flags[1]), "{mode}: {a:?}");
        assert!(!a.contains(&"--restricted".to_string()));
        assert!(!a.contains(&"--allow-dangerously-skip-permissions".to_string()));
    }
    r.mode = Mode::ReadOnly;
    let a = args(&r);
    assert!(a.contains(&"--restricted".to_string()));
    assert!(has_pair(&a, "--tools", "Read,Glob,Grep"));
    assert!(has_pair(&a, "--permission-mode", "manual"));
    r.mode = Mode::FullAccess;
    r.full_access_confirmed = true;
    let a = args(&r);
    assert!(has_pair(&a, "--permission-mode", "bypassPermissions"));
    assert!(a.contains(&"--allow-dangerously-skip-permissions".to_string()));
}

#[test]
fn full_access_is_refused_without_the_confirmation() {
    let mut r = StartRequest::new("t", "/w");
    r.mode = Mode::FullAccess;
    assert!(matches!(
        argv::build(&r),
        Err(DelegateError::FullAccessUnconfirmed)
    ));
}

#[test]
fn model_resume_and_extra_directories_are_passed_and_the_directories_come_last() {
    let mut r = StartRequest::new("t", "/w");
    r.model = Some("opus".into());
    r.resume = Some("sess-1".into());
    r.extra_dirs = vec!["/a".into(), "/b".into()];
    let a = args(&r);
    assert!(has_pair(&a, "--model", "opus"));
    assert!(has_pair(&a, "--resume", "sess-1"));
    let i = a.iter().position(|x| x == "--add-dir").unwrap();
    assert_eq!(
        &a[i..],
        ["--add-dir", "/a", "/b"],
        "a variadic flag must be last"
    );
}

#[tokio::test]
async fn a_full_run_writes_the_task_on_stdin_and_relays_the_events() {
    let script = format!(
        "expect fix the bug\nemit {INIT}\nemit {{\"type\":\"assistant\",\"message\":{{\"id\":\"m\",\"model\":\"claude-x\",\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"Hi\"}}]}}}}\nemit {RESULT_OK}\n"
    );
    let fake = Fake::new("claude", VERSION, &script);
    let mut run = ClaudeBackend.start(req(&fake)).await.unwrap();
    let events = collect(&mut run.events, LIMIT).await;
    assert_eq!(
        finished(&events),
        Some(TurnEndReason::Completed),
        "{events:?}"
    );
    assert_eq!(texts(&events), ["Hi"]);
    assert!(
        matches!(&events[0], DelegateEvent::Started { session_id, child_version, .. } if session_id == "sess-9" && child_version == "2.1.312")
    );
    assert_eq!(run.control.session_id().as_deref(), Some("sess-9"));
    let argv = fake.argv();
    assert!(!argv.iter().any(|a| a.contains("fix the bug")));
    assert_eq!(fake.stdin_lines().len(), 1);
    assert!(fake.stdin_lines()[0].contains("fix the bug"));
}

fn approval_script(expect: &str) -> String {
    format!(
        "expect fix the bug\nemit {INIT}\n\
         emit {{\"type\":\"control_request\",\"request_id\":\"req_1\",\"request\":{{\"subtype\":\"can_use_tool\",\"tool_name\":\"Bash\",\"input\":{{\"command\":\"ls\"}},\"tool_use_id\":\"toolu_1\"}}}}\n\
         expect {expect}\nemit {RESULT_OK}\n"
    )
}

async fn until_approval(run: &mut harness_delegate::DelegateRun) -> String {
    loop {
        match tokio::time::timeout(LIMIT, run.events.recv())
            .await
            .unwrap()
        {
            Some(DelegateEvent::ApprovalNeeded(a)) => return a.id,
            Some(_) => {}
            None => panic!("the stream ended before an approval"),
        }
    }
}

#[tokio::test]
async fn an_approval_round_trip_allows_the_call() {
    let fake = Fake::new(
        "claude",
        VERSION,
        &approval_script("\"behavior\":\"allow\""),
    );
    let mut run = ClaudeBackend.start(req(&fake)).await.unwrap();
    let id = until_approval(&mut run).await;
    assert_eq!(id, "req_1");
    run.control
        .answer(&id, ApprovalAnswer::AllowOnce)
        .await
        .unwrap();
    let events = collect(&mut run.events, LIMIT).await;
    assert_eq!(
        finished(&events),
        Some(TurnEndReason::Completed),
        "the child saw an allow: {events:?}"
    );
    let sent: Value = serde_json::from_str(&fake.stdin_lines()[1]).unwrap();
    assert_eq!(sent["response"]["request_id"], "req_1");
}

#[tokio::test]
async fn a_denied_approval_tells_the_child_why() {
    let fake = Fake::new("claude", VERSION, &approval_script("not here"));
    let mut run = ClaudeBackend.start(req(&fake)).await.unwrap();
    let id = until_approval(&mut run).await;
    run.control
        .answer(
            &id,
            ApprovalAnswer::Deny {
                reason: Some("not here".into()),
            },
        )
        .await
        .unwrap();
    let events = collect(&mut run.events, LIMIT).await;
    assert_eq!(
        finished(&events),
        Some(TurnEndReason::Completed),
        "{events:?}"
    );
}

#[tokio::test]
async fn a_cancelled_approval_ends_the_run_as_interrupted() {
    // The child gets the deny-with-interrupt and does not stop by itself: the ladder ends it.
    let script = format!(
        "expect fix the bug\nemit {INIT}\n\
        emit {{\"type\":\"control_request\",\"request_id\":\"req_1\",\"request\":{{\"subtype\":\"can_use_tool\",\"tool_name\":\"Bash\",\"input\":{{}},\"tool_use_id\":\"t\"}}}}\n\
        expect interrupt\nhang\n"
    );
    let fake = Fake::new("claude", VERSION, &script);
    let mut run = ClaudeBackend.start(req(&fake)).await.unwrap();
    let id = until_approval(&mut run).await;
    run.control
        .answer(&id, ApprovalAnswer::Cancel)
        .await
        .unwrap();
    let events = collect(&mut run.events, LIMIT).await;
    assert_eq!(
        finished(&events),
        Some(TurnEndReason::Interrupted),
        "{events:?}"
    );
}

#[tokio::test]
async fn an_interrupt_is_sent_as_a_control_request_and_a_cooperating_child_ends_the_turn() {
    let script = format!(
        "expect fix the bug\nemit {INIT}\nexpect interrupt\n\
        emit {{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"is_error\":true,\"total_cost_usd\":0.0,\"result\":\"\",\"errors\":[],\"usage\":{{}},\"modelUsage\":{{}},\"permission_denials\":[],\"session_id\":\"sess-9\"}}\n"
    );
    let fake = Fake::new("claude", VERSION, &script);
    let mut run = ClaudeBackend.start(req(&fake)).await.unwrap();
    loop {
        if let Some(DelegateEvent::Started { .. }) = run.events.recv().await {
            break;
        }
    }
    run.control.interrupt().unwrap();
    let events = collect(&mut run.events, LIMIT).await;
    assert_eq!(
        finished(&events),
        Some(TurnEndReason::Interrupted),
        "{events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, DelegateEvent::Error { .. }))
    );
}

#[tokio::test]
async fn a_child_that_ignores_the_interrupt_is_ended_by_the_signal_ladder() {
    let script = format!("expect fix the bug\nemit {INIT}\nhang\n");
    let fake = Fake::new("claude", VERSION, &script);
    let mut run = ClaudeBackend.start(req(&fake)).await.unwrap();
    while !matches!(run.events.recv().await, Some(DelegateEvent::Started { .. })) {}
    run.control.interrupt().unwrap();
    let events = collect(&mut run.events, LIMIT).await;
    assert_eq!(
        finished(&events),
        Some(TurnEndReason::Interrupted),
        "{events:?}"
    );
}

#[tokio::test]
async fn a_crash_mid_stream_is_an_error_naming_claude_and_its_stderr() {
    let script = format!("expect fix the bug\nemit {INIT}\nstderr fatal: it fell over\nexit 2\n");
    let fake = Fake::new("claude", VERSION, &script);
    let mut run = ClaudeBackend.start(req(&fake)).await.unwrap();
    let events = collect(&mut run.events, LIMIT).await;
    assert_eq!(finished(&events), Some(TurnEndReason::Error));
    let message = events.iter().find_map(|e| match e {
        DelegateEvent::Error {
            kind: ErrorKind::Internal,
            message,
        } => Some(message.clone()),
        _ => None,
    });
    let message = message.expect("an error");
    assert!(
        message.contains("claude") && message.contains("fell over"),
        "{message}"
    );
}

#[tokio::test]
async fn a_malformed_line_is_a_warning_and_the_run_goes_on() {
    let script =
        format!("expect fix the bug\nemit {INIT}\nemit {{this is not json\nemit {RESULT_OK}\n");
    let fake = Fake::new("claude", VERSION, &script);
    let mut run = ClaudeBackend.start(req(&fake)).await.unwrap();
    let events = collect(&mut run.events, LIMIT).await;
    assert!(
        events.iter().any(
            |e| matches!(e, DelegateEvent::Warning { message } if message.contains("not JSON"))
        )
    );
    assert_eq!(finished(&events), Some(TurnEndReason::Completed));
}

#[tokio::test]
async fn a_version_outside_the_range_is_refused_before_anything_runs() {
    let fake = Fake::new("claude", "1.0.0 (Claude Code)", "expect x\n");
    let err = ClaudeBackend
        .start(req(&fake))
        .await
        .err()
        .expect("refused");
    assert!(
        matches!(err, DelegateError::VersionOutOfRange { .. }),
        "{err}"
    );
    assert!(
        fake.log().is_empty(),
        "the child was never run with arguments"
    );
    let mut r = req(&fake);
    r.allow_untested = true;
    let script = format!("expect fix the bug\nemit {INIT}\nemit {RESULT_OK}\n");
    let fake = Fake::new("claude", "1.0.0 (Claude Code)", &script);
    r.binary = Some(fake.binary.clone());
    let mut run = ClaudeBackend.start(r).await.unwrap();
    assert_eq!(
        finished(&collect(&mut run.events, LIMIT).await),
        Some(TurnEndReason::Completed)
    );
}

fn parent(extra: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
    let mut env = vec![
        ("PATH".into(), std::env::var_os("PATH").unwrap_or_default()),
        ("KEEP_ME".into(), "1".into()),
        ("ANTHROPIC_API_KEY".into(), "sk-ant-from-parent".into()),
        ("OPENAI_API_KEY".into(), "sk-openai-from-parent".into()),
    ];
    env.extend(extra.iter().map(|(k, v)| ((*k).into(), (*v).into())));
    env
}

#[tokio::test]
async fn subscription_mode_scrubs_the_childs_environment() {
    let script = format!("expect fix the bug\nemit {INIT}\nemit {RESULT_OK}\n");
    let fake = Fake::new("claude", VERSION, &script);
    let mut r = req(&fake);
    r.parent_env = Some(parent(&[]));
    let mut run = ClaudeBackend.start(r).await.unwrap();
    collect(&mut run.events, LIMIT).await;
    let env = fake.env();
    assert_eq!(env.get("KEEP_ME").map(String::as_str), Some("1"));
    assert!(
        !env.contains_key("ANTHROPIC_API_KEY") && !env.contains_key("OPENAI_API_KEY"),
        "{env:?}"
    );
}

#[tokio::test]
async fn api_key_mode_passes_the_key_in_the_environment_only() {
    let init = INIT.replace("\"none\"", "\"ANTHROPIC_API_KEY\"");
    let script = format!("expect fix the bug\nemit {init}\nemit {RESULT_OK}\n");
    let fake = Fake::new("claude", VERSION, &script);
    let mut r = req(&fake);
    r.parent_env = Some(parent(&[]));
    r.auth = AuthMode::ApiKey(ApiKey::new("sk-ant-callers-key"));
    let mut run = ClaudeBackend.start(r).await.unwrap();
    let events = collect(&mut run.events, LIMIT).await;
    assert_eq!(
        finished(&events),
        Some(TurnEndReason::Completed),
        "{events:?}"
    );
    assert_eq!(
        fake.env().get("ANTHROPIC_API_KEY").map(String::as_str),
        Some("sk-ant-callers-key")
    );
    assert!(
        !fake.argv().iter().any(|a| a.contains("sk-ant")),
        "never on the command line"
    );
    assert!(
        !fake.stdin_lines().iter().any(|l| l.contains("sk-ant")),
        "never on stdin"
    );
}

#[tokio::test]
async fn a_child_that_would_bill_an_api_key_in_subscription_mode_is_stopped() {
    let init = INIT.replace("\"none\"", "\"ANTHROPIC_API_KEY\"");
    let script = format!("expect fix the bug\nemit {init}\nhang\n");
    let fake = Fake::new("claude", VERSION, &script);
    let mut run = ClaudeBackend.start(req(&fake)).await.unwrap();
    let events = collect(&mut run.events, LIMIT).await;
    assert_eq!(finished(&events), Some(TurnEndReason::Error));
    assert!(events.iter().any(
        |e| matches!(e, DelegateEvent::Error { message, .. } if message.contains("subscription"))
    ));
}

#[tokio::test]
async fn what_the_child_relays_is_redacted_with_the_callers_redactor() {
    let secret = "sk-live-0123456789abcdef";
    let script = format!(
        "expect fix the bug\nemit {INIT}\n\
        emit {{\"type\":\"assistant\",\"message\":{{\"id\":\"m\",\"model\":\"claude-x\",\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"the key is {secret}\"}}]}}}}\nemit {RESULT_OK}\n"
    );
    let fake = Fake::new("claude", VERSION, &script);
    let redactor = Arc::new(Redactor::default());
    redactor.add(secret);
    let mut r = req(&fake);
    r.redactor = redactor;
    let mut run = ClaudeBackend.start(r).await.unwrap();
    let events = collect(&mut run.events, LIMIT).await;
    let all = serde_json::to_string(&events).unwrap();
    assert!(!all.contains(secret), "{all}");
    assert!(all.contains("[redacted]"));
}

#[tokio::test]
async fn the_child_runs_in_the_workspace() {
    let script = format!("expect fix the bug\nemit {INIT}\nemit {RESULT_OK}\n");
    let fake = Fake::new("claude", VERSION, &script);
    let dir = tempfile::tempdir().unwrap();
    let mut r = req(&fake);
    r.workspace = dir.path().to_path_buf();
    let mut run = ClaudeBackend.start(r).await.unwrap();
    collect(&mut run.events, LIMIT).await;
    let pwd = fake.env().get("PWD").cloned();
    // PWD is set by the shell wrapper from the real cwd.
    let want = std::fs::canonicalize(dir.path()).unwrap();
    assert_eq!(pwd.map(|p| std::fs::canonicalize(p).unwrap()), Some(want));
}
