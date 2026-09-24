use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::{
    message::ToolSpec,
    permission::{Action, resolve_path},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        ToolOutput {
            content: content.into(),
            is_error: false,
        }
    }

    pub fn error(content: impl Into<String>) -> Self {
        ToolOutput {
            content: content.into(),
            is_error: true,
        }
    }
}

/// Remembers the content hash of each file the model has read, so writes can detect stale views.
#[derive(Debug, Default)]
pub struct ReadTracker {
    hashes: Mutex<HashMap<PathBuf, String>>,
}

impl ReadTracker {
    fn hash(bytes: &[u8]) -> String {
        hex::encode(Sha256::digest(bytes))
    }

    pub fn record(&self, path: &Path, bytes: &[u8]) {
        self.hashes
            .lock()
            .expect("tracker lock")
            .insert(path.to_path_buf(), Self::hash(bytes));
    }

    /// `Ok` if the file was read in this session and is unchanged on disk since.
    pub fn check_fresh(&self, path: &Path, current: &[u8]) -> Result<(), String> {
        match self.hashes.lock().expect("tracker lock").get(path) {
            None => Err(format!(
                "{} exists but has not been read in this session; read it first",
                path.display()
            )),
            Some(hash) if *hash != Self::hash(current) => Err(format!(
                "{} changed on disk since it was read; read it again",
                path.display()
            )),
            Some(_) => Ok(()),
        }
    }
}

/// What every tool call can see: the workspace, read history, and the turn's cancellation token.
#[derive(Debug, Clone)]
pub struct ToolContext {
    pub workspace: PathBuf,
    pub tracker: Arc<ReadTracker>,
    pub cancel: CancellationToken,
}

impl ToolContext {
    pub fn new(workspace: &Path) -> Self {
        ToolContext {
            workspace: workspace
                .canonicalize()
                .unwrap_or_else(|_| workspace.to_path_buf()),
            tracker: Arc::default(),
            cancel: CancellationToken::new(),
        }
    }

    /// Resolves a path argument against the workspace, following symlinks.
    pub fn resolve(&self, path: &str) -> PathBuf {
        resolve_path(&self.workspace, Path::new(path))
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    /// What running this call would do, for the permission check. Called after schema validation.
    fn action(&self, args: &Value, ctx: &ToolContext) -> Action;
    async fn run(&self, args: Value, ctx: &ToolContext) -> ToolOutput;
}

/// Tools in a fixed order, so tool definitions are byte-identical across requests.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new(tools: Vec<Arc<dyn Tool>>) -> Self {
        ToolRegistry { tools }
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.iter().map(|t| t.spec()).collect()
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.iter().find(|t| t.spec().name == name).cloned()
    }
}
