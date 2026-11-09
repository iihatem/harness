mod codex_support;

use codex_support::{check_client_line, check_server_line};
use harness_core::{
    event::{ErrorKind, TurnEndReason},
    meter::WindowSource,
    permission::Mode,
};
use harness_delegate::{
    DelegateError,
    codex::protocol::{CodexParams, CodexProtocol, policy},
    driver::{Out, Protocol},
    event::{ApprovalAnswer, DelegateApprovalKind, DelegateEvent, FileChange, LimitStatus},
};
use serde_json::{Value, json};

const THREAD: &str = "th-1";
const TURN: &str = "turn-1";

fn params() -> CodexParams {
    CodexParams {
        clock: || 1_000,
        ..CodexParams::new("fix the bug", "/work", "0.154.0")
    }
}

// --- server messages, built to be valid against the schema -------------------------------

fn turn(status: &str) -> Value {
    json!({"id": TURN, "items": [], "status": status})
}

fn thread_result() -> Value {
    json!({
        "approvalPolicy": "untrusted", "approvalsReviewer": "user", "cwd": "/work",
        "model": "gpt-5-codex", "modelProvider": "openai", "sandbox": {"type": "workspaceWrite"},
        "thread": {"cliVersion": "0.154.0", "createdAt": 1, "cwd": "/work", "ephemeral": false,
            "id": THREAD, "modelProvider": "openai", "preview": "", "projectId": null,
            "sessionId": "sess-1", "source": "appServer", "status": {"type": "idle"},
            "turns": [], "updatedAt": 1}
    })
}

fn snapshot(primary: u64, secondary: u64) -> Value {
    json!({"limitId": "codex", "planType": "plus",
        "primary": {"usedPercent": primary, "windowDurationMins": 300, "resetsAt": 1_800},
        "secondary": {"usedPercent": secondary, "windowDurationMins": 10080, "resetsAt": 90_000}})
}

fn note(method: &str, params: Value) -> String {
    json!({"method": method, "params": params}).to_string()
}

fn item_started(item: Value) -> String {
    note(
        "item/started",
        json!({"item": item, "startedAtMs": 1, "threadId": THREAD, "turnId": TURN}),
    )
}

fn item_completed(item: Value) -> String {
    note(
        "item/completed",
        json!({"item": item, "completedAtMs": 2, "threadId": THREAD, "turnId": TURN}),
    )
}

fn command_item(status: &str, output: Option<&str>, exit: Option<i64>) -> Value {
    json!({"type": "commandExecution", "id": "item-cmd", "command": "cargo test", "commandActions": [],
        "cwd": "/work", "status": status, "aggregatedOutput": output, "exitCode": exit})
}

fn file_item(status: &str) -> Value {
    json!({"type": "fileChange", "id": "item-file", "status": status, "changes": [
        {"path": "src/a.rs", "kind": {"type": "update", "move_path": null}, "diff": "@@"},
        {"path": "src/new.rs", "kind": {"type": "add"}, "diff": "+x"},
        {"path": "src/gone.rs", "kind": {"type": "delete"}, "diff": "-x"}]})
}

fn usage_note(total: u64, last_input: u64) -> String {
    note(
        "thread/tokenUsage/updated",
        json!({"threadId": THREAD, "turnId": TURN, "tokenUsage": {
        "last": {"inputTokens": last_input, "cachedInputTokens": 40, "outputTokens": 10, "reasoningOutputTokens": 4, "totalTokens": last_input + 10},
        "total": {"inputTokens": total, "cachedInputTokens": 40, "outputTokens": 10, "reasoningOutputTokens": 4, "totalTokens": total + 10},
        "modelContextWindow": 200_000}}),
    )
}

fn command_request(id: Value, decisions: Option<Vec<&str>>) -> String {
    let mut p = json!({"itemId": "item-cmd", "threadId": THREAD, "turnId": TURN, "startedAtMs": 1, "command": "cargo test",
        "cwd": "/work", "reason": "needs to run the tests"});
    if let Some(d) = decisions {
        p["availableDecisions"] = json!(d);
    }
    json!({"id": id, "method": "item/commandExecution/requestApproval", "params": p}).to_string()
}

