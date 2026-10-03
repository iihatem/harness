use std::path::PathBuf;

use async_trait::async_trait;
use harness_core::{
    message::ToolSpec,
    permission::Action,
    tool::{Tool, ToolContext, ToolOutput},
};
use serde_json::{Value, json};

use crate::patch::{self, FileOp};

pub struct ApplyPatchTool;

/// One file's part of a patch, computed before anything is written.
struct Planned {
    path: PathBuf,
    /// What the file held, `None` when it did not exist.
    before: Option<Vec<u8>>,
    /// What it will hold, `None` to delete it.
    after: Option<String>,
    summary: String,
    /// The permissions to give the file once written: a moved file keeps its old ones.
    mode: Option<u32>,
}

/// The paths a patch writes: for each file the one it ends up at, and a moved file's old one too.
fn paths_of(ops: &[FileOp], ctx: &ToolContext) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for op in ops {
        match op {
            FileOp::Update {
                path,
                move_to: Some(target),
                ..
            } => {
                paths.push(ctx.resolve(target));
                paths.push(ctx.resolve(path));
            }
            other => paths.push(ctx.resolve(other.path())),
        }
    }
    paths
}

fn patch_of(args: &Value) -> &str {
    args["input"].as_str().unwrap_or_default()
}

#[async_trait]
impl Tool for ApplyPatchTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "apply_patch".into(),
            description: "Edit files with a patch: `*** Begin Patch`, then for each file `*** Add File: path` (every line starts with +), `*** Update File: path` (hunks starting with @@ and an optional line to find the place by; lines start with a space for context, - to remove, + to add) or `*** Delete File: path`, then `*** End Patch`. Applied all or nothing. Read a file before updating or deleting it.".into(),
            parameters: json!({
                "type": "object",
                "properties": {"input": {"type": "string", "description": "The whole patch"}},
                "required": ["input"],
                "additionalProperties": false
            }),
        }
    }

    fn action(&self, args: &Value, ctx: &ToolContext) -> Action {
        self.actions(args, ctx)
            .into_iter()
            .next()
            .unwrap_or_else(|| Action::Read(ctx.workspace.clone()))
    }

    /// Every path the patch writes. A patch that does not parse writes none, and is refused when
    /// it runs.
    fn actions(&self, args: &Value, ctx: &ToolContext) -> Vec<Action> {
        match patch::parse(patch_of(args)) {
            Ok(ops) => paths_of(&ops, ctx).into_iter().map(Action::Write).collect(),
            Err(_) => vec![Action::Read(ctx.workspace.clone())],
        }
    }

    fn changed_paths(&self, args: &Value, ctx: &ToolContext) -> Vec<PathBuf> {
        let Ok(ops) = patch::parse(patch_of(args)) else {
            return Vec::new();
        };
        ops.iter()
            .filter_map(|op| match op {
                FileOp::Delete { .. } => None,
                FileOp::Update {
                    move_to: Some(target),
                    ..
                } => Some(ctx.resolve(target)),
                other => Some(ctx.resolve(other.path())),
            })
            .collect()
    }

    async fn run(&self, args: Value, ctx: &ToolContext) -> ToolOutput {
        let ops = match patch::parse(patch_of(&args)) {
            Ok(ops) => ops,
            Err(e) => return refused(&e.to_string()),
        };
        let planned = match plan(&ops, ctx).await {
            Ok(planned) => planned,
            Err(message) => return refused(&message),
        };
        if let Err(message) = write_all(&planned).await {
            return refused(&message);
        }
        for file in &planned {
            if let Some(after) = &file.after {
                ctx.tracker.record(&file.path, after.as_bytes());
            }
        }
        let lines: Vec<&str> = planned.iter().map(|f| f.summary.as_str()).collect();
        ToolOutput::ok(format!("Applied the patch:\n{}", lines.join("\n")))
    }
}

fn refused(message: &str) -> ToolOutput {
    ToolOutput::error(format!(
        "the patch was not applied, no file changed: {message}"
    ))
}

