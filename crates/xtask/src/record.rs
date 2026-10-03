//! A model's edit, recorded as the tool calls it would make in an edit format. The recordings
//! are made from a task's `before` and `after` trees, deterministically, and checked in under
//! `replay/<format>.json`: replay mode applies them without a model.

use std::path::PathBuf;

use harness_core::edit_format::EditFormat;
use harness_tools::{
    hashline::address,
    patch::{FileOp, apply_hunks, parse},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::task::{Task, Tree};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Call {
    pub name: String,
    pub arguments: Value,
}

#[derive(Serialize, Deserialize)]
struct File {
    calls: Vec<Call>,
}

fn call(name: &str, arguments: Value) -> Call {
    Call {
        name: name.into(),
        arguments,
    }
}

/// The calls that turn `before` into `after` in `format`: a `read` of each file to change, then
/// the edits. Files that are the same in both are left out.
pub fn calls(format: EditFormat, before: &Tree, after: &Tree) -> Vec<Call> {
    let mut reads = Vec::new();
    let mut edits = Vec::new();
    let mut patch = String::new();
    for (path, new) in after {
        let old = before.get(path);
        if old == Some(new) {
            continue;
        }
        if old.is_some() {
            reads.push(call("read", json!({"path": path})));
        }
        match (old, format) {
            (None, EditFormat::ApplyPatch) => {
                patch.push_str(&format!("*** Add File: {path}\n"));
                patch.extend(new.lines().map(|l| format!("+{l}\n")));
            }
            (None, _) => edits.push(call("write", json!({"path": path, "content": new}))),
            // An empty file has no text to replace and no line to address: it is written.
            (Some(old), EditFormat::StrReplace | EditFormat::Hashline) if old.is_empty() => {
                edits.push(call("write", json!({"path": path, "content": new})))
            }
            (Some(old), EditFormat::StrReplace) => edits.push(str_replace(path, old, new)),
            (Some(old), EditFormat::ApplyPatch) => {
                patch.push_str(&format!("*** Update File: {path}\n{}", hunk(old, new)))
            }
            (Some(_), EditFormat::WholeFile) => {
                edits.push(call("write", json!({"path": path, "content": new})))
            }
            (Some(old), EditFormat::Hashline) => edits.push(hashline(path, old, new)),
        }
    }
    if !patch.is_empty() {
        edits.push(call(
            "apply_patch",
            json!({"input": format!("*** Begin Patch\n{patch}*** End Patch\n")}),
        ));
    }
    reads.extend(edits);
    reads
}

fn lines(text: &str) -> Vec<&str> {
    text.lines().collect()
}

/// The part of two files that differs: the lines both start with, `start`, then the end of the
/// differing part in each (the lines both end with are left out of it).
fn region(old: &[&str], new: &[&str]) -> (usize, usize, usize) {
    let shortest = old.len().min(new.len());
    let start = (0..shortest).take_while(|&i| old[i] == new[i]).count();
    let end = (0..shortest - start)
        .take_while(|&i| old[old.len() - 1 - i] == new[new.len() - 1 - i])
        .count();
    (start, old.len() - end, new.len() - end)
}

fn str_replace(path: &str, old: &str, new: &str) -> Call {
    let (a, b) = (lines(old), lines(new));
    let (start, old_end, new_end) = region(&a, &b);
    // Lines around the change until the text to replace occurs once in the file.
    let mut context = 1;
    loop {
        let from = start.saturating_sub(context);
        let to = (old_end + context).min(a.len());
        let wanted = a[from..to].join("\n");
        let whole = from == 0 && to == a.len();
        if whole || (!wanted.is_empty() && old.matches(&wanted).count() == 1) {
            let mut replacement: Vec<&str> = a[from..start].to_vec();
            replacement.extend(&b[start..new_end]);
            replacement.extend(&a[old_end..to]);
            return call(
                "edit",
                json!({"path": path, "old_string": wanted, "new_string": replacement.join("\n")}),
            );
        }
        context += 1;
    }
}

/// One hunk with as much context as it takes to find its place.
fn hunk(old: &str, new: &str) -> String {
    let (a, b) = (lines(old), lines(new));
    let (start, old_end, new_end) = region(&a, &b);
    let mut context = 3;
    loop {
        let from = start.saturating_sub(context);
        let to = (old_end + context).min(a.len());
        let mut text = String::from("@@\n");
        text.extend(a[from..start].iter().map(|l| format!(" {l}\n")));
        text.extend(a[start..old_end].iter().map(|l| format!("-{l}\n")));
        text.extend(b[start..new_end].iter().map(|l| format!("+{l}\n")));
        text.extend(a[old_end..to].iter().map(|l| format!(" {l}\n")));
        let exact = parse(&format!(
            "*** Begin Patch\n*** Update File: f\n{text}*** End Patch\n"
        ))
        .ok()
        .and_then(|ops| match ops.into_iter().next() {
            Some(FileOp::Update { hunks, .. }) => apply_hunks("f", old, &hunks).ok(),
            _ => None,
        })
        .is_some_and(|result| result == new);
        if exact || (from == 0 && to == a.len()) {
            return text;
        }
        context += 1;
    }
}

fn hashline(path: &str, old: &str, new: &str) -> Call {
    let (a, b) = (lines(old), lines(new));
    let (start, old_end, new_end) = region(&a, &b);
    let added: Vec<&str> = b[start..new_end].to_vec();
    if old_end > start {
        // Lines are replaced, or deleted.
        return call(
            "hashline_edit",
            json!({
                "path": path,
                "start": address(start + 1, a[start]),
                "end": address(old_end, a[old_end - 1]),
                "new_text": added.join("\n"),
            }),
        );
    }
    // Lines are only added: the line before them (or the first, for the start) is replaced by
    // itself and them.
    if start > 0 {
        let mut text = vec![a[start - 1]];
        text.extend(&added);
        call(
            "hashline_edit",
            json!({"path": path, "start": address(start, a[start - 1]), "new_text": text.join("\n")}),
        )
    } else {
        let mut text = added;
        text.push(a[0]);
        call(
            "hashline_edit",
            json!({"path": path, "start": address(1, a[0]), "new_text": text.join("\n")}),
        )
    }
}

/// Where a task's recording for `format` is kept.
pub fn path_of(task: &Task, format: EditFormat) -> PathBuf {
    task.dir.join("replay").join(format!("{format}.json"))
}

/// The calls recorded for `task` in `format`.
pub fn recorded(task: &Task, format: EditFormat) -> Result<Vec<Call>, String> {
    let path = path_of(task, format);
    let text = std::fs::read_to_string(&path).map_err(|e| {
        format!(
            "{}: no recording for {format} ({}): {e}",
            task.id,
            path.display()
        )
    })?;
    serde_json::from_str::<File>(&text)
        .map(|file| file.calls)
        .map_err(|e| format!("{}: the {format} recording is not valid: {e}", task.id))
}

/// The calls the recorder makes for `task` in `format` now.
pub fn regenerate(task: &Task, format: EditFormat) -> Vec<Call> {
    calls(format, &task.before, &task.after)
}

/// Writes every task's recordings.
pub fn write_all(tasks: &[Task]) -> Result<(), String> {
    for task in tasks {
        for format in EditFormat::ALL {
            let path = path_of(task, format);
            std::fs::create_dir_all(path.parent().expect("a directory"))
                .map_err(|e| e.to_string())?;
            let file = File {
                calls: regenerate(task, format),
            };
            let mut text = serde_json::to_string_pretty(&file).map_err(|e| e.to_string())?;
            text.push('\n');
            std::fs::write(&path, text)
                .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        }
    }
    Ok(())
}