fn file_request(id: Value) -> String {
    json!({"id": id, "method": "item/fileChange/requestApproval", "params":
        {"itemId": "item-file", "threadId": THREAD, "turnId": TURN, "startedAtMs": 1, "reason": "write outside"}}).to_string()
}

// --- a driver around the protocol ----------------------------------------------------------

struct Run {
    p: CodexProtocol,
    writes: Vec<Value>,
    events: Vec<DelegateEvent>,
    finished: bool,
    close_stdin: bool,
    stop: bool,
    /// The method of each server request seen, by the id it will be answered with.
    asked: std::collections::HashMap<String, String>,
}

impl Run {
    fn new(params: CodexParams) -> Run {
        let mut run = Run {
            p: CodexProtocol::new(params).unwrap(),
            writes: vec![],
            events: vec![],
            finished: false,
            close_stdin: false,
            stop: false,
            asked: Default::default(),
        };
        let out = run.p.start();
        run.take(out);
        run
    }

    fn take(&mut self, out: Out) -> Vec<Value> {
        let writes: Vec<Value> = out
            .writes
            .iter()
            .map(|w| check_client_line(w, Some(self.response_schema(w))))
            .collect();
        self.writes.extend(writes.clone());
        self.events.extend(out.events);
        self.finished |= out.finished;
        self.close_stdin |= out.close_stdin;
        self.stop |= out.stop;
        writes
    }

    /// A line from the server (checked against the schema first); what we wrote because of it.
    fn server(&mut self, line: &str) -> Vec<Value> {
        check_server_line(line, None);
        let v: Value = serde_json::from_str(line).unwrap();
        if let (Some(id), Some(method)) = (v.get("id"), v["method"].as_str()) {
            let key = id.as_str().map_or_else(|| id.to_string(), String::from);
            self.asked.insert(key, method.to_string());
        }
        let out = self.p.on_line(line);
        self.take(out)
    }

    fn respond(&mut self, id: u64, result: Value, schema: Option<&str>) -> Vec<Value> {
        let line = json!({"id": id, "result": result}).to_string();
        check_server_line(&line, schema);
        let out = self.p.on_line(&line);
        self.take(out)
    }

    /// Runs the handshake up to a started turn (ids 1 to 4).
    fn started(params: CodexParams) -> Run {
        let mut run = Run::new(params);
        run.respond(1, json!({"userAgent": "codex/0.154.0"}), None);
        run.respond(
            2,
            json!({"rateLimits": snapshot(10, 5)}),
            Some("v2/GetAccountRateLimitsResponse.json"),
        );
        run.respond(3, thread_result(), Some("v2/ThreadStartResponse.json"));
        run.respond(
            4,
            json!({"turn": turn("inProgress")}),
            Some("v2/TurnStartResponse.json"),
        );
        run
    }

    fn response_schema(&self, line: &str) -> &'static str {
        let v: Value = serde_json::from_str(line).unwrap();
        let id = v["id"]
            .as_str()
            .map_or_else(|| v["id"].to_string(), String::from);
        match self.asked.get(&id).map(String::as_str) {
            Some("item/fileChange/requestApproval") => "FileChangeRequestApprovalResponse.json",
            _ => "CommandExecutionRequestApprovalResponse.json",
        }
    }

    fn answer(&mut self, id: &str, a: ApprovalAnswer) -> Vec<Value> {
        let out = self.p.on_answer(id, a).unwrap();
        self.take(out)
    }
}

// --- tests -----------------------------------------------------------------------------------

#[test]
fn the_first_line_is_initialize_with_harness_as_the_client() {
    let run = Run::new(params());
    assert_eq!(run.writes.len(), 1);
    assert_eq!(run.writes[0]["method"], "initialize");
    assert_eq!(run.writes[0]["id"], 1);
    assert_eq!(run.writes[0]["params"]["clientInfo"]["name"], "harness");
}

