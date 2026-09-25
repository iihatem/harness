use std::{process::Stdio, sync::Arc, sync::Mutex, time::Duration};

use async_trait::async_trait;
use harness_core::{
    message::ToolSpec,
    permission::Action,
    tool::{Tool, ToolContext, ToolOutput},
};
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;

const DEFAULT_TIMEOUT_SECS: u64 = 120;
const MAX_TIMEOUT_SECS: u64 = 600;

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
        let mut child = match tokio::process::Command::new("sh")
            .arg("-c")
            .arg(format!("exec 2>&1\n{command}"))
            .current_dir(&ctx.workspace)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
        {
            Ok(child) => child,
            Err(e) => return ToolOutput::error(format!("failed to start sh: {e}")),
        };
        let pgid = child.id().map(|id| id as i32);
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
                let text = partial_text(&output);
                match status {
                    Ok(status) => {
                        let code = status.code().map_or_else(|| "signal".to_string(), |c| c.to_string());
                        let body = format!("exit code {code}\n{text}");
                        if status.success() { ToolOutput::ok(body) } else { ToolOutput::error(body) }
                    }
                    Err(e) => ToolOutput::error(format!("failed to wait for command: {e}\n{text}")),
                }
            }
            _ = tokio::time::sleep(Duration::from_secs(secs)) => {
                kill_group(pgid);
                reader.abort();
                let text = partial_text(&output);
                ToolOutput::error(format!("command timed out after {secs}s and was terminated\n{text}"))
            }
            _ = ctx.cancel.cancelled() => {
                kill_group(pgid);
                reader.abort();
                let text = partial_text(&output);
                ToolOutput::error(format!("command interrupted by the user\n{text}"))
            }
        }
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