/// Checks every part of the patch against the disk and the guards, and computes the new
/// contents. Writes nothing.
async fn plan(ops: &[FileOp], ctx: &ToolContext) -> Result<Vec<Planned>, String> {
    let mut planned: Vec<Planned> = Vec::new();
    // Every file a patch names, as the disk sees it: two operations on one file would lose a
    // change, however the path is spelled (`a.rs`, `./a.rs`), or whether it is moved onto.
    let mut named: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    for op in ops {
        let name = op.path();
        let path = ctx.resolve(name);
        if !named.insert(path.clone()) {
            return Err(format!(
                "{name} appears twice in the patch (a path may be named once, as a file or a move's target); put its changes in one section"
            ));
        }
        if let FileOp::Update {
            move_to: Some(target),
            ..
        } = op
            && !named.insert(ctx.resolve(target))
        {
            return Err(format!(
                "{target} appears twice in the patch (a path may be named once, as a file or a move's target)"
            ));
        }
        let existing = match tokio::fs::read(&path).await {
            Ok(bytes) => Some(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(format!("{name}: cannot read it: {e}")),
        };
        match op {
            FileOp::Add { content, .. } => {
                if existing.is_some() {
                    return Err(format!("{name} already exists; use `*** Update File:`"));
                }
                planned.push(Planned {
                    path,
                    before: None,
                    after: Some(content.clone()),
                    summary: format!("added {name} ({} lines)", content.lines().count()),
                    mode: None,
                });
            }
            FileOp::Delete { .. } => {
                let bytes = existing.ok_or_else(|| format!("{name} does not exist"))?;
                ctx.tracker
                    .check_fresh(&path, &bytes)
                    .map_err(|e| format!("{name}: {e}"))?;
                planned.push(Planned {
                    path,
                    before: Some(bytes),
                    after: None,
                    summary: format!("deleted {name}"),
                    mode: None,
                });
            }
            FileOp::Update { move_to, hunks, .. } => {
                let bytes = existing.ok_or_else(|| format!("{name} does not exist"))?;
                ctx.tracker
                    .check_fresh(&path, &bytes)
                    .map_err(|e| format!("{name}: {e}"))?;
                let text = String::from_utf8(bytes.clone())
                    .map_err(|_| format!("{name} is not valid UTF-8"))?;
                let (updated, notes) =
                    patch::apply_hunks_noted(name, &text, hunks).map_err(|e| e.to_string())?;
                let notes: String = notes.iter().map(|n| format!("; {n}")).collect();
                let changed = format!(
                    "(+{} -{})",
                    hunks
                        .iter()
                        .flat_map(|h| &h.lines)
                        .filter(|(k, _)| *k == patch::LineKind::Add)
                        .count(),
                    hunks
                        .iter()
                        .flat_map(|h| &h.lines)
                        .filter(|(k, _)| *k == patch::LineKind::Remove)
                        .count(),
                );
                match move_to {
                    None => planned.push(Planned {
                        path,
                        before: Some(bytes),
                        after: Some(updated),
                        summary: format!("updated {name} {changed}{notes}"),
                        mode: None,
                    }),
                    Some(target) => {
                        let target_path = ctx.resolve(target);
                        if tokio::fs::try_exists(&target_path).await.unwrap_or(true) {
                            return Err(format!("cannot move {name} to {target}: it exists"));
                        }
                        use std::os::unix::fs::PermissionsExt;
                        let mode = tokio::fs::metadata(&path)
                            .await
                            .ok()
                            .map(|m| m.permissions().mode());
                        planned.push(Planned {
                            path: target_path,
                            before: None,
                            after: Some(updated),
                            summary: format!("moved {name} to {target} {changed}{notes}"),
                            mode,
                        });
                        planned.push(Planned {
                            path,
                            before: Some(bytes),
                            after: None,
                            summary: String::new(),
                            mode: None,
                        });
                    }
                }
            }
        }
    }
    Ok(planned)
}

/// Writes every planned file, and puts back what was there if one write fails.
async fn write_all(planned: &[Planned]) -> Result<(), String> {
    for (done, file) in planned.iter().enumerate() {
        if let Err(message) = write_one(file).await {
            for undone in planned[..done].iter().rev() {
                let _ = restore(undone).await;
            }
            // The failed one may have been written in part.
            let _ = restore(file).await;
            return Err(message);
        }
    }
    Ok(())
}

async fn write_one(file: &Planned) -> Result<(), String> {
    let name = file.path.display();
    match &file.after {
        Some(text) => {
            if let Some(parent) = file.path.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
            }
            tokio::fs::write(&file.path, text)
                .await
                .map_err(|e| format!("cannot write {name}: {e}"))?;
            if let Some(mode) = file.mode {
                use std::os::unix::fs::PermissionsExt;
                tokio::fs::set_permissions(&file.path, std::fs::Permissions::from_mode(mode))
                    .await
                    .map_err(|e| format!("cannot set the permissions of {name}: {e}"))?;
            }
            Ok(())
        }
        None => tokio::fs::remove_file(&file.path)
            .await
            .map_err(|e| format!("cannot delete {name}: {e}")),
    }
}

async fn restore(file: &Planned) -> std::io::Result<()> {
    match &file.before {
        Some(bytes) => tokio::fs::write(&file.path, bytes).await,
        None => match tokio::fs::remove_file(&file.path).await {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            other => other,
        },
    }
}