#[test]
fn after_initialize_it_says_initialized_reads_the_limits_and_starts_a_thread() {
    let mut run = Run::new(params());
    let w = run.respond(1, json!({"userAgent": "codex/0.154.0"}), None);
    assert_eq!(w[0]["method"], "initialized");
    assert!(w[0].get("id").is_none());
    assert_eq!(
        (w[1]["method"].as_str(), w[1]["id"].as_u64()),
        (Some("account/rateLimits/read"), Some(2))
    );
    assert_eq!(
        (w[2]["method"].as_str(), w[2]["id"].as_u64()),
        (Some("thread/start"), Some(3))
    );
    assert_eq!(w[2]["params"]["cwd"], "/work");
    assert_eq!(w[2]["params"]["approvalPolicy"], "untrusted");
    assert_eq!(w[2]["params"]["sandbox"], "workspace-write");
}

#[test]
fn a_started_thread_starts_the_turn_with_the_task_and_the_policy() {
    let mut run = Run::new(CodexParams {
        model: Some("gpt-5-codex".into()),
        ..params()
    });
    run.respond(1, json!({}), None);
    let w = run.respond(3, thread_result(), Some("v2/ThreadStartResponse.json"));
    assert_eq!(w[0]["method"], "turn/start");
    let p = &w[0]["params"];
    assert_eq!(p["threadId"], THREAD);
    assert_eq!(p["input"][0]["type"], "text");
    assert_eq!(p["input"][0]["text"], "fix the bug");
    assert_eq!(p["model"], "gpt-5-codex");
    assert_eq!(p["cwd"], "/work");
    assert_eq!(p["approvalPolicy"], "untrusted");
    assert_eq!(p["sandboxPolicy"]["type"], "workspaceWrite");
    assert_eq!(p["sandboxPolicy"]["networkAccess"], false);
    assert!(
        matches!(&run.events[0], DelegateEvent::Started { session_id, model, child_version }
        if session_id == THREAD && model.as_deref() == Some("gpt-5-codex") && child_version == "0.154.0")
    );
    assert_eq!(run.p.session_id().as_deref(), Some(THREAD));
}

#[test]
fn resuming_a_thread_sends_thread_resume_with_its_id() {
    let mut run = Run::new(CodexParams {
        resume: Some("th-old".into()),
        ..params()
    });
    let w = run.respond(1, json!({}), None);
    assert_eq!(w[2]["method"], "thread/resume");
    assert_eq!(w[2]["params"]["threadId"], "th-old");
    let w = run.respond(3, thread_result(), Some("v2/ThreadResumeResponse.json"));
    assert_eq!(
        w[0]["params"]["threadId"], THREAD,
        "the turn goes to the thread codex answered with"
    );
}

#[test]
fn each_mode_maps_to_its_sandbox_and_approval_policy() {
    for (mode, approval, sandbox, kind) in [
        (Mode::Plan, "on-request", "read-only", "readOnly"),
        (Mode::ReadOnly, "untrusted", "read-only", "readOnly"),
        (Mode::Ask, "untrusted", "workspace-write", "workspaceWrite"),
        (
            Mode::Auto,
            "on-request",
            "workspace-write",
            "workspaceWrite",
        ),
    ] {
        let p = policy(mode, false, &[]).unwrap();
        assert_eq!(
            (
                p.approval,
                p.sandbox,
                p.sandbox_policy["type"].as_str().unwrap()
            ),
            (approval, sandbox, kind),
            "{mode}"
        );
        assert_eq!(
            p.sandbox_policy["networkAccess"], false,
            "{mode}: no network"
        );
    }
    assert!(matches!(
        policy(Mode::FullAccess, false, &[]),
        Err(DelegateError::FullAccessUnconfirmed)
    ));
    let p = policy(Mode::FullAccess, true, &[]).unwrap();
    assert_eq!(
        (
            p.approval,
            p.sandbox,
            p.sandbox_policy["type"].as_str().unwrap()
        ),
        ("never", "danger-full-access", "dangerFullAccess")
    );
    let p = policy(Mode::Auto, false, &["/extra".into()]).unwrap();
    assert_eq!(p.sandbox_policy["writableRoots"], json!(["/extra"]));
}

