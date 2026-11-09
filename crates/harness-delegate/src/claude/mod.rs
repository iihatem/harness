//! Claude Code, driven as `claude -p` with stream-json in and out.

pub mod argv;
pub mod protocol;

use async_trait::async_trait;
use tokio::process::Command;

use crate::{
    BackendKind, DelegateBackend, DelegateError, DelegateRun, StartRequest,
    detect::{DetectOptions, detect},
    driver::{DriverConfig, spawn_run},
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
        let args = argv::build(&request)?;
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
            },
        )
    }
}
