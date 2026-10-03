use std::{path::Path, process::Stdio, sync::Arc, sync::Mutex, time::Duration};

use async_trait::async_trait;
use harness_core::{
    message::ToolSpec,
    permission::{Action, FsAccess},
    tool::{
        CommandGuard, CommandSandbox, GuardReport, SandboxedCommand, Tool, ToolContext, ToolOutput,
    },
};
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;

const DEFAULT_TIMEOUT_SECS: u64 = 120;
const MAX_TIMEOUT_SECS: u64 = 600;

/// Where bash is looked for, in order. Fixed absolute paths, so no `bash` on `PATH` is ever used.
const BASH_PATHS: [&str; 3] = [
    "/bin/bash",
    "/usr/bin/bash",
    "/run/current-system/sw/bin/bash",
];

/// The shell to run commands with, and its arguments before the script: the first bash in
/// `BASH_PATHS` that `is_file` finds, without startup files, else `/bin/sh`.
fn shell(is_file: impl Fn(&Path) -> bool) -> (&'static str, Vec<&'static str>) {
    match BASH_PATHS.into_iter().find(|p| is_file(Path::new(p))) {
        Some(bash) => (bash, vec!["--noprofile", "--norc", "-c"]),
        None => ("/bin/sh", vec!["-c"]),
    }
}

pub struct BashTool;

#[async_trait]
impl Tool for BashTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bash".into(),
            description: "Run a non-interactive shell command in the workspace. Returns the exit code and combined stdout/stderr. Default timeout 120s, max 600s.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "timeout_secs": {"type": "integer", "minimum": 1, "maximum": 600}
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        }
    }

    fn action(&self, args: &Value, _ctx: &ToolContext) -> Action {
        Action::Bash(args["command"].as_str().unwrap_or_default().to_string())
    }

    async fn run(&self, args: Value, ctx: &ToolContext) -> ToolOutput {
        let command = args["command"].as_str().unwrap_or_default();
        let secs = args["timeout_secs"]
            .as_u64()
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .clamp(1, MAX_TIMEOUT_SECS);

        // `exec 2>&1` merges stderr into stdout for the whole script, preserving interleaving.
        let script = format!("exec 2>&1\n{command}");
        let (shell, mut args) = shell(Path::is_file);
        args.push(&script);

        let sandbox = ctx.sandbox.clone().filter(|_| !ctx.unsandboxed);
        let (cmd, mut guard) = match &sandbox {
            Some(sandbox) => {
                match prepare(sandbox.clone(), ctx.access, &ctx.workspace, shell, &args).await {
                    Ok(prepared) => (prepared.command, prepared.guard),
                    Err(e) => {
                        return ToolOutput::error(format!("failed to prepare the sandbox: {e}"));
                    }
                }
            }
            None => {
                let mut cmd = tokio::process::Command::new(shell);
                cmd.args(&args).process_group(0);
                (cmd, None)
            }
        };
        let mut output =
            run_command(cmd, ctx, shell, secs, sandbox.as_deref(), guard.as_mut()).await;
        // The guard is finished however the command ended: exited, timed out, interrupted, or
        // never started.
        if let Some(guard) = guard
            && let Some(report) = finish(guard).await
        {
            output.content.push('\n');
            output.content.push_str(&report.message);
            if report.blocked {
                output.is_error = true;
                output.guard_blocked = true;
                // A guard-blocked result is never offered a re-run: it would redo exactly what
                // the guard just undid.
                output.sandbox_denied = false;
            }
        }
        output
    }
}

/// The sandboxed command and its guard, prepared off the async runtime: the guard walks the
/// workspace, which takes a while in a large one.
async fn prepare(
    sandbox: Arc<dyn CommandSandbox>,
    access: FsAccess,
    workspace: &Path,
    shell: &str,
    args: &[&str],
) -> std::io::Result<SandboxedCommand> {
    let workspace = workspace.to_path_buf();
    let shell = shell.to_string();
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    tokio::task::spawn_blocking(move || {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        sandbox.prepare(access, &workspace, &shell, &args)
    })
    .await
    .map_err(std::io::Error::other)?
}

/// Finishes the guard off the async runtime. A guard that panics leaves the command's effect on
/// git metadata unchecked, so the command counts as blocked.
async fn finish(guard: Box<dyn CommandGuard>) -> Option<GuardReport> {
    match tokio::task::spawn_blocking(move || guard.finish()).await {
        Ok(report) => report,
        Err(e) => Some(GuardReport {
            message: format!("[harness could not check git metadata after this command: {e}]\n"),
            blocked: true,
        }),
    }
}