#[test]
fn the_policy_goes_on_the_wire_for_a_plan_run() {
    let mut run = Run::new(CodexParams {
        mode: Mode::Plan,
        ..params()
    });
    let w = run.respond(1, json!({}), None);
    assert_eq!(w[2]["params"]["sandbox"], "read-only");
    assert_eq!(w[2]["params"]["approvalPolicy"], "on-request");
    let w = run.respond(3, thread_result(), None);
    assert_eq!(w[0]["params"]["sandboxPolicy"]["type"], "readOnly");
}

#[test]
fn the_limits_read_at_start_and_the_updates_become_rate_limit_events() {
    let mut run = Run::started(params());
    let DelegateEvent::RateLimits {
        snapshot,
        status,
        plan,
    } = run
        .events
        .iter()
        .find(|e| matches!(e, DelegateEvent::RateLimits { .. }))
        .unwrap()
        .clone()
    else {
        panic!()
    };
    assert_eq!(status, Some(LimitStatus::Allowed));
    assert_eq!(plan.as_deref(), Some("plus"));
    assert_eq!(snapshot.windows.len(), 2);
    assert_eq!(snapshot.windows[0].window_minutes, Some(300));
    assert_eq!(snapshot.windows[0].used_percent, Some(10.0));
    assert_eq!(snapshot.windows[0].resets_at, Some(1_800));
    assert_eq!(snapshot.windows[0].source, WindowSource::Poll);
    assert_eq!(snapshot.windows[1].window_minutes, Some(10_080));
    assert_eq!(snapshot.observed_at, 1_000);

    run.events.clear();
    run.server(&note(
        "account/rateLimits/updated",
        json!({"rateLimits": snapshot_with_reached()}),
    ));
    let DelegateEvent::RateLimits {
        snapshot, status, ..
    } = &run.events[0]
    else {
        panic!()
    };
    assert_eq!(*status, Some(LimitStatus::Rejected));
    assert_eq!(snapshot.windows[0].used_percent, Some(100.0));
    assert_eq!(snapshot.windows[0].source, WindowSource::Stream);
}

fn snapshot_with_reached() -> Value {
    let mut s = snapshot(100, 60);
    s["rateLimitReachedType"] = json!("rate_limit_reached");
    s
}

#[test]
fn a_failed_limits_read_is_ignored() {
    let mut run = Run::new(params());
    run.respond(1, json!({}), None);
    let line = json!({"id": 2, "error": {"code": -32000, "message": "not signed in with chatgpt"}})
        .to_string();
    check_server_line(&line, None);
    let out = run.p.on_line(&line);
    let writes = run.take(out);
    assert!(writes.is_empty() && run.events.is_empty() && !run.finished);
}

