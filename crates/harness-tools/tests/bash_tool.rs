use std::time::{Duration, Instant};

use harness_core::tool::{Tool, ToolContext};
use harness_tools::BashTool;
use serde_json::json;

fn ctx() -> (tempfile::TempDir, ToolContext) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ToolContext::new(dir.path());
    (dir, ctx)
}

#[tokio::test]
async fn combines_stdout_and_stderr_and_reports_the_exit_code() {
    let (_dir, ctx) = ctx();
    let out = BashTool
        .run(json!({"command": "echo out; echo err >&2"}), &ctx)
        .await;
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.starts_with("exit code 0\n"));
    assert!(out.content.contains("out") && out.content.contains("err"));
}

#[tokio::test]
async fn runs_in_the_workspace() {
    let (dir, ctx) = ctx();
    std::fs::write(dir.path().join("marker.txt"), "").unwrap();
    let out = BashTool.run(json!({"command": "ls"}), &ctx).await;
    assert!(out.content.contains("marker.txt"));
}

#[tokio::test]
async fn nonzero_exit_is_an_error_result() {
    let (_dir, ctx) = ctx();
    let out = BashTool
        .run(json!({"command": "echo failing; exit 3"}), &ctx)
        .await;
    assert!(out.is_error);
    assert!(out.content.starts_with("exit code 3\n"));
    assert!(out.content.contains("failing"));
}

#[tokio::test]
async fn timeout_kills_the_whole_process_group() {
    let (dir, ctx) = ctx();
    let started = Instant::now();
    let out = BashTool
        .run(
            json!({"command": "sleep 30 & echo $! > child.pid; wait", "timeout_secs": 1}),
            &ctx,
        )
        .await;
    assert!(out.is_error);
    assert!(
        out.content.contains("timed out after 1s"),
        "{}",
        out.content
    );
    assert!(started.elapsed() < Duration::from_secs(5));

    let pid: i32 = std::fs::read_to_string(dir.path().join("child.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let mut gone = false;
    for _ in 0..20 {
        if nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err() {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(gone, "background child {pid} survived the timeout");
}

// Review Focus: output produced before a timeout must not be lost.
#[tokio::test]
async fn timeout_preserves_output_produced_before_it_fired() {
    let (_dir, ctx) = ctx();
    let out = BashTool
        .run(
            json!({"command": "echo started; sleep 30", "timeout_secs": 1}),
            &ctx,
        )
        .await;
    assert!(out.is_error);
    assert!(out.content.contains("timed out"), "{}", out.content);
    assert!(out.content.contains("started"), "{}", out.content);
}

// Review Focus: a background process holding stdout open.
#[tokio::test]
async fn background_process_holding_stdout_does_not_hang_past_the_timeout() {
    let (_dir, ctx) = ctx();
    let started = Instant::now();
    let out = BashTool
        .run(
            json!({"command": "sleep 30 & echo started", "timeout_secs": 1}),
            &ctx,
        )
        .await;
    assert!(out.is_error);
    assert!(out.content.contains("timed out"));
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn cancellation_interrupts_a_running_command() {
    let (_dir, ctx) = ctx();
    let cancel = ctx.cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        cancel.cancel();
    });
    let started = Instant::now();
    let out = BashTool.run(json!({"command": "sleep 600"}), &ctx).await;
    assert!(out.is_error);
    assert!(out.content.contains("interrupted"));
    assert!(started.elapsed() < Duration::from_secs(2));
}
