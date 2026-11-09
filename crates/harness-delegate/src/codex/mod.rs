//! Codex, driven as `codex app-server` over stdio.

pub mod argv;
pub mod protocol;

use async_trait::async_trait;
use tokio::process::Command;

use crate::{
    BackendKind, DelegateBackend, DelegateError, DelegateRun, StartRequest,
    detect::{DetectOptions, detect},
    driver::{DriverConfig, Keep, spawn_run},
    env::child_env,
};

use protocol::{CodexParams, CodexProtocol};

/// The longest line read from codex: a command's output can be large.
const MAX_LINE: usize = 16 * 1024 * 1024;

/// Starts `codex app-server` as a child process. Checks the binary's version first.
#[derive(Debug, Default, Clone, Copy)]
pub struct CodexBackend;

#[async_trait]
impl DelegateBackend for CodexBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Codex
    }

    async fn start(&self, request: StartRequest) -> Result<DelegateRun, DelegateError> {
        let cwd = request.workspace.display().to_string();
        let mut params = CodexParams::new(request.task, cwd, "");
        params.mode = request.mode;
        params.full_access_confirmed = request.full_access_confirmed;
        params.model = request.model;
        params.resume = request.resume;
        params.extra_dirs = request
            .extra_dirs
            .iter()
            .map(|d| d.display().to_string())
            .collect();
        // Refuse before looking for anything: a missing confirmation needs no binary.
        let _ = CodexProtocol::new(params.clone())?;
        let found = detect(
            BackendKind::Codex,
            &DetectOptions {
                binary: request.binary.clone(),
                path_var: request.path_var.clone(),
                allow_untested: request.allow_untested,
                ..DetectOptions::default()
            },
        )
        .await?;
        params.child_version = found.version.to_string();
        let protocol = CodexProtocol::new(params)?;
        let parent = request
            .parent_env
            .clone()
            .unwrap_or_else(|| std::env::vars_os().collect());
        let env = child_env(BackendKind::Codex, &request.auth, parent);
        // Codex starts the MCP servers of the user's own config: harness decides what a delegate
        // may reach, so they are switched off, by the names codex itself lists.
        let servers = user_mcp_servers(&found.path, &env, &request.workspace).await;
        let mut command = Command::new(&found.path);
        command
            .args(argv::build(&servers))
            .current_dir(&request.workspace)
            .env_clear()
            .envs(env);
        spawn_run(
            command,
            protocol,
            DriverConfig {
                label: "codex".into(),
                grace: request.grace,
                redactor: request.redactor,
                max_line: MAX_LINE,
                keep: Keep::default(),
            },
        )
    }
}

/// The names of the MCP servers codex would start for the user (`codex mcp list --json`, which
/// reads the config but starts nothing). Best effort: when it cannot be run or read, there are
/// no names, and the servers are not switched off (the plan lists this as a limit).
async fn user_mcp_servers(
    binary: &std::path::Path,
    env: &[(std::ffi::OsString, std::ffi::OsString)],
    workspace: &std::path::Path,
) -> Vec<String> {
    use std::process::Stdio;
    let mut command = Command::new(binary);
    command
        .args(["mcp", "list", "--json"])
        .current_dir(workspace)
        .env_clear()
        .envs(env.iter().cloned())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .kill_on_drop(true);
    let Ok(child) = command.spawn() else {
        return Vec::new();
    };
    let pid = child.id();
    let listed =
        tokio::time::timeout(std::time::Duration::from_secs(10), child.wait_with_output()).await;
    let output = match listed {
        Ok(Ok(output)) if output.status.success() => output.stdout,
        _ => {
            if let Some(pid) = pid.and_then(|p| i32::try_from(p).ok()) {
                let _ = nix::sys::signal::killpg(
                    nix::unistd::Pid::from_raw(pid),
                    nix::sys::signal::Signal::SIGKILL,
                );
            }
            return Vec::new();
        }
    };
    let Ok(list) = serde_json::from_slice::<serde_json::Value>(&output) else {
        return Vec::new();
    };
    list.as_array()
        .into_iter()
        .flatten()
        .filter_map(|server| server["name"].as_str())
        .filter(|name| !name.chars().any(char::is_control))
        .map(String::from)
        .collect()
}