#[test]
fn a_happy_turn_streams_text_tools_files_and_one_usage() {
    let mut run = Run::started(params());
    run.events.clear();
    run.server(&note(
        "item/agentMessage/delta",
        json!({"delta": "Hel", "itemId": "m1", "threadId": THREAD, "turnId": TURN}),
    ));
    run.server(&note(
        "item/agentMessage/delta",
        json!({"delta": "lo", "itemId": "m1", "threadId": THREAD, "turnId": TURN}),
    ));
    run.server(&item_completed(
        json!({"type": "agentMessage", "id": "m1", "text": "Hello"}),
    ));
    run.server(&note("item/reasoning/summaryTextDelta", json!({"delta": "thinking", "itemId": "r1", "summaryIndex": 0, "threadId": THREAD, "turnId": TURN})));
    run.server(&item_started(command_item("inProgress", None, None)));
    run.server(&item_completed(command_item(
        "completed",
        Some("ok\n"),
        Some(0),
    )));
    run.server(&item_started(file_item("inProgress")));
    run.server(&item_completed(file_item("completed")));
    run.server(&usage_note(100, 100));
    run.server(&usage_note(100, 100));
    run.server(&usage_note(250, 150));
    run.server(&note(
        "turn/completed",
        json!({"threadId": THREAD, "turn": turn("completed")}),
    ));
    let e = &run.events;
    assert_eq!(e[0], DelegateEvent::TextDelta { text: "Hel".into() });
    assert_eq!(e[1], DelegateEvent::TextDelta { text: "lo".into() });
    assert_eq!(
        e[2],
        DelegateEvent::AssistantMessage {
            content: "Hello".into(),
            model: Some("gpt-5-codex".into())
        }
    );
    assert_eq!(
        e[3],
        DelegateEvent::ReasoningDelta {
            text: "thinking".into()
        }
    );
    let DelegateEvent::ToolCallRequested {
        id,
        name,
        arguments,
    } = &e[4]
    else {
        panic!("{:?}", e[4])
    };
    assert_eq!((id.as_str(), name.as_str()), ("item-cmd", "command"));
    assert_eq!(
        serde_json::from_str::<Value>(arguments).unwrap()["command"],
        "cargo test"
    );
    assert_eq!(
        e[5],
        DelegateEvent::ToolCallFinished {
            id: "item-cmd".into(),
            output: "ok\n".into(),
            is_error: false
        }
    );
    assert!(
        matches!(&e[6], DelegateEvent::ToolCallRequested { name, .. } if name == "file_change")
    );
    assert!(
        matches!(&e[7], DelegateEvent::ToolCallFinished { id, is_error: false, .. } if id == "item-file")
    );
    assert_eq!(
        e[8],
        DelegateEvent::FileChanged {
            call_id: "item-file".into(),
            path: "src/a.rs".into(),
            change: FileChange::Update
        }
    );
    assert_eq!(
        e[9],
        DelegateEvent::FileChanged {
            call_id: "item-file".into(),
            path: "src/new.rs".into(),
            change: FileChange::Add
        }
    );
    assert_eq!(
        e[10],
        DelegateEvent::FileChanged {
            call_id: "item-file".into(),
            path: "src/gone.rs".into(),
            change: FileChange::Delete
        }
    );
    let DelegateEvent::Usage { model, usage } = &e[11] else {
        panic!("{:?}", e[11])
    };
    assert_eq!(model, "gpt-5-codex");
    assert_eq!(
        usage.input_tokens, 250,
        "the repeated update is counted once: 100 + 150"
    );
    assert_eq!(usage.cached_tokens, 80);
    assert_eq!(usage.output_tokens, 20);
    assert_eq!(usage.reasoning_tokens, 8);
    assert_eq!(
        e[12],
        DelegateEvent::Finished {
            reason: TurnEndReason::Completed,
            detail: None
        }
    );
    assert_eq!(e.len(), 13);
    assert!(run.finished && run.close_stdin && !run.stop);
}

#[test]
fn text_with_no_deltas_comes_as_a_delta_before_the_message() {
    let mut run = Run::started(params());
    run.events.clear();
    run.server(&item_completed(
        json!({"type": "agentMessage", "id": "m9", "text": "All at once"}),
    ));
    assert_eq!(
        run.events[0],
        DelegateEvent::TextDelta {
            text: "All at once".into()
        }
    );
    assert!(
        matches!(&run.events[1], DelegateEvent::AssistantMessage { content, .. } if content == "All at once")
    );
}

#[test]
fn a_failed_command_and_a_declined_patch_are_errors_and_change_no_file() {
    let mut run = Run::started(params());
    run.events.clear();
    run.server(&item_completed(command_item(
        "completed",
        Some("boom"),
        Some(1),
    )));
    assert!(
        matches!(
            &run.events[0],
            DelegateEvent::ToolCallFinished { is_error: true, .. }
        ),
        "a non-zero exit is an error"
    );
    run.events.clear();
    run.server(&item_completed(file_item("declined")));
    assert!(matches!(
        &run.events[0],
        DelegateEvent::ToolCallFinished { is_error: true, .. }
    ));
    assert_eq!(run.events.len(), 1, "no FileChanged for a declined patch");
}

