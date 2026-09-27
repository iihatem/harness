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

use std::{path::Path, sync::Arc};

use harness_core::permission::FsAccess;
use harness_core::tool::CommandSandbox;

/// Runs commands directly, marks them with an env var, and reports "FAKE-DENIED" output as a denial.
#[derive(Debug)]
struct FakeSandbox;

impl CommandSandbox for FakeSandbox {
    fn name(&self) -> &'static str {
        "fake"
    }

    fn command(
        &self,
        _access: FsAccess,
        _workspace: &Path,
        program: &str,
        args: &[&str],
    ) -> std::io::Result<tokio::process::Command> {
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(args)
            .env("HARNESS_FAKE_SANDBOX", "1")
            .process_group(0);
        Ok(cmd)
    }

    fn is_denial(&self, _exit_code: Option<i32>, output: &str) -> bool {
        output.contains("FAKE-DENIED")
    }
}

fn sandboxed() -> (tempfile::TempDir, harness_core::tool::ToolContext) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = harness_core::tool::ToolContext::new(dir.path())
        .with_sandbox(Some(Arc::new(FakeSandbox)), FsAccess::WorkspaceWrite);
    (dir, ctx)
}

#[tokio::test]
async fn sandboxed_commands_go_through_the_sandbox() {
    let (_dir, ctx) = sandboxed();
    let out = BashTool
        .run(json!({"command": "echo \"[$HARNESS_FAKE_SANDBOX]\""}), &ctx)
        .await;
    assert!(out.content.contains("[1]"), "{}", out.content);
}

#[tokio::test]
async fn an_unsandboxed_rerun_bypasses_the_sandbox() {
    let (_dir, mut ctx) = sandboxed();
    ctx.unsandboxed = true;
    let out = BashTool
        .run(json!({"command": "echo \"[$HARNESS_FAKE_SANDBOX]\""}), &ctx)
        .await;
    assert!(out.content.contains("[]"), "{}", out.content);
}

#[tokio::test]
async fn sandbox_denials_are_flagged() {
    let (_dir, ctx) = sandboxed();
    let out = BashTool
        .run(json!({"command": "echo FAKE-DENIED; exit 1"}), &ctx)
        .await;
    assert!(out.is_error && out.sandbox_denied);
    assert!(
        out.content
            .contains("[the sandbox may have blocked part of this command]"),
        "{}",
        out.content
    );
    let ok = BashTool
        .run(json!({"command": "echo FAKE-DENIED"}), &ctx)
        .await;
    assert!(!ok.sandbox_denied, "successful commands are never denials");
}

#[tokio::test]
async fn commands_run_in_bash() {
    let (_dir, ctx) = ctx();
    let out = BashTool
        .run(json!({"command": "echo \"[${BASH_VERSION:+bash}]\""}), &ctx)
        .await;
    assert!(out.content.contains("[bash]"), "{}", out.content);
}

use std::sync::Mutex;

use harness_core::tool::{CommandGuard, GuardReport, SandboxedCommand};

/// Records what happened to the guards a [`GuardedSandbox`] hands out.
#[derive(Debug, Default)]
struct GuardLog {
    events: Mutex<Vec<String>>,
    /// The process ids `started` was given.
    pids: Mutex<Vec<u32>>,
}

impl GuardLog {
    fn events(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }

    fn pids(&self) -> Vec<u32> {
        self.pids.lock().unwrap().clone()
    }
}

/// Runs `program` (or `/nonexistent/program` with `broken`) directly, with a guard that logs
/// `finish` and returns `report` (or, with `panics`, a guard whose `finish` panics instead).
/// `denial` is what `is_denial` returns, so a test can force the exit-code heuristic to mark a
/// result `sandbox_denied` and check a blocking guard report clears that flag.
#[derive(Debug)]
struct GuardedSandbox {
    log: Arc<GuardLog>,
    report: Option<GuardReport>,
    broken: bool,
    fail_prepare: bool,
    denial: bool,
    panics: bool,
}

struct LoggingGuard {
    log: Arc<GuardLog>,
    report: Option<GuardReport>,
}

impl CommandGuard for LoggingGuard {
    fn started(&mut self, pid: u32) {
        self.log.events.lock().unwrap().push("started".into());
        self.log.pids.lock().unwrap().push(pid);
    }

    fn finish(self: Box<Self>) -> Option<GuardReport> {
        self.log.events.lock().unwrap().push("finished".into());
        self.report
    }
}

/// A guard whose `finish` panics, so `bash.rs`'s `spawn_blocking` join fails.
struct PanickingGuard;

impl CommandGuard for PanickingGuard {
    fn finish(self: Box<Self>) -> Option<GuardReport> {
        panic!("the guard exploded");
    }
}

