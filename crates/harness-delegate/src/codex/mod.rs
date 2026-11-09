//! Codex, driven as `codex app-server` over stdio.

pub mod argv;
pub mod protocol;

use async_trait::async_trait;
use tokio::process::Command;

use crate::{
    BackendKind, DelegateBackend, DelegateError, DelegateRun, StartRequest,
    detect::{DetectOptions, detect},
    driver::{DriverConfig, spawn_run},
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
        let mut command = Command::new(&found.path);
        command
            .args(argv::build())
            .current_dir(&request.workspace)
            .env_clear()
            .envs(child_env(BackendKind::Codex, &request.auth, parent));
        spawn_run(
            command,
            protocol,
            DriverConfig {
                label: "codex".into(),
                grace: request.grace,
                redactor: request.redactor,
                max_line: MAX_LINE,
            },
        )
    }
}
