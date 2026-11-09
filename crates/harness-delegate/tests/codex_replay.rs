//! A real `codex app-server` run (codex 0.154.0, ChatGPT free plan, approval policy `untrusted`,
//! personal data scrubbed), replayed through the protocol: what harness writes must match what the
//! recording's client sent, and the events must be what really happened.

mod codex_support;

use codex_support::check_server_line;
use harness_core::{event::TurnEndReason, message::Usage, meter::WindowSource, permission::Mode};
use harness_delegate::{
    codex::protocol::{CodexParams, CodexProtocol},
    driver::{Out, Protocol},
    event::{ApprovalAnswer, DelegateApprovalKind, DelegateEvent, LimitStatus},
};
use serde_json::{Value, json};

const MODEL: &str = "gpt-5.6-luna";
const THREAD: &str = "01a1be41-ffaf-7ce0-95f2-3a9d561469d7";
/// Index of the last line of the first turn (`turn/completed`) and of the second turn's request.
const FIRST_TURN_END: usize = 167;
const SECOND_TURN_START: usize = 168;

struct Line {
    dir_in: bool,
    v: Value,
    raw: String,
}

fn recording() -> Vec<Line> {
    let text = include_str!("fixtures/codex/ask-0.154.0.jsonl");
    text.lines()
        .map(|l| {
            let outer: Value = serde_json::from_str(l).unwrap();
            let v = outer["line"].clone();
            Line {
                dir_in: outer["dir"] == "in",
                raw: v.to_string(),
                v,
            }
        })
        .collect()
}

fn params(task: &str) -> CodexParams {
    CodexParams {
        mode: Mode::Ask,
        model: Some(MODEL.into()),
        clock: || 1_000,
        ..CodexParams::new(task, "/work/ws", "0.154.0")
    }
}

#[derive(Default)]
struct Replay {
    writes: Vec<Value>,
    events: Vec<DelegateEvent>,
    finished: bool,
}

impl Replay {
    fn take(&mut self, out: Out) {
        self.writes.extend(
            out.writes
                .iter()
                .map(|w| serde_json::from_str::<Value>(w).unwrap()),
        );
        self.events.extend(out.events);
        self.finished |= out.finished;
    }

    /// Feeds one recorded server line. Approvals are answered the way the recorder did: the
    /// first `accept`, every later one `decline`.
    fn feed(&mut self, p: &mut CodexProtocol, line: &str, approvals: &mut usize) {
        check_server_line(line, None);
        let before = self.events.len();
        let out = p.on_line(line);
        self.take(out);
        let new: Vec<String> = self.events[before..]
            .iter()
            .filter_map(|e| match e {
                DelegateEvent::ApprovalNeeded(a) => Some(a.id.clone()),
                _ => None,
            })
            .collect();
        for id in new {
            let answer = if *approvals == 0 {
                ApprovalAnswer::AllowOnce
            } else {
                ApprovalAnswer::Deny { reason: None }
            };
            *approvals += 1;
            let out = p.on_answer(&id, answer).unwrap();
            self.take(out);
        }
    }
}

fn without_version(mut v: Value) -> Value {
    if let Some(info) = v.pointer_mut("/params/clientInfo") {
        info["version"] = json!("x");
    }
    v
}

/// What the recorder's client sent in `range`, in order.
fn sent(lines: &[Line], range: std::ops::Range<usize>) -> Vec<Value> {
    lines[range]
        .iter()
        .filter(|l| l.dir_in)
        .map(|l| without_version(l.v.clone()))
        .collect()
}

fn first_turn() -> (Replay, Vec<Value>) {
    let lines = recording();
    let mut p = CodexProtocol::new(params(
        "1) If your project instructions mention a secret word, say it; otherwise say NONE. 2) Run the shell command: cat hello.txt 3) Create a file out.txt containing that text in upper case. 4) Reply DONE.",
    ))
    .unwrap();
    let mut r = Replay::default();
    let out = p.start();
    r.take(out);
    let mut approvals = 0;
    for l in &lines[..=FIRST_TURN_END] {
        if !l.dir_in {
            r.feed(&mut p, &l.raw, &mut approvals);
        }
    }
    let expected = sent(&lines, 0..SECOND_TURN_START);
    (r, expected)
}

