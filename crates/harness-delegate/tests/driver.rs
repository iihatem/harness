mod common;

use std::{path::Path, time::Duration};

use common::{Fake, Toy, collect, finished, texts};
use harness_core::{
    event::{ErrorKind, TurnEndReason},
    redact::Redactor,
};
use harness_delegate::{
    DelegateError,
    driver::{DriverConfig, spawn_run},
    event::{ApprovalAnswer, DelegateEvent},
};
use std::sync::Arc;
use tokio::process::Command;

const LIMIT: Duration = Duration::from_secs(10);

fn cfg() -> DriverConfig {
    DriverConfig {
        label: "fake".into(),
        grace: Duration::from_millis(150),
        redactor: Arc::new(Redactor::default()),
        max_line: 4096,
        keep: Default::default(),
    }
}

fn run(fake: &Fake, cfg: DriverConfig) -> harness_delegate::DelegateRun {
    spawn_run(Command::new(&fake.binary), Toy::default(), cfg).unwrap()
}

/// Waits for the child's `ready` line: its signal dispositions are set by then.
async fn ready(r: &mut harness_delegate::DelegateRun) {
    let first = tokio::time::timeout(LIMIT, r.events.recv()).await.unwrap();
    assert!(matches!(first, Some(DelegateEvent::TextDelta { text }) if text == "ready"));
}

fn alive(pid: i32) -> bool {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
}