#[test]
fn an_mcp_tool_call_is_relayed_with_its_server_and_tool() {
    let mut run = Run::started(params());
    run.events.clear();
    let item = |status: &str| {
        json!({"type": "mcpToolCall", "id": "mcp1", "server": "docs", "tool": "search",
        "arguments": {"q": "x"}, "status": status, "result": {"content": [{"type": "text", "text": "found it"}]}})
    };
    run.server(&item_started(item("inProgress")));
    run.server(&item_completed(item("completed")));
    assert!(
        matches!(&run.events[0], DelegateEvent::ToolCallRequested { name, .. } if name == "docs/search")
    );
    assert_eq!(
        run.events[1],
        DelegateEvent::ToolCallFinished {
            id: "mcp1".into(),
            output: "found it".into(),
            is_error: false
        }
    );
}

#[test]
fn a_command_approval_is_relayed_and_answers_map_to_decisions() {
    let all = Some(vec!["accept", "acceptForSession", "decline", "cancel"]);
    let mut run = Run::started(params());
    run.events.clear();
    run.server(&command_request(json!(7), all.clone()));
    let DelegateEvent::ApprovalNeeded(a) = &run.events[0] else {
        panic!("{:?}", run.events)
    };
    assert_eq!(
        (a.id.as_str(), a.call_id.as_str(), a.tool.as_str()),
        ("7", "item-cmd", "command")
    );
    assert_eq!(
        a.kind,
        DelegateApprovalKind::Command {
            command: "cargo test".into(),
            cwd: Some("/work".into())
        }
    );
    assert_eq!(a.reason, "needs to run the tests");
    assert!(a.can_remember);

    for (answer, want) in [
        (ApprovalAnswer::AllowOnce, "accept"),
        (ApprovalAnswer::AllowForRun, "acceptForSession"),
        (
            ApprovalAnswer::Deny {
                reason: Some("no".into()),
            },
            "decline",
        ),
    ] {
        let mut run = Run::started(params());
        run.server(&command_request(json!(7), all.clone()));
        let w = run.answer("7", answer);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0]["id"], 7);
        assert_eq!(w[0]["result"]["decision"], want);
        assert!(!run.stop);
    }
}

#[test]
fn for_the_run_is_only_sent_when_codex_offered_it() {
    let mut run = Run::started(params());
    run.server(&command_request(
        json!(8),
        Some(vec!["accept", "decline", "cancel"]),
    ));
    let DelegateEvent::ApprovalNeeded(a) = run.events.last().unwrap() else {
        panic!()
    };
    assert!(!a.can_remember);
    let w = run.answer("8", ApprovalAnswer::AllowForRun);
    assert_eq!(w[0]["result"]["decision"], "accept");
}

#[test]
fn cancel_declines_the_turn_and_interrupts_it() {
    let mut run = Run::started(params());
    run.server(&command_request(json!(9), None));
    let w = run.answer("9", ApprovalAnswer::Cancel);
    assert_eq!(w[0]["result"]["decision"], "cancel");
    assert_eq!(w[1]["method"], "turn/interrupt");
    assert_eq!(w[1]["params"], json!({"threadId": THREAD, "turnId": TURN}));
    assert!(run.stop);
}

#[test]
fn a_file_change_approval_names_the_paths_codex_already_announced() {
    let mut run = Run::started(params());
    run.server(&item_started(file_item("inProgress")));
    run.events.clear();
    run.server(&file_request(json!("req-abc")));
    let DelegateEvent::ApprovalNeeded(a) = &run.events[0] else {
        panic!()
    };
    assert_eq!(a.id, "req-abc");
    assert_eq!(a.tool, "file_change");
    assert_eq!(
        a.kind,
        DelegateApprovalKind::FileChange {
            paths: vec!["src/a.rs".into(), "src/new.rs".into(), "src/gone.rs".into()]
        }
    );
    let w = run.answer("req-abc", ApprovalAnswer::AllowOnce);
    assert_eq!(w[0]["id"], "req-abc", "a string id is echoed as a string");
    assert_eq!(w[0]["result"]["decision"], "accept");
    let mut run = Run::started(params());
    run.server(&file_request(json!(3)));
    let w = run.answer("3", ApprovalAnswer::Deny { reason: None });
    assert_eq!(w[0]["result"]["decision"], "decline");
}