impl CommandSandbox for GuardedSandbox {
    fn name(&self) -> &'static str {
        "guarded"
    }

    fn command(
        &self,
        _access: FsAccess,
        _workspace: &Path,
        program: &str,
        args: &[&str],
    ) -> std::io::Result<tokio::process::Command> {
        let program = if self.broken {
            "/nonexistent/program"
        } else {
            program
        };
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(args).process_group(0);
        Ok(cmd)
    }

    fn is_denial(&self, _exit_code: Option<i32>, _output: &str) -> bool {
        self.denial
    }

    fn prepare(
        &self,
        access: FsAccess,
        workspace: &Path,
        program: &str,
        args: &[&str],
    ) -> std::io::Result<SandboxedCommand> {
        if self.fail_prepare {
            return Err(std::io::Error::other("no way"));
        }
        self.log.events.lock().unwrap().push("prepared".into());
        let guard: Box<dyn CommandGuard> = if self.panics {
            Box::new(PanickingGuard)
        } else {
            Box::new(LoggingGuard {
                log: self.log.clone(),
                report: self.report.clone(),
            })
        };
        Ok(SandboxedCommand {
            command: self.command(access, workspace, program, args)?,
            guard: Some(guard),
        })
    }
}

fn guarded(
    report: Option<GuardReport>,
    broken: bool,
) -> (tempfile::TempDir, ToolContext, Arc<GuardLog>) {
    guarded_with(report, broken, false)
}

/// As `guarded`, but also controls what `is_denial` returns.
fn guarded_with(
    report: Option<GuardReport>,
    broken: bool,
    denial: bool,
) -> (tempfile::TempDir, ToolContext, Arc<GuardLog>) {
    let dir = tempfile::tempdir().unwrap();
    let log = Arc::new(GuardLog::default());
    let sandbox = GuardedSandbox {
        log: log.clone(),
        report,
        broken,
        fail_prepare: false,
        denial,
        panics: false,
    };
    let ctx = ToolContext::new(dir.path())
        .with_sandbox(Some(Arc::new(sandbox)), FsAccess::WorkspaceWrite);
    (dir, ctx, log)
}

fn blocking_report() -> Option<GuardReport> {
    Some(GuardReport {
        message: "[the sandbox undid changes: .git/hooks/pre-commit]\n".into(),
        blocked: true,
    })
}

#[tokio::test]
async fn a_guard_report_is_appended_and_a_blocking_one_marks_the_command_blocked() {
    let (_dir, ctx, log) = guarded(blocking_report(), false);
    let out = BashTool.run(json!({"command": "echo hi"}), &ctx).await;
    assert_eq!(log.events(), ["prepared", "started", "finished"]);
    assert!(out.is_error && out.guard_blocked && !out.sandbox_denied);
    assert_eq!(
        out.content,
        "exit code 0\nhi\n\n[the sandbox undid changes: .git/hooks/pre-commit]\n"
    );
}

#[tokio::test]
async fn the_guard_is_told_the_pid_of_the_shell_it_guards_before_it_finishes() {
    let (_dir, ctx, log) = guarded(None, false);
    let out = BashTool
        .run(json!({"command": "echo \"[$$]\""}), &ctx)
        .await;
    assert_eq!(log.events(), ["prepared", "started", "finished"]);
    let pids = log.pids();
    assert_eq!(pids.len(), 1);
    assert!(
        out.content.contains(&format!("[{}]", pids[0])),
        "{} (started: {pids:?})",
        out.content
    );
}

#[tokio::test]
async fn a_report_that_blocks_nothing_leaves_the_result_as_it_was() {
    let report = GuardReport {
        message: "[before this command ran, harness found …]\n".into(),
        blocked: false,
    };
    let (_dir, ctx, _log) = guarded(Some(report), false);
    let out = BashTool.run(json!({"command": "echo hi"}), &ctx).await;
    assert!(
        !out.is_error && !out.guard_blocked && !out.sandbox_denied,
        "{}",
        out.content
    );
    assert!(
        out.content
            .ends_with("[before this command ran, harness found …]\n")
    );
}

#[tokio::test]
async fn no_report_leaves_the_output_unchanged() {
    let (_dir, ctx, log) = guarded(None, false);
    let out = BashTool.run(json!({"command": "echo hi"}), &ctx).await;
    assert_eq!(log.events(), ["prepared", "started", "finished"]);
    assert_eq!(out.content, "exit code 0\nhi\n");
}

#[tokio::test]
async fn the_guard_finishes_after_a_timeout() {
    let (_dir, ctx, log) = guarded(blocking_report(), false);
    let out = BashTool
        .run(json!({"command": "sleep 30", "timeout_secs": 1}), &ctx)
        .await;
    assert_eq!(log.events(), ["prepared", "started", "finished"]);
    assert!(out.content.contains("timed out"), "{}", out.content);
    assert!(out.guard_blocked && !out.sandbox_denied);
}

