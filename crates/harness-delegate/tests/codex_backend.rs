mod codex_support;
mod common;

use std::{ffi::OsString, sync::Arc, time::Duration};

use codex_support::check_client_line;
use common::{Fake, collect, finished, texts};
use harness_core::{
    event::{ErrorKind, TurnEndReason},
    permission::Mode,
    redact::Redactor,
};
use harness_delegate::{
    DelegateBackend, DelegateError, StartRequest,
    codex::{CodexBackend, argv},
    env::{ApiKey, AuthMode},
    event::{ApprovalAnswer, DelegateEvent},
};
use serde_json::Value;

const LIMIT: Duration = Duration::from_secs(10);
const VERSION: &str = "codex-cli 0.154.0";
const THREAD_RESULT: &str = r#"{"approvalPolicy":"untrusted","approvalsReviewer":"user","cwd":"/work","model":"gpt-5-codex","modelProvider":"openai","sandbox":{"type":"workspaceWrite"},"thread":{"cliVersion":"0.154.0","createdAt":1,"cwd":"/work","ephemeral":false,"id":"th-1","modelProvider":"openai","preview":"","projectId":null,"sessionId":"s1","source":"appServer","status":{"type":"idle"},"turns":[],"updatedAt":1}}"#;
const LIMITS: &str = r#"{"rateLimits":{"limitId":"codex","planType":"plus","primary":{"usedPercent":12,"windowDurationMins":300,"resetsAt":1800},"secondary":{"usedPercent":3,"windowDurationMins":10080,"resetsAt":90000}}}"#;
const TURN_STARTED: &str = r#"{"turn":{"id":"turn-1","items":[],"status":"inProgress"}}"#;

fn completed(status: &str) -> String {
    format!(
        r#"{{"method":"turn/completed","params":{{"threadId":"th-1","turn":{{"id":"turn-1","items":[],"status":"{status}"}}}}}}"#
    )
}

fn message(text: &str) -> String {
    format!(
        r#"{{"method":"item/completed","params":{{"item":{{"type":"agentMessage","id":"m1","text":"{text}"}},"completedAtMs":2,"threadId":"th-1","turnId":"turn-1"}}}}"#
    )
}

fn command_request(id: u64) -> String {
    format!(
        r#"{{"id":{id},"method":"item/commandExecution/requestApproval","params":{{"itemId":"i1","threadId":"th-1","turnId":"turn-1","startedAtMs":1,"command":"cargo test","cwd":"/work","reason":"run tests"}}}}"#
    )
}

/// The script up to a started turn: what harness must write, in order, and what the server says.
fn handshake() -> String {
    format!(
        "expect \"method\":\"initialize\"\nemit {{\"id\":1,\"result\":{{\"userAgent\":\"codex/0.154.0\"}}}}\n\
         expect \"method\":\"initialized\"\nexpect account/rateLimits/read\nexpect \"method\":\"thread/start\"\n\
         emit {{\"id\":2,\"result\":{LIMITS}}}\nemit {{\"id\":3,\"result\":{THREAD_RESULT}}}\n\
         expect \"method\":\"turn/start\"\nemit {{\"id\":4,\"result\":{TURN_STARTED}}}\n"
    )
}

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