/// Runs `cmd` until it exits, times out after `secs`, or the user interrupts it, and reports its
/// exit code and output. `guard` is told the pid of the process spawned.
async fn run_command(
    mut cmd: tokio::process::Command,
    ctx: &ToolContext,
    shell: &str,
    secs: u64,
    sandbox: Option<&dyn CommandSandbox>,
    guard: Option<&mut Box<dyn CommandGuard>>,
) -> ToolOutput {
    let mut child = match cmd
        .current_dir(&ctx.workspace)
        .env_remove("BASH_ENV")
        .env_remove("ENV")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(e) => return ToolOutput::error(format!("failed to start {shell}: {e}")),
    };
    // Before anything else: the guard must know which process this task waits for, so it never
    // reaps it itself.
    if let (Some(guard), Some(pid)) = (guard, child.id()) {
        guard.started(pid);
    }
    let pgid = child.id().map(|id| id as i32);
    // Dropped with this future while the command runs (a panic unwinding, the runtime shutting
    // down), it kills the command's whole group; `kill_on_drop` kills only the shell.
    let mut group = GroupGuard(pgid);
    let mut stdout = child.stdout.take().expect("stdout is piped");

    // Read stdout on a separate task into a buffer that outlives the `select!` below: if a
    // branch other than `finished` wins (timeout/interrupt), `finished` (and any buffer local
    // to it) is dropped, but this task keeps draining into `output`, so whatever the command
    // already printed is not lost.
    let output = Arc::new(Mutex::new(Vec::new()));
    let reader_output = output.clone();
    let mut reader = tokio::spawn(async move {
        let mut chunk = [0u8; 8192];
        loop {
            match stdout.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => reader_output
                    .lock()
                    .expect("bash output lock")
                    .extend_from_slice(&chunk[..n]),
            }
        }
    });
    let partial_text = |output: &Arc<Mutex<Vec<u8>>>| {
        let buf = output.lock().expect("bash output lock");
        String::from_utf8_lossy(&buf).into_owned()
    };

    let finished = async {
        let status = child.wait().await;
        // Drain whatever is left so a fast-exiting command's full output is captured.
        let _ = (&mut reader).await;
        status
    };

    tokio::select! {
        status = finished => {
            // It ended by itself: what it left running in the background is left alone.
            group.disarm();
            let text = partial_text(&output);
            match status {
                Ok(status) => {
                    let code = status.code().map_or_else(|| "signal".to_string(), |c| c.to_string());
                    let body = format!("exit code {code}\n{text}");
                    if status.success() {
                        ToolOutput::ok(body)
                    } else if sandbox.is_some_and(|s| s.is_denial(status.code(), &text)) {
                        let mut out = ToolOutput::error(format!("{body}\n[the sandbox may have blocked part of this command]"));
                        out.sandbox_denied = true;
                        out
                    } else {
                        ToolOutput::error(body)
                    }
                }
                Err(e) => ToolOutput::error(format!("failed to wait for command: {e}\n{text}")),
            }
        }
        _ = tokio::time::sleep(Duration::from_secs(secs)) => {
            kill_group(pgid);
            group.disarm();
            reap(&mut child, pgid).await;
            reader.abort();
            let text = partial_text(&output);
            ToolOutput::error(format!("command timed out after {secs}s and was terminated\n{text}"))
        }
        _ = ctx.cancel.cancelled() => {
            kill_group(pgid);
            group.disarm();
            reap(&mut child, pgid).await;
            reader.abort();
            let text = partial_text(&output);
            ToolOutput::error(format!("command interrupted by the user\n{text}"))
        }
    }
}

/// Kills a command's process group when dropped, unless disarmed first: the command ended, or
/// was killed already.
struct GroupGuard(Option<i32>);

impl GroupGuard {
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        kill_group(self.0);
    }
}