#[test]
fn requests_harness_does_not_serve_get_an_error_and_a_warning_never_an_acceptance() {
    for (method, params) in [
        (
            "item/permissions/requestApproval",
            json!({"itemId": "i", "threadId": THREAD, "turnId": TURN, "permissions": {}, "startedAtMs": 1, "cwd": "/work"}),
        ),
        (
            "item/tool/requestUserInput",
            json!({"itemId": "i", "threadId": THREAD, "turnId": TURN, "questions": []}),
        ),
    ] {
        let mut run = Run::started(params_default());
        run.events.clear();
        let line = json!({"id": 21, "method": method, "params": params}).to_string();
        let out = run.p.on_line(&line);
        let w = run.take(out);
        assert_eq!(w[0]["id"], 21, "{method}");
        assert!(
            w[0]["error"]["message"]
                .as_str()
                .unwrap()
                .contains("does not handle"),
            "{method}"
        );
        assert!(matches!(&run.events[0], DelegateEvent::Warning { .. }));
        assert!(
            !run.events
                .iter()
                .any(|e| matches!(e, DelegateEvent::ApprovalNeeded(_)))
        );
    }
    let mut run = Run::started(params_default());
    let line = json!({"id": 22, "method": "account/chatgptAuthTokens/refresh", "params": {"reason": "unauthorized"}}).to_string();
    let out = run.p.on_line(&line);
    run.take(out);
    assert!(
        matches!(run.events.last(), Some(DelegateEvent::Warning { message }) if message.contains("codex login"))
    );
}

fn params_default() -> CodexParams {
    params()
}

#[test]
fn a_resolved_request_withdraws_the_approval_and_a_late_answer_writes_nothing() {
    let mut run = Run::started(params());
    run.server(&command_request(json!(5), None));
    run.events.clear();
    run.server(&note(
        "serverRequest/resolved",
        json!({"requestId": 5, "threadId": THREAD}),
    ));
    assert_eq!(
        run.events,
        vec![DelegateEvent::ApprovalWithdrawn { id: "5".into() }]
    );
    assert!(run.answer("5", ApprovalAnswer::AllowOnce).is_empty());
}

#[test]
fn an_answer_is_given_once_and_an_unknown_id_is_refused() {
    let mut run = Run::started(params());
    run.server(&command_request(json!(5), None));
    run.answer("5", ApprovalAnswer::AllowOnce);
    assert!(matches!(
        run.p.on_answer("5", ApprovalAnswer::AllowOnce),
        Err(DelegateError::UnknownApproval(_))
    ));
    assert!(matches!(
        run.p.on_answer("zzz", ApprovalAnswer::AllowOnce),
        Err(DelegateError::UnknownApproval(_))
    ));
}

#[test]
fn an_interrupt_sends_turn_interrupt_and_the_interrupted_turn_finishes_as_interrupted() {
    let mut run = Run::started(params());
    let out = run.p.on_interrupt();
    let w = run.take(out);
    assert_eq!(w[0]["method"], "turn/interrupt");
    assert_eq!(w[0]["params"], json!({"threadId": THREAD, "turnId": TURN}));
    run.events.clear();
    run.server(&note(
        "turn/completed",
        json!({"threadId": THREAD, "turn": turn("interrupted")}),
    ));
    assert_eq!(
        run.events.last(),
        Some(&DelegateEvent::Finished {
            reason: TurnEndReason::Interrupted,
            detail: None
        })
    );
    assert!(run.finished);
}

#[test]
fn an_interrupt_before_the_turn_has_an_id_is_sent_when_it_gets_one() {
    let mut run = Run::new(params());
    run.respond(1, json!({}), None);
    run.respond(3, thread_result(), None);
    let out = run.p.on_interrupt();
    assert!(run.take(out).is_empty(), "no turn to interrupt yet");
    let w = run.respond(4, json!({"turn": turn("inProgress")}), None);
    assert_eq!(w[0]["method"], "turn/interrupt");
}

#[test]
fn an_interrupt_before_the_thread_exists_ends_the_run_without_a_turn() {
    let mut run = Run::new(params());
    run.respond(1, json!({}), None);
    let out = run.p.on_interrupt();
    run.take(out);
    let w = run.respond(3, thread_result(), None);
    assert!(
        w.iter().all(|m| m["method"] != "turn/start"),
        "no turn is started: {w:?}"
    );
    assert_eq!(
        run.events.last(),
        Some(&DelegateEvent::Finished {
            reason: TurnEndReason::Interrupted,
            detail: None
        })
    );
    assert!(run.finished && run.close_stdin);
}