fn parent(extra: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
    let mut env = vec![
        ("PATH".into(), std::env::var_os("PATH").unwrap_or_default()),
        ("KEEP_ME".into(), "1".into()),
        ("OPENAI_API_KEY".into(), "sk-openai-from-parent".into()),
        ("CODEX_API_KEY".into(), "codex-from-parent".into()),
        ("ANTHROPIC_API_KEY".into(), "sk-ant-from-parent".into()),
    ];
    env.extend(extra.iter().map(|(k, v)| ((*k).into(), (*v).into())));
    env
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

/// Every line the child received is valid against the generated schema.
fn all_stdin_valid(fake: &Fake) {
    for line in fake.stdin_lines() {
        check_client_line(&line, Some("CommandExecutionRequestApprovalResponse.json"));
    }
}

#[test]
fn the_command_line_is_app_server_on_stdio() {
    let a: Vec<String> = argv::build(&[])
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(a, ["app-server", "--listen", "stdio://"]);
}

#[test]
fn the_users_mcp_servers_are_switched_off_by_name_with_a_toml_table() {
    let names = ["alpha".to_string(), "we.ird \"name\"".to_string()];
    let a: Vec<String> = argv::build(&names)
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let i = a.iter().position(|x| x == "-c").expect("a -c override");
    assert_eq!(
        a[i + 1],
        r#"mcp_servers={"alpha"={enabled=false},"we.ird \"name\""={enabled=false}}"#
    );
    assert_eq!(a[0], "app-server", "the subcommand comes first");
}

#[tokio::test]
async fn the_servers_codex_lists_for_the_user_are_disabled_on_the_command_line() {
    let script = format!("{}emit {}\n", handshake(), completed("completed"));
    let fake = Fake::new("codex", VERSION, &script);
    std::fs::write(
        fake.path().join("mcp.json"),
        r#"[{"name":"alpha","enabled":true},{"name":"beta","enabled":false}]"#,
    )
    .unwrap();
    let mut run = CodexBackend.start(req(&fake)).await.unwrap();
    collect(&mut run.events, LIMIT).await;
    let a = fake.argv();
    let i = a.iter().position(|x| x == "-c").expect("a -c override");
    assert_eq!(
        a[i + 1],
        r#"mcp_servers={"alpha"={enabled=false},"beta"={enabled=false}}"#
    );
}

#[tokio::test]
async fn with_no_user_mcp_servers_there_is_no_override() {
    let script = format!("{}emit {}\n", handshake(), completed("completed"));
    let fake = Fake::new("codex", VERSION, &script);
    let mut run = CodexBackend.start(req(&fake)).await.unwrap();
    collect(&mut run.events, LIMIT).await;
    assert!(!fake.argv().contains(&"-c".to_string()));
}

#[tokio::test]
async fn a_full_run_relays_the_events_and_every_line_written_is_valid() {
    let script = format!(
        "{}emit {}\nemit {}\n",
        handshake(),
        message("Done"),
        completed("completed")
    );
    let fake = Fake::new("codex", VERSION, &script);
    let mut run = CodexBackend.start(req(&fake)).await.unwrap();
    let events = collect(&mut run.events, LIMIT).await;
    assert_eq!(
        finished(&events),
        Some(TurnEndReason::Completed),
        "{events:?}"
    );
    assert_eq!(texts(&events), ["Done"]);
    assert!(matches!(
        &events[0],
        DelegateEvent::RateLimits { .. } | DelegateEvent::Started { .. }
    ));
    assert!(events.iter().any(|e| matches!(e, DelegateEvent::Started { session_id, child_version, .. } if session_id == "th-1" && child_version == "0.154.0")));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, DelegateEvent::RateLimits { plan: Some(p), .. } if p == "plus")),
        "the limits were read at the start"
    );
    assert_eq!(run.control.session_id().as_deref(), Some("th-1"));
    all_stdin_valid(&fake);
    assert_eq!(fake.argv(), ["app-server", "--listen", "stdio://"]);
    assert!(!fake.argv().iter().any(|a| a.contains("fix the bug")));
    let turn: Value = serde_json::from_str(
        fake.stdin_lines()
            .iter()
            .find(|l| l.contains("turn/start"))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(turn["params"]["input"][0]["text"], "fix the bug");
}

#[tokio::test]
async fn an_exec_approval_round_trip() {
    let script = format!(
        "{}emit {}\nexpect \"decision\":\"accept\"\nemit {}\n",
        handshake(),
        command_request(7),
        completed("completed")
    );
    let fake = Fake::new("codex", VERSION, &script);
    let mut run = CodexBackend.start(req(&fake)).await.unwrap();
    let id = until_approval(&mut run).await;
    assert_eq!(id, "7");
    run.control
        .answer(&id, ApprovalAnswer::AllowOnce)
        .await
        .unwrap();
    let events = collect(&mut run.events, LIMIT).await;
    assert_eq!(
        finished(&events),
        Some(TurnEndReason::Completed),
        "{events:?}"
    );
    all_stdin_valid(&fake);
}