/// Kills the command and everything it started (it runs in its own process group).
fn kill_group(pgid: Option<i32>) {
    if let Some(pgid) = pgid {
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(pgid),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
}

/// Waits for the shell itself to be reaped, then polls the process group for up to about 1s,
/// so no member of it (a background job included) is still mid-syscall when the guard's final
/// check runs. `kill_group` must already have sent the group SIGKILL. If waiting for the shell
/// failed, the shell may not have been reaped, and waiting on its group could take its exit
/// status: then it does neither.
async fn reap(child: &mut tokio::process::Child, pgid: Option<i32>) {
    if child.wait().await.is_err() {
        return;
    }
    let Some(pgid) = pgid else { return };
    let pgid = nix::unistd::Pid::from_raw(pgid);
    let deadline = tokio::time::Instant::now() + Duration::from_millis(1000);
    loop {
        reap_orphaned_members(pgid);
        if nix::sys::signal::killpg(pgid, None).is_err() || tokio::time::Instant::now() >= deadline
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Reaps the members of the process group `pgid` that are harness's own children. On Linux,
/// harness is a child subreaper in the basic tier of git-metadata protection, so once the shell
/// dies the rest of its group reparents to harness, and each member stays a zombie, still in the
/// group, until harness reaps it. Only this group is waited for, never `-1`: its one member
/// harness spawned, and tokio waits for, is the shell, which `reap` has reaped already.
fn reap_orphaned_members(pgid: nix::unistd::Pid) {
    use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
    // A group id of 1 would make this `waitpid(-1)`.
    if pgid.as_raw() <= 1 {
        return;
    }
    let group = nix::unistd::Pid::from_raw(-pgid.as_raw());
    // Each round reaps one zombie; the group, killed, makes no new ones.
    while let Ok(status) = waitpid(group, Some(WaitPidFlag::WNOHANG)) {
        if status == WaitStatus::StillAlive {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// When waiting for the shell fails, the shell may not have been reaped, and a wait on its
    /// group could take its exit status: `reap` waits for no member of the group then.
    #[tokio::test]
    async fn reap_waits_for_no_group_member_once_waiting_for_the_shell_failed() {
        use std::os::unix::process::CommandExt;

        use nix::sys::wait::{WaitStatus, waitpid};
        use nix::unistd::Pid;
        let mut shell = tokio::process::Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let pgid = shell.id().unwrap() as i32;
        // A child of this process in the shell's group, which someone else waits for.
        let mut member = std::process::Command::new("true")
            .process_group(pgid)
            .spawn()
            .unwrap();
        // Reaped behind tokio's back, so `wait` fails.
        shell.start_kill().unwrap();
        assert!(matches!(
            waitpid(Pid::from_raw(pgid), None),
            Ok(WaitStatus::Signaled(..))
        ));
        reap(&mut shell, Some(pgid)).await;
        let status = member
            .wait()
            .expect("reap took the exit status of a group member it does not wait for");
        assert!(status.success());
    }

    // Final review M3: a command dropped while it runs (the agent's task panicked and unwound, or
    // was dropped as harness exits) killed only its shell; what the shell started in its group
    // ran on. The whole group is killed.
    #[tokio::test]
    async fn a_command_dropped_while_it_runs_takes_its_process_group_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path());
        let running = tokio::spawn(async move {
            BashTool
                .run(json!({"command": "sleep 30 & echo $! > child; wait"}), &ctx)
                .await
        });
        let file = dir.path().join("child");
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let child = loop {
            if let Ok(pid) = std::fs::read_to_string(&file)
                && pid.ends_with('\n')
            {
                break nix::unistd::Pid::from_raw(pid.trim().parse().unwrap());
            }
            assert!(std::time::Instant::now() < deadline, "never started");
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        running.abort();
        assert!(running.await.unwrap_err().is_cancelled());
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while nix::sys::signal::kill(child, None).is_ok() {
            assert!(
                std::time::Instant::now() < deadline,
                "what the command started still runs"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[test]
    fn bash_is_looked_for_at_fixed_paths_before_falling_back_to_sh() {
        let only = |path: &'static str| move |p: &Path| p == Path::new(path);
        for bash in [
            "/bin/bash",
            "/usr/bin/bash",
            "/run/current-system/sw/bin/bash",
        ] {
            assert_eq!(
                shell(only(bash)),
                (bash, vec!["--noprofile", "--norc", "-c"])
            );
        }
        assert_eq!(shell(|_| true).0, "/bin/bash");
        assert_eq!(shell(only("/usr/local/bin/bash")), ("/bin/sh", vec!["-c"]));
        assert_eq!(shell(|_| false), ("/bin/sh", vec!["-c"]));
    }
}
