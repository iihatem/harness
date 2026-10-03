//! Diagnostics after an edit: what a language-server front end tells the model about the files
//! an edit tool changed.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;

use crate::{permission::FsAccess, tool::CommandSandbox};

/// The files an edit changed, and how a server for them may be run.
pub struct EditedFiles<'a> {
    pub paths: &'a [PathBuf],
    pub workspace: &'a Path,
    /// The sandbox bash runs in now, when there is one.
    pub sandbox: Option<Arc<dyn CommandSandbox>>,
    /// What sandboxed commands may write now.
    pub access: FsAccess,
    /// Whether a server may run with no sandbox: the mode is `full-access`.
    pub unsandboxed_ok: bool,
}

#[async_trait]
pub trait Diagnostics: Send + Sync {
    /// What to append to the edit's result about `edited`: the errors found in the files, or why
    /// there is nothing to say. `None` for nothing.
    async fn after_edit(&self, edited: &EditedFiles<'_>) -> Option<String>;
    /// Stops what runs for the session that is ending, without waiting (`/new`).
    fn reset(&self);
    /// Stops everything and waits, as a session that ends does.
    async fn shutdown(&self);
}