#[test]
fn a_failed_turn_is_an_error_with_codexs_message() {
    let mut run = Run::started(params());
    run.events.clear();
    let mut t = turn("failed");
    t["error"] = json!({"message": "usage limit reached"});
    run.server(&note(
        "turn/completed",
        json!({"threadId": THREAD, "turn": t}),
    ));
    assert!(
        matches!(&run.events[0], DelegateEvent::Error { kind: ErrorKind::Provider, message } if message.contains("usage limit"))
    );
    assert_eq!(
        run.events[1],
        DelegateEvent::Finished {
            reason: TurnEndReason::Error,
            detail: None
        }
    );
}

#[test]
fn error_notifications_are_retries_or_the_failure_a_turn_with_no_message_reports() {
    let mut run = Run::started(params());
    run.events.clear();
    run.server(&note("error", json!({"error": {"message": "stream dropped"}, "threadId": THREAD, "turnId": TURN, "willRetry": true})));
    assert!(
        matches!(&run.events[0], DelegateEvent::Retrying { attempt: 1, reason, .. } if reason.contains("stream dropped"))
    );
    run.server(&note("error", json!({"error": {"message": "server overloaded"}, "threadId": THREAD, "turnId": TURN, "willRetry": false})));
    run.events.clear();
    run.server(&note(
        "turn/completed",
        json!({"threadId": THREAD, "turn": turn("failed")}),
    ));
    assert!(
        matches!(&run.events[0], DelegateEvent::Error { message, .. } if message.contains("server overloaded"))
    );
}

#[test]
fn an_error_answer_to_a_request_that_starts_the_run_ends_it() {
    let mut run = Run::new(params());
    run.respond(1, json!({}), None);
    let line =
        json!({"id": 3, "error": {"code": -32600, "message": "model not found"}}).to_string();
    let out = run.p.on_line(&line);
    run.take(out);
    assert!(
        matches!(&run.events[0], DelegateEvent::Error { message, .. } if message.contains("model not found"))
    );
    assert_eq!(
        run.events[1],
        DelegateEvent::Finished {
            reason: TurnEndReason::Error,
            detail: None
        }
    );
    assert!(run.finished && run.stop && run.close_stdin);
}

#[test]
fn warnings_and_reroutes_are_warnings_and_unknown_things_are_ignored() {
    let mut run = Run::started(params());
    run.events.clear();
    run.server(&note("warning", json!({"message": "careful"})));
    run.server(&note("model/rerouted", json!({"fromModel": "a", "toModel": "b", "reason": "highRiskCyberActivity", "threadId": THREAD, "turnId": TURN})));
    assert!(matches!(&run.events[0], DelegateEvent::Warning { message } if message == "careful"));
    assert!(
        matches!(&run.events[1], DelegateEvent::Warning { message } if message.contains('a') && message.contains('b'))
    );
    run.events.clear();
    let out = run
        .p
        .on_line(&json!({"method": "some/future/notification", "params": {}}).to_string());
    assert!(out.events.is_empty() && out.writes.is_empty());
    let out = run.p.on_line(&json!({"id": 999, "result": {}}).to_string());
    assert!(out.events.is_empty() && out.writes.is_empty());
    let out = run.p.on_line("{broken");
    assert!(
        matches!(&out.events[0], DelegateEvent::Warning { message } if message.contains("not JSON"))
    );
}

#[test]
fn every_request_id_harness_writes_is_new() {
    let mut run = Run::started(params());
    run.server(&command_request(json!(5), None));
    run.answer("5", ApprovalAnswer::Cancel);
    let ids: Vec<u64> = run
        .writes
        .iter()
        .filter(|w| w.get("method").is_some())
        .filter_map(|w| w["id"].as_u64())
        .collect();
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted, ids, "ids are unique and rise: {ids:?}");
}
