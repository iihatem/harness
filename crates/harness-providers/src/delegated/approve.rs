//! What a delegated child asks leave to do, put to harness's permission engine as the same
//! actions its own tools would be checked as: a Bash command is a `bash` action, a write a
//! `write` action on its path, a read a `read` action. A tool the engine has no action for is
//! always a question.
//!
//! Two refusals do not wait for the engine: in `plan` and `read-only` mode Codex runs in a
//! read-only sandbox, so anything it asks leave for is outside it (the transport's notes), and
//! `ExitPlanMode` is answered with a note instead of letting the child leave plan mode on its
//! own: the plan is shown to the user as the reply (the existing Build/Edit/Keep choice follows).

use std::path::{Path, PathBuf};

use harness_core::{delegated::GateRequest, permission::Action, permission::Mode};
use harness_delegate::{
    BackendKind,
    event::{DelegateApproval, DelegateApprovalKind},
};
use serde_json::{Value, json};

/// What to tell Claude when it calls `ExitPlanMode`.
pub const PLAN_NOTE: &str = "harness shows the plan to the user and asks for approval itself: write the finished plan as your reply now, and stop. Do not call ExitPlanMode.";

/// What harness does with one approval.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// Ask the permission engine (and the user, when it says so).
    Ask(GateRequest),
    /// Refuse at once, with this to tell the child.
    Refuse(String),
}

fn absolute(workspace: &Path, path: &str) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace.join(path)
    }
}

fn first<'a>(arguments: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|key| arguments[*key].as_str())
}

/// The step for `approval`, asked by `kind`'s program in `mode`, with the workspace at
/// `workspace`.
pub fn map(kind: BackendKind, approval: &DelegateApproval, workspace: &Path, mode: Mode) -> Step {
    if kind == BackendKind::Claude && approval.tool == "ExitPlanMode" {
        return Step::Refuse(PLAN_NOTE.into());
    }
    if kind == BackendKind::Codex && matches!(mode, Mode::Plan | Mode::ReadOnly) {
        return Step::Refuse(format!(
            "{mode} mode: this would run outside the read-only sandbox, which harness does not allow in this mode"
        ));
    }
    let call_id = approval.call_id.clone();
    let args = &approval.arguments;
    match &approval.kind {
        DelegateApprovalKind::Command { command, .. } => Step::Ask(GateRequest {
            call_id,
            tool: "bash".into(),
            arguments: json!({"command": command}),
            actions: vec![Action::Bash(command.clone())],
            reason: approval.reason.clone(),
        }),
        DelegateApprovalKind::FileChange { paths } => {
            let mut actions: Vec<Action> = paths
                .iter()
                .map(|p| Action::Write(absolute(workspace, p)))
                .collect();
            // Codex may ask to write under a wider root than the files: that is checked too,
            // and the strictest answer decides.
            let mut reason = approval.reason.clone();
            if let Some(root) = args["grantRoot"].as_str().filter(|r| !r.is_empty()) {
                actions.push(Action::Write(absolute(workspace, root)));
                reason = format!("{reason} (and write access under {root})");
            }
            // The shapes harness's own `write` and `edit` prompts show a diff for, where the
            // child's call has them.
            let (tool, arguments) = match (approval.tool.as_str(), paths.as_slice()) {
                ("Write", [path]) => ("write", json!({"path": path, "content": args["content"]})),
                ("Edit", [path]) => (
                    "edit",
                    json!({
                        "path": path,
                        "old_string": args["old_string"],
                        "new_string": args["new_string"],
                        "replace_all": args["replace_all"],
                    }),
                ),
                ("file_change", _) => ("file_change", json!({"paths": paths})),
                (other, _) => (other, args.clone()),
            };
            Step::Ask(GateRequest {
                call_id,
                tool: tool.into(),
                arguments,
                actions,
                reason,
            })
        }
        DelegateApprovalKind::Tool => {
            // The child's read-only tools are reads of a path; every other tool has no action
            // the engine knows, and is asked about.
            let actions = match approval.tool.as_str() {
                "Read" | "NotebookRead" | "Glob" | "Grep" | "LS" => {
                    vec![Action::Read(
                        first(args, &["file_path", "notebook_path", "path"])
                            .map_or_else(|| workspace.to_path_buf(), |p| absolute(workspace, p)),
                    )]
                }
                _ => Vec::new(),
            };
            Step::Ask(GateRequest {
                call_id,
                tool: approval.tool.clone(),
                arguments: args.clone(),
                actions,
                reason: approval.reason.clone(),
            })
        }
    }
}
