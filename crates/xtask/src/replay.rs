//! Replay: a recording's tool calls applied, through the real tools, to a copy of the task's
//! `before` tree. No model is called; this tests the formats' parsers and the apply logic.

use harness_core::{edit_format::EditFormat, tool::ToolContext};
use harness_tools::builtin_for;

use crate::{
    record::{Call, recorded},
    task::{Task, Tree, read_tree, write_tree},
};

/// `before` after `calls` were made in `format`. An error says which call failed and why.
pub fn apply(format: EditFormat, before: &Tree, calls: &[Call]) -> Result<Tree, String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    write_tree(dir.path(), before)?;
    let tools = builtin_for(format);
    let ctx = ToolContext::new(dir.path());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .map_err(|e| e.to_string())?;
    for (index, call) in calls.iter().enumerate() {
        let number = index + 1;
        let tool = tools.get(&call.name).ok_or_else(|| {
            format!(
                "call {number}: the tool `{}` is not offered in {format}",
                call.name
            )
        })?;
        let output = runtime.block_on(tool.run(call.arguments.clone(), &ctx));
        if output.is_error {
            return Err(format!("call {number} ({}): {}", call.name, output.content));
        }
    }
    read_tree(dir.path())
}

/// Replays the recording of `task` in `format`, and checks the tree it leaves is the task's
/// `after`.
pub fn check(task: &Task, format: EditFormat) -> Result<(), String> {
    let calls = recorded(task, format)?;
    let result =
        apply(format, &task.before, &calls).map_err(|e| format!("{} {format}: {e}", task.id))?;
    if result == task.after {
        return Ok(());
    }
    let mut differing: Vec<&str> = result
        .keys()
        .chain(task.after.keys())
        .filter(|path| result.get(*path) != task.after.get(*path))
        .map(String::as_str)
        .collect();
    differing.sort();
    differing.dedup();
    Err(format!(
        "{} {format}: the result differs from the task's `after` in {}",
        task.id,
        differing.join(", ")
    ))
}

/// Every task in every format; what failed.
pub fn check_all(tasks: &[Task]) -> Vec<String> {
    tasks
        .iter()
        .flat_map(|task| {
            EditFormat::ALL
                .into_iter()
                .map(move |format| (task, format))
        })
        .filter_map(|(task, format)| check(task, format).err())
        .collect()
}