#[test]
fn what_harness_writes_matches_what_the_recorded_client_sent() {
    let (r, expected) = first_turn();
    let written: Vec<Value> = r.writes.into_iter().map(without_version).collect();
    assert_eq!(written, expected);
}

#[test]
fn the_first_turn_events_are_what_happened() {
    let (r, _) = first_turn();
    let ev = &r.events;
    assert!(r.finished);

    // Started, once, with the thread and the model the thread reported.
    let started: Vec<_> = ev
        .iter()
        .filter(|e| matches!(e, DelegateEvent::Started { .. }))
        .collect();
    assert_eq!(
        started,
        [&DelegateEvent::Started {
            session_id: THREAD.into(),
            model: Some(MODEL.into()),
            child_version: "0.154.0".into(),
        }]
    );

    // Text: every message streamed once, then the message itself; the last one has the secret word.
    let streamed: String = ev
        .iter()
        .filter_map(|e| match e {
            DelegateEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    let messages: Vec<&str> = ev
        .iter()
        .filter_map(|e| match e {
            DelegateEvent::AssistantMessage { content, .. } => Some(content.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(messages.len(), 3, "{messages:?}");
    assert_eq!(streamed, messages.concat(), "each message is shown once");
    assert!(messages[0].starts_with("I\u{2019}ll check the project instruction"));
    assert!(messages[2].starts_with("PINEAPPLE."));

    // The four approvals: ids as the server sent them, what is asked, what can be remembered.
    let approvals: Vec<_> = ev
        .iter()
        .filter_map(|e| match e {
            DelegateEvent::ApprovalNeeded(a) => Some(a),
            _ => None,
        })
        .collect();
    let ids: Vec<&str> = approvals.iter().map(|a| a.id.as_str()).collect();
    assert_eq!(ids, ["0", "1", "2", "3"]);
    assert_eq!(
        approvals[0].kind,
        DelegateApprovalKind::Command {
            command: "/bin/zsh -lc 'cat hello.txt'".into(),
            cwd: Some("/work/ws".into()),
        }
    );
    assert!(
        !approvals[0].can_remember,
        "acceptForSession was not offered for the command"
    );
    for i in [1, 2] {
        assert_eq!(
            approvals[i].kind,
            DelegateApprovalKind::FileChange {
                paths: vec!["/work/ws/out.txt".into()]
            }
        );
    }
    assert!(matches!(
        &approvals[3].kind,
        DelegateApprovalKind::Command { command, .. } if command.contains("> out.txt")
    ));
    // Answered by harness, then resolved by the server: never shown as withdrawn.
    assert!(
        !ev.iter()
            .any(|e| matches!(e, DelegateEvent::ApprovalWithdrawn { .. }))
    );

    // Items: the command ran (accepted), the three writes were declined.
    let finished: Vec<(&str, &str, bool)> = ev
        .iter()
        .filter_map(|e| match e {
            DelegateEvent::ToolCallFinished {
                id,
                output,
                is_error,
            } => Some((id.as_str(), output.as_str(), *is_error)),
            _ => None,
        })
        .collect();
    assert_eq!(finished.len(), 4, "{finished:?}");
    assert_eq!(finished[0].1, "hello from harness\n");
    assert!(!finished[0].2);
    assert!(finished[1..].iter().all(|f| f.2), "declined is an error");
    assert!(
        !ev.iter()
            .any(|e| matches!(e, DelegateEvent::FileChanged { .. })),
        "nothing was written"
    );
    let requested = ev
        .iter()
        .filter(|e| matches!(e, DelegateEvent::ToolCallRequested { .. }))
        .count();
    assert_eq!(requested, 4);

    // Tokens: the sum of every request's `last`, not the thread's `total` (which would also
    // carry earlier turns of a thread).
    let usage: Vec<_> = ev
        .iter()
        .filter_map(|e| match e {
            DelegateEvent::Usage { model, usage } => Some((model.as_str(), *usage)),
            _ => None,
        })
        .collect();
    assert_eq!(
        usage,
        [(
            MODEL,
            Usage {
                input_tokens: 80_117,
                output_tokens: 1_164,
                cached_tokens: 62_976,
                cache_write_tokens: 0,
                cache_write_1h_tokens: 0,
                reasoning_tokens: 712,
            }
        )]
    );

    // Rate limits: the read, then one update per request. A free plan has one window.
    let limits: Vec<_> = ev
        .iter()
        .filter_map(|e| match e {
            DelegateEvent::RateLimits {
                snapshot,
                status,
                plan,
            } => Some((snapshot, *status, plan.as_deref())),
            _ => None,
        })
        .collect();
    assert_eq!(limits.len(), 6);
    for (i, (snapshot, status, plan)) in limits.iter().enumerate() {
        assert_eq!(
            snapshot.windows.len(),
            1,
            "secondary is null on a free plan"
        );
        let w = &snapshot.windows[0];
        assert_eq!(w.window_minutes, Some(43_200));
        assert_eq!(w.used_percent, Some(0.0));
        assert_eq!(
            w.source,
            if i == 0 {
                WindowSource::Poll
            } else {
                WindowSource::Stream
            }
        );
        assert_eq!(*status, Some(LimitStatus::Allowed));
        assert_eq!(*plan, Some("free"));
    }
    assert_eq!(limits[0].0.windows[0].resets_at, Some(1_796_785_359));

    // One finish, after the usage, because the turn completed.
    let finishes: Vec<_> = ev
        .iter()
        .filter_map(|e| match e {
            DelegateEvent::Finished { reason, .. } => Some(*reason),
            _ => None,
        })
        .collect();
    assert_eq!(finishes, [TurnEndReason::Completed]);
    assert!(matches!(ev.last(), Some(DelegateEvent::Finished { .. })));
}

#[test]
fn the_second_turn_of_the_thread_finishes_with_its_own_usage() {
    // Turn two of the recording, as a run of its own on the same thread: the thread's `total`
    // already holds turn one, and only `last` is this turn's.
    let lines = recording();
    let mut p = CodexProtocol::new(CodexParams {
        resume: Some(THREAD.into()),
        ..params("Reply with just OK.")
    })
    .unwrap();
    let mut r = Replay::default();
    let out = p.start();
    r.take(out);
    let mut approvals = 0;
    // The handshake answers (initialize, rate limits, the thread), then the turn's.
    for i in [1, 19, 6] {
        r.feed(&mut p, &lines[i].raw, &mut approvals);
    }
    // Our turn/start is request 4 here (the recording's client had made 4 requests before).
    let turn_response = lines[169].v.to_string().replace("\"id\":5", "\"id\":4");
    for raw in std::iter::once(&turn_response).chain(lines[170..].iter().map(|l| &l.raw)) {
        r.feed(&mut p, raw, &mut approvals);
    }
    assert!(r.finished);
    let usage: Vec<_> = r
        .events
        .iter()
        .filter_map(|e| match e {
            DelegateEvent::Usage { usage, .. } => Some(*usage),
            _ => None,
        })
        .collect();
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0].input_tokens, 21_660);
    assert_eq!(usage[0].cached_tokens, 16_256);
    assert_eq!(usage[0].output_tokens, 5);
    let texts: Vec<&str> = r
        .events
        .iter()
        .filter_map(|e| match e {
            DelegateEvent::AssistantMessage { content, .. } => Some(content.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, ["OK"]);
    assert!(matches!(
        r.events.last(),
        Some(DelegateEvent::Finished {
            reason: TurnEndReason::Completed,
            ..
        })
    ));
    // What was written for the turn: the thread resumed, then a turn with the task.
    let methods: Vec<&str> = r
        .writes
        .iter()
        .filter_map(|w| w["method"].as_str())
        .collect();
    assert_eq!(
        methods,
        [
            "initialize",
            "initialized",
            "account/rateLimits/read",
            "thread/resume",
            "turn/start"
        ]
    );
    let turn_start = r.writes.last().unwrap();
    let expected = &sent(&lines, SECOND_TURN_START..SECOND_TURN_START + 1)[0];
    assert_eq!(turn_start["params"], expected["params"]);
}

// --- a model the account cannot use ---------------------------------------------------------

const TURN: &str = "01a1be42-0086-7921-bd5c-c6dd5224993f";
const DETAIL: &str =
    "The 'gpt-5.6' model is not supported when using Codex with a ChatGPT account.";

/// A run up to a started turn, from the recording's own handshake lines.
fn started_turn() -> (CodexProtocol, Replay) {
    let lines = recording();
    let mut p = CodexProtocol::new(params("do it")).unwrap();
    let mut r = Replay::default();
    let out = p.start();
    r.take(out);
    let mut approvals = 0;
    for i in [1, 19, 6, 17] {
        r.feed(&mut p, &lines[i].raw, &mut approvals);
    }
    (p, r)
}

fn error_note(message: &str, details: Value) -> String {
    json!({"method": "error", "params": {"error": {"message": message, "codexErrorInfo": "badRequest",
        "additionalDetails": details}, "threadId": THREAD, "turnId": TURN, "willRetry": false}})
    .to_string()
}

fn failed_turn(error: Value) -> String {
    json!({"method": "turn/completed", "params": {"threadId": THREAD, "turn": {"id": TURN, "items": [],
        "itemsView": "summary", "status": "failed", "error": error, "startedAt": 1, "completedAt": 2,
        "durationMs": 1}}})
    .to_string()
}

fn errors(r: &Replay) -> Vec<&str> {
    r.events
        .iter()
        .filter_map(|e| match e {
            DelegateEvent::Error { message, .. } => Some(message.as_str()),
            _ => None,
        })
        .collect()
}

fn assert_failed_with_detail(r: &Replay) {
    assert!(r.finished);
    let errors = errors(r);
    assert_eq!(
        errors.len(),
        1,
        "one error, not one per notification: {errors:?}"
    );
    assert!(errors[0].contains(DETAIL), "{}", errors[0]);
    assert!(matches!(
        r.events.last(),
        Some(DelegateEvent::Finished {
            reason: TurnEndReason::Error,
            ..
        })
    ));
    assert_eq!(
        r.events
            .iter()
            .filter(|e| matches!(e, DelegateEvent::Finished { .. }))
            .count(),
        1
    );
}

#[test]
fn a_model_the_account_cannot_use_fails_the_turn_with_codexs_text() {
    // The shape of the real failure: an `error` notification, then `turn/completed` as failed
    // with no error of its own.
    let (mut p, mut r) = started_turn();
    let mut n = 0;
    r.feed(&mut p, &error_note(DETAIL, Value::Null), &mut n);
    assert!(
        errors(&r).is_empty() && !r.finished,
        "wait for the turn's end"
    );
    r.feed(&mut p, &failed_turn(Value::Null), &mut n);
    assert_failed_with_detail(&r);
}

#[test]
fn the_turns_own_error_is_the_one_shown_and_only_once() {
    let (mut p, mut r) = started_turn();
    let mut n = 0;
    r.feed(&mut p, &error_note(DETAIL, Value::Null), &mut n);
    r.feed(&mut p, &failed_turn(json!({"message": DETAIL})), &mut n);
    assert_failed_with_detail(&r);
}

#[test]
fn the_http_body_in_additional_details_is_part_of_the_message() {
    let (mut p, mut r) = started_turn();
    let mut n = 0;
    r.feed(
        &mut p,
        &error_note("unexpected status 400 Bad Request", json!(DETAIL)),
        &mut n,
    );
    r.feed(&mut p, &failed_turn(Value::Null), &mut n);
    assert_failed_with_detail(&r);
    assert!(errors(&r)[0].contains("400"));
}

#[test]
fn a_failed_turn_after_retries_still_names_the_last_error() {
    let (mut p, mut r) = started_turn();
    let mut n = 0;
    let retry = json!({"method": "error", "params": {"error": {"message": "stream dropped"},
        "threadId": THREAD, "turnId": TURN, "willRetry": true}})
    .to_string();
    r.feed(&mut p, &retry, &mut n);
    r.feed(&mut p, &error_note(DETAIL, Value::Null), &mut n);
    r.feed(&mut p, &failed_turn(Value::Null), &mut n);
    assert_failed_with_detail(&r);
    assert!(
        r.events
            .iter()
            .any(|e| matches!(e, DelegateEvent::Retrying { attempt: 1, .. }))
    );
}