#[tokio::test]
async fn the_guard_finishes_after_an_interrupt() {
    let (_dir, ctx, log) = guarded(None, false);
    let cancel = ctx.cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        cancel.cancel();
    });
    let out = BashTool.run(json!({"command": "sleep 30"}), &ctx).await;
    assert_eq!(log.events(), ["prepared", "started", "finished"]);
    assert!(out.content.contains("interrupted"), "{}", out.content);
}

#[tokio::test]
async fn the_guard_finishes_when_the_command_cannot_start() {
    let (_dir, ctx, log) = guarded(None, true);
    let out = BashTool.run(json!({"command": "echo hi"}), &ctx).await;
    assert_eq!(log.events(), ["prepared", "finished"]);
    assert!(
        out.is_error && out.content.contains("failed to start"),
        "{}",
        out.content
    );
}

#[tokio::test]
async fn an_unsandboxed_rerun_starts_no_guard() {
    let (_dir, mut ctx, log) = guarded(blocking_report(), false);
    ctx.unsandboxed = true;
    let out = BashTool.run(json!({"command": "echo hi"}), &ctx).await;
    assert!(log.events().is_empty());
    assert!(!out.sandbox_denied);
}

#[tokio::test]
async fn a_sandbox_that_cannot_prepare_runs_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let sandbox = GuardedSandbox {
        log: Arc::new(GuardLog::default()),
        report: None,
        broken: false,
        fail_prepare: true,
        denial: false,
        panics: false,
    };
    let ctx = ToolContext::new(dir.path())
        .with_sandbox(Some(Arc::new(sandbox)), FsAccess::WorkspaceWrite);
    let out = BashTool
        .run(json!({"command": "touch made.txt"}), &ctx)
        .await;
    assert!(out.is_error);
    assert_eq!(out.content, "failed to prepare the sandbox: no way");
    assert!(!dir.path().join("made.txt").exists());
}

#[tokio::test]
async fn a_blocking_guard_report_clears_a_heuristic_sandbox_denial() {
    // `exit 1` plus `denial: true` makes the exit-code heuristic in `run_command` set
    // `sandbox_denied`; the blocking guard report must clear it and set `guard_blocked` instead,
    // since a guard-blocked result is never offered a re-run.
    let (_dir, ctx, log) = guarded_with(blocking_report(), false, true);
    let out = BashTool.run(json!({"command": "exit 1"}), &ctx).await;
    assert_eq!(log.events(), ["prepared", "started", "finished"]);
    assert!(
        out.guard_blocked && out.is_error && !out.sandbox_denied,
        "{}",
        out.content
    );
}

#[tokio::test]
async fn a_panicking_guard_is_reported_blocked_not_denied() {
    let dir = tempfile::tempdir().unwrap();
    let sandbox = GuardedSandbox {
        log: Arc::new(GuardLog::default()),
        report: None,
        broken: false,
        fail_prepare: false,
        denial: false,
        panics: true,
    };
    let ctx = ToolContext::new(dir.path())
        .with_sandbox(Some(Arc::new(sandbox)), FsAccess::WorkspaceWrite);
    let out = BashTool.run(json!({"command": "echo hi"}), &ctx).await;
    assert!(
        out.is_error && out.guard_blocked && !out.sandbox_denied,
        "{}",
        out.content
    );
    assert!(
        out.content.contains("could not check git metadata"),
        "{}",
        out.content
    );
}

/// With the test process a child subreaper, as harness is in the Linux basic tier, the members of
/// a timed-out command's process group reparent to it when the shell dies, and stay zombies in
/// the group until reaped. `reap` reaps them while it waits for the group, so none is left.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_timed_out_commands_orphaned_group_members_are_reaped() {
    nix::sys::prctl::set_child_subreaper(true).expect("become a child subreaper");
    let (_dir, ctx) = ctx();
    let out = BashTool
        .run(
            json!({"command": "echo \"[$$]\"; sleep 30 & sleep 30 & wait", "timeout_secs": 1}),
            &ctx,
        )
        .await;
    assert!(out.content.contains("timed out"), "{}", out.content);
    let pgid: i32 = out
        .content
        .split_once('[')
        .and_then(|(_, rest)| rest.split_once(']'))
        .and_then(|(pid, _)| pid.parse().ok())
        .unwrap_or_else(|| panic!("no pid in {}", out.content));
    // A group with only zombies left still exists: `killpg` reaches it.
    assert!(
        nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pgid), None).is_err(),
        "members of the command's group are left over"
    );
}