async fn pid_from(file: &Path) -> i32 {
    for _ in 0..500 {
        if let Some(pid) = std::fs::read_to_string(file)
            .ok()
            .and_then(|text| text.trim().parse().ok())
        {
            return pid;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("no pid in {}", file.display());
}

async fn dead_soon(pid: i32) -> bool {
    for _ in 0..100 {
        if !alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    false
}

#[tokio::test]
async fn lines_come_out_in_order_and_the_stream_ends_after_finished() {
    let fake = Fake::new("x", "1.0.0", "emit a\nemit b\nemit done\n");
    let mut r = run(&fake, cfg());
    let events = collect(&mut r.events, LIMIT).await;
    assert_eq!(texts(&events), ["a", "b"]);
    assert_eq!(finished(&events), Some(TurnEndReason::Completed));
    assert!(matches!(
        events.last(),
        Some(DelegateEvent::Finished { .. })
    ));
}

#[tokio::test]
async fn a_line_over_the_cap_is_dropped_with_a_warning_and_the_stream_goes_on() {
    let long = "x".repeat(10_000);
    let fake = Fake::new("x", "1.0.0", &format!("emit {long}\nemit ok\nemit done\n"));
    let mut r = run(
        &fake,
        DriverConfig {
            max_line: 256,
            keep: Default::default(),
            ..cfg()
        },
    );
    let events = collect(&mut r.events, LIMIT).await;
    assert_eq!(texts(&events), ["ok"]);
    assert!(
        events.iter().any(
            |e| matches!(e, DelegateEvent::Warning { message } if message.contains("too long"))
        ),
        "{events:?}"
    );
}

#[tokio::test]
async fn a_crash_before_finishing_is_an_error_with_the_redacted_stderr_tail() {
    let fake = Fake::new(
        "x",
        "1.0.0",
        "emit a\nstderr boom sk-live-0123456789abcdef\nexit 3\n",
    );
    let redactor = Arc::new(Redactor::default());
    redactor.add("sk-live-0123456789abcdef");
    let mut r = run(&fake, DriverConfig { redactor, ..cfg() });
    let events = collect(&mut r.events, LIMIT).await;
    assert_eq!(finished(&events), Some(TurnEndReason::Error));
    let message = events
        .iter()
        .find_map(|e| match e {
            DelegateEvent::Error {
                kind: ErrorKind::Internal,
                message,
            } => Some(message.clone()),
            _ => None,
        })
        .expect("an error event");
    assert!(
        message.contains("fake") && message.contains("3") && message.contains("boom"),
        "{message}"
    );
    assert!(!message.contains("sk-live"), "{message}");
    let last_two: Vec<_> = events.iter().rev().take(2).collect();
    assert!(
        matches!(last_two[1], DelegateEvent::Error { .. }),
        "the error comes right before the finish"
    );
}

#[tokio::test]
async fn an_interrupt_writes_the_polite_request_and_then_escalates() {
    // The child neither reads stdin nor ignores SIGINT: the first signal ends it.
    let fake = Fake::new("x", "1.0.0", "hang\n");
    let mut r = run(&fake, cfg());
    r.control.interrupt().unwrap();
    let events = collect(&mut r.events, LIMIT).await;
    assert_eq!(
        finished(&events),
        Some(TurnEndReason::Interrupted),
        "{events:?}"
    );
}

#[tokio::test]
async fn a_child_that_ignores_sigint_is_ended_by_sigterm() {
    let fake = Fake::new("x", "1.0.0", "ignore-sigint\nemit ready\nhang\n");
    let mut r = run(&fake, cfg());
    ready(&mut r).await;
    let started = std::time::Instant::now();
    r.control.interrupt().unwrap();
    let events = collect(&mut r.events, LIMIT).await;
    assert_eq!(finished(&events), Some(TurnEndReason::Interrupted));
    assert!(
        started.elapsed() >= Duration::from_millis(250),
        "SIGINT came first, then a wait"
    );
}

#[tokio::test]
async fn a_child_that_ignores_sigint_and_sigterm_is_killed() {
    let fake = Fake::new(
        "x",
        "1.0.0",
        "ignore-sigint\nignore-sigterm\nemit ready\nhang\n",
    );
    let mut r = run(&fake, cfg());
    ready(&mut r).await;
    let started = std::time::Instant::now();
    r.control.interrupt().unwrap();
    let events = collect(&mut r.events, LIMIT).await;
    assert_eq!(finished(&events), Some(TurnEndReason::Interrupted));
    assert!(
        started.elapsed() >= Duration::from_millis(400),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn the_whole_process_group_is_killed_with_the_child() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("gc.pid");
    let fake = Fake::new(
        "x",
        "1.0.0",
        &format!("spawn-grandchild {}\nhang\n", pidfile.display()),
    );
    let mut r = run(&fake, cfg());
    let pid = pid_from(&pidfile).await;
    assert!(alive(pid));
    r.control.interrupt().unwrap();
    collect(&mut r.events, LIMIT).await;
    assert!(dead_soon(pid).await, "the grandchild outlived the run");
}

#[tokio::test]
async fn dropping_the_event_stream_kills_the_child_and_what_it_started() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("gc.pid");
    let fake = Fake::new(
        "x",
        "1.0.0",
        &format!("spawn-grandchild {}\nhang\n", pidfile.display()),
    );
    let r = run(&fake, cfg());
    let pid = pid_from(&pidfile).await;
    drop(r);
    assert!(
        dead_soon(pid).await,
        "the grandchild outlived a dropped run"
    );
}

#[tokio::test]
async fn control_after_the_run_ended_is_an_error_not_a_hang() {
    let fake = Fake::new("x", "1.0.0", "emit done\n");
    let mut r = run(&fake, cfg());
    collect(&mut r.events, LIMIT).await;
    assert!(matches!(
        r.control.interrupt(),
        Err(DelegateError::RunEnded)
    ));
    let answered = tokio::time::timeout(LIMIT, r.control.answer("x", ApprovalAnswer::AllowOnce))
        .await
        .unwrap();
    assert!(matches!(answered, Err(DelegateError::RunEnded)));
}

#[tokio::test]
async fn an_answer_is_written_by_the_protocol_and_an_unknown_id_is_refused() {
    let fake = Fake::new("x", "1.0.0", "emit hello\nexpect ANSWER x\nemit done\n");
    let mut r = run(&fake, cfg());
    let bad = r.control.answer("nope", ApprovalAnswer::AllowOnce).await;
    assert!(matches!(bad, Err(DelegateError::UnknownApproval(id)) if id == "nope"));
    r.control
        .answer("x", ApprovalAnswer::AllowOnce)
        .await
        .unwrap();
    let events = collect(&mut r.events, LIMIT).await;
    assert_eq!(
        finished(&events),
        Some(TurnEndReason::Completed),
        "{events:?}"
    );
}

#[tokio::test]
async fn a_child_that_lingers_after_finishing_is_ended_and_the_stream_still_ends() {
    let fake = Fake::new("x", "1.0.0", "emit done\nhang\n");
    let mut r = run(&fake, cfg());
    let events = collect(&mut r.events, LIMIT).await;
    assert_eq!(finished(&events), Some(TurnEndReason::Completed));
}

#[tokio::test]
async fn the_session_id_a_protocol_reports_is_readable_from_the_control() {
    struct WithSession;
    impl harness_delegate::driver::Protocol for WithSession {
        fn start(&mut self) -> harness_delegate::driver::Out {
            Default::default()
        }
        fn on_line(&mut self, line: &str) -> harness_delegate::driver::Out {
            let mut out = harness_delegate::driver::Out {
                finished: line == "done",
                ..Default::default()
            };
            if out.finished {
                out.events.push(DelegateEvent::Finished {
                    reason: TurnEndReason::Completed,
                    detail: None,
                });
            }
            out
        }
        fn on_answer(
            &mut self,
            id: &str,
            _: ApprovalAnswer,
        ) -> Result<harness_delegate::driver::Out, DelegateError> {
            Err(DelegateError::UnknownApproval(id.into()))
        }
        fn on_interrupt(&mut self) -> harness_delegate::driver::Out {
            Default::default()
        }
        fn session_id(&self) -> Option<String> {
            Some("sess-1".into())
        }
    }
    let fake = Fake::new("x", "1.0.0", "emit hi\nemit done\n");
    let mut r = spawn_run(Command::new(&fake.binary), WithSession, cfg()).unwrap();
    collect(&mut r.events, LIMIT).await;
    assert_eq!(r.control.session_id().as_deref(), Some("sess-1"));
}

#[tokio::test]
async fn a_protocol_that_asks_to_stop_starts_the_signal_ladder() {
    let fake = Fake::new("x", "1.0.0", "emit ready\nhang\n");
    let mut r = run(&fake, cfg());
    ready(&mut r).await;
    r.control
        .answer("stop", ApprovalAnswer::Cancel)
        .await
        .unwrap();
    let events = collect(&mut r.events, LIMIT).await;
    assert_eq!(
        finished(&events),
        Some(TurnEndReason::Interrupted),
        "{events:?}"
    );
}
