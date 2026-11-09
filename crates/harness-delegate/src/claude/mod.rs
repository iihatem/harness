//! Claude Code, driven as `claude -p` with stream-json in and out.

pub mod argv;
pub mod protocol;

use async_trait::async_trait;
use tokio::process::Command;

use crate::{
    BackendKind, DelegateBackend, DelegateError, DelegateRun, StartRequest,
    detect::{DetectOptions, detect},
    driver::{DriverConfig, Keep, spawn_run},
    env::{AuthMode, child_env},
};

use protocol::{AuthKind, ClaudeParams, ClaudeProtocol};

/// The longest line read from claude: a tool result can be large.
const MAX_LINE: usize = 16 * 1024 * 1024;

/// Starts `claude` as a child process. Checks the binary's version first.
#[derive(Debug, Default, Clone, Copy)]
pub struct ClaudeBackend;

#[async_trait]
impl DelegateBackend for ClaudeBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Claude
    }

    async fn start(&self, request: StartRequest) -> Result<DelegateRun, DelegateError> {
        // Refuse before looking for anything: this needs no binary.
        argv::build(&request)?;
        let found = detect(
            BackendKind::Claude,
            &DetectOptions {
                binary: request.binary.clone(),
                path_var: request.path_var.clone(),
                allow_untested: request.allow_untested,
                ..DetectOptions::default()
            },
        )
        .await?;
        // The instructions file lives as long as the run (the driver holds it) and goes with it.
        let (instructions, keep) = match request.instructions.as_deref().filter(|i| !i.is_empty()) {
            Some(text) => {
                let file = write_instructions(text)?;
                let path = file.to_path_buf();
                (Some(path), Keep::new(file))
            }
            None => (None, Keep::default()),
        };
        let args = argv::build_with(&request, instructions.as_deref())?;
        let parent = request
            .parent_env
            .clone()
            .unwrap_or_else(|| std::env::vars_os().collect());
        let mut command = Command::new(&found.path);
        command
            .args(args)
            .current_dir(&request.workspace)
            .env_clear()
            .envs(child_env(BackendKind::Claude, &request.auth, parent));
        let protocol = ClaudeProtocol::new(ClaudeParams {
            auth: match request.auth {
                AuthMode::Subscription => AuthKind::Subscription,
                AuthMode::ApiKey(_) => AuthKind::ApiKey,
            },
            cost_baseline_usd: request.cost_baseline_usd,
            ..ClaudeParams::new(request.task, found.version.to_string())
        });
        spawn_run(
            command,
            protocol,
            DriverConfig {
                label: "claude".into(),
                grace: request.grace,
                redactor: request.redactor,
                max_line: MAX_LINE,
                keep,
            },
        )
    }
}

/// The instructions in a private temporary file (mode 0600) that is removed when dropped.
fn write_instructions(text: &str) -> Result<tempfile::TempPath, DelegateError> {
    use std::io::Write;
    let spawn = |source| DelegateError::Spawn {
        binary: "claude".into(),
        source,
    };
    let mut file = tempfile::Builder::new()
        .prefix("harness-instructions-")
        .suffix(".md")
        .tempfile()
        .map_err(spawn)?;
    file.write_all(text.as_bytes()).map_err(spawn)?;
    file.flush().map_err(spawn)?;
    Ok(file.into_temp_path())
}