#[tokio::test]
async fn a_patch_approval_round_trip_and_a_deny() {
    let file_request = r#"{"id":"p1","method":"item/fileChange/requestApproval","params":{"itemId":"f1","threadId":"th-1","turnId":"turn-1","startedAtMs":1}}"#;
    let script = format!(
        "{}emit {file_request}\nexpect \"decision\":\"decline\"\nemit {}\n",
        handshake(),
        completed("completed")
    );
    let fake = Fake::new("codex", VERSION, &script);
    let mut run = CodexBackend.start(req(&fake)).await.unwrap();
    let id = until_approval(&mut run).await;
    assert_eq!(id, "p1");
    run.control
        .answer(
            &id,
            ApprovalAnswer::Deny {
                reason: Some("no".into()),
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
    let answer: Value = serde_json::from_str(&fake.stdin_lines().last().unwrap().clone()).unwrap();
    assert_eq!(answer["id"], "p1");
}

#[tokio::test]
async fn a_cancelled_approval_writes_cancel_and_interrupts_the_turn() {
    let script = format!(
        "{}emit {}\nexpect \"decision\":\"cancel\"\nexpect turn/interrupt\nemit {}\n",
        handshake(),
        command_request(9),
        completed("interrupted")
    );
    let fake = Fake::new("codex", VERSION, &script);
    let mut run = CodexBackend.start(req(&fake)).await.unwrap();
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
    all_stdin_valid(&fake);
}

#[tokio::test]
async fn an_interrupt_is_turn_interrupt_and_a_cooperating_child_ends_the_turn() {
    let script = format!(
        "{}expect turn/interrupt\nemit {}\n",
        handshake(),
        completed("interrupted")
    );
    let fake = Fake::new("codex", VERSION, &script);
    let mut run = CodexBackend.start(req(&fake)).await.unwrap();
    while !matches!(run.events.recv().await, Some(DelegateEvent::Started { .. })) {}
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
async fn a_child_that_ignores_the_interrupt_is_ended_by_a_signal() {
    let script = format!("{}hang\n", handshake());
    let fake = Fake::new("codex", VERSION, &script);
    let mut run = CodexBackend.start(req(&fake)).await.unwrap();
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
async fn a_crash_mid_stream_is_an_error_naming_codex_and_its_stderr() {
    let script = format!("{}stderr thread panicked: boom\nexit 101\n", handshake());
    let fake = Fake::new("codex", VERSION, &script);
    let mut run = CodexBackend.start(req(&fake)).await.unwrap();
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
        message.contains("codex") && message.contains("boom") && message.contains("101"),
        "{message}"
    );
}

#[tokio::test]
async fn a_malformed_line_is_a_warning_and_the_run_goes_on() {
    let script = format!(
        "{}emit {{broken\nemit {}\n",
        handshake(),
        completed("completed")
    );
    let fake = Fake::new("codex", VERSION, &script);
    let mut run = CodexBackend.start(req(&fake)).await.unwrap();
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
    let fake = Fake::new("codex", "codex-cli 0.160.0", "expect x\n");
    let err = CodexBackend.start(req(&fake)).await.err().expect("refused");
    assert!(
        matches!(err, DelegateError::VersionOutOfRange { .. }),
        "{err}"
    );
    assert!(
        err.to_string().contains("0.160.0") && err.to_string().contains("0.154"),
        "{err}"
    );
    assert!(fake.log().is_empty());
}

#[tokio::test]
async fn full_access_is_refused_unconfirmed_and_goes_on_the_wire_when_confirmed() {
    let fake = Fake::new("codex", VERSION, "expect x\n");
    let mut r = req(&fake);
    r.mode = Mode::FullAccess;
    let err = CodexBackend.start(r).await.err().expect("refused");
    assert!(matches!(err, DelegateError::FullAccessUnconfirmed));
    assert!(fake.log().is_empty());

    let script = format!("{}emit {}\n", handshake(), completed("completed"));
    let fake = Fake::new("codex", VERSION, &script);
    let mut r = req(&fake);
    r.mode = Mode::FullAccess;
    r.full_access_confirmed = true;
    let mut run = CodexBackend.start(r).await.unwrap();
    collect(&mut run.events, LIMIT).await;
    let lines = fake.stdin_lines();
    let thread: Value =
        serde_json::from_str(lines.iter().find(|l| l.contains("thread/start")).unwrap()).unwrap();
    assert_eq!(thread["params"]["sandbox"], "danger-full-access");
    assert_eq!(thread["params"]["approvalPolicy"], "never");
    let turn: Value =
        serde_json::from_str(lines.iter().find(|l| l.contains("turn/start")).unwrap()).unwrap();
    assert_eq!(turn["params"]["sandboxPolicy"]["type"], "dangerFullAccess");
}

#[tokio::test]
async fn a_plan_run_is_read_only_on_the_wire() {
    let script = format!("{}emit {}\n", handshake(), completed("completed"));
    let fake = Fake::new("codex", VERSION, &script);
    let mut r = req(&fake);
    r.mode = Mode::Plan;
    let mut run = CodexBackend.start(r).await.unwrap();
    collect(&mut run.events, LIMIT).await;
    let turn: Value = serde_json::from_str(
        fake.stdin_lines()
            .iter()
            .find(|l| l.contains("turn/start"))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(turn["params"]["sandboxPolicy"]["type"], "readOnly");
    assert_eq!(turn["params"]["approvalPolicy"], "on-request");
}

#[tokio::test]
async fn resume_and_model_go_on_the_wire() {
    let script = format!(
        "{}emit {}\n",
        handshake().replace(
            "\"method\":\"thread/start\"",
            "\"method\":\"thread/resume\""
        ),
        completed("completed")
    );
    let fake = Fake::new("codex", VERSION, &script);
    let mut r = req(&fake);
    r.resume = Some("th-old".into());
    r.model = Some("gpt-5-codex".into());
    let mut run = CodexBackend.start(r).await.unwrap();
    let events = collect(&mut run.events, LIMIT).await;
    assert_eq!(
        finished(&events),
        Some(TurnEndReason::Completed),
        "{events:?}"
    );
    let resume: Value = serde_json::from_str(
        fake.stdin_lines()
            .iter()
            .find(|l| l.contains("thread/resume"))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(resume["params"]["threadId"], "th-old");
    assert_eq!(resume["params"]["model"], "gpt-5-codex");
}

#[tokio::test]
async fn subscription_mode_scrubs_the_childs_environment() {
    let script = format!("{}emit {}\n", handshake(), completed("completed"));
    let fake = Fake::new("codex", VERSION, &script);
    let mut r = req(&fake);
    r.parent_env = Some(parent(&[]));
    let mut run = CodexBackend.start(r).await.unwrap();
    collect(&mut run.events, LIMIT).await;
    let env = fake.env();
    assert_eq!(env.get("KEEP_ME").map(String::as_str), Some("1"));
    for gone in ["OPENAI_API_KEY", "CODEX_API_KEY", "ANTHROPIC_API_KEY"] {
        assert!(!env.contains_key(gone), "{gone} reached the child: {env:?}");
    }
}

#[tokio::test]
async fn api_key_mode_passes_the_key_in_the_environment_only() {
    let script = format!("{}emit {}\n", handshake(), completed("completed"));
    let fake = Fake::new("codex", VERSION, &script);
    let mut r = req(&fake);
    r.parent_env = Some(parent(&[]));
    r.auth = AuthMode::ApiKey(ApiKey::new("sk-openai-callers-key"));
    let mut run = CodexBackend.start(r).await.unwrap();
    collect(&mut run.events, LIMIT).await;
    let env = fake.env();
    assert_eq!(
        env.get("OPENAI_API_KEY").map(String::as_str),
        Some("sk-openai-callers-key")
    );
    assert!(!env.contains_key("CODEX_API_KEY") && !env.contains_key("ANTHROPIC_API_KEY"));
    assert!(!fake.argv().iter().any(|a| a.contains("sk-openai")));
    assert!(!fake.stdin_lines().iter().any(|l| l.contains("sk-openai")));
}

#[tokio::test]
async fn what_the_child_relays_is_redacted_with_the_callers_redactor() {
    let secret = "sk-live-0123456789abcdef";
    let script = format!(
        "{}emit {}\nemit {}\n",
        handshake(),
        message(&format!("the key is {secret}")),
        completed("completed")
    );
    let fake = Fake::new("codex", VERSION, &script);
    let redactor = Arc::new(Redactor::default());
    redactor.add(secret);
    let mut r = req(&fake);
    r.redactor = redactor;
    let mut run = CodexBackend.start(r).await.unwrap();
    let events = collect(&mut run.events, LIMIT).await;
    let all = serde_json::to_string(&events).unwrap();
    assert!(!all.contains(secret), "{all}");
    assert!(all.contains("[redacted]"));
}

#[tokio::test]
async fn the_child_runs_in_the_workspace() {
    let script = format!("{}emit {}\n", handshake(), completed("completed"));
    let fake = Fake::new("codex", VERSION, &script);
    let dir = tempfile::tempdir().unwrap();
    let mut r = req(&fake);
    r.workspace = dir.path().to_path_buf();
    let mut run = CodexBackend.start(r).await.unwrap();
    collect(&mut run.events, LIMIT).await;
    let want = std::fs::canonicalize(dir.path()).unwrap();
    assert_eq!(
        fake.env()
            .get("PWD")
            .map(|p| std::fs::canonicalize(p).unwrap()),
        Some(want)
    );
}
