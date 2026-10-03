//! The checked-in suite: its tasks load, and every recorded output applies to its task.

use harness_core::edit_format::EditFormat;
use xtask::{
    record::{recorded, regenerate},
    replay::{check, check_all},
    task::{Task, load_all},
};

fn root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../eval")
}

fn suite() -> Vec<Task> {
    load_all(&root().join("tasks")).unwrap()
}

#[test]
fn the_suite_loads_and_every_task_is_well_formed() {
    let tasks = suite();
    assert!(tasks.len() >= 6);
    let mut ids: Vec<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), tasks.len(), "ids are unique");
    for task in &tasks {
        assert!(
            ["rust", "python", "typescript", "go"].contains(&task.language.as_str()),
            "{}: {}",
            task.id,
            task.language
        );
        assert!(
            !task.prompt.is_empty() && !task.test.is_empty(),
            "{}",
            task.id
        );
        assert!(
            !task.before.is_empty() && !task.after.is_empty(),
            "{}",
            task.id
        );
        assert_ne!(task.before, task.after, "{}: nothing to do", task.id);
        // Text files that end in a newline, which the recorder relies on.
        for text in task.before.values().chain(task.after.values()) {
            assert!(text.ends_with('\n'), "{}", task.id);
        }
    }
}

// Spec "CI replay": no model is called, and every recorded output applies to its task with the
// result it had when it was recorded.
#[test]
fn every_recorded_output_applies_to_its_task_in_every_format() {
    let tasks = suite();
    let failures = check_all(&tasks);
    assert!(failures.is_empty(), "{failures:#?}");
}

#[test]
fn the_recordings_are_what_the_recorder_makes_today() {
    for task in suite() {
        for format in EditFormat::ALL {
            assert_eq!(
                recorded(&task, format).unwrap(),
                regenerate(&task, format),
                "{} {format}: run `cargo xtask eval record`",
                task.id
            );
        }
    }
}

#[test]
fn a_missing_recording_fails_the_check() {
    let mut task = suite().remove(0);
    task.dir = task.dir.join("no-such-dir");
    let error = check(&task, EditFormat::ApplyPatch).unwrap_err();
    assert!(error.contains("apply_patch"), "{error}");
}

#[test]
fn a_recording_that_gives_the_wrong_result_fails_the_check() {
    let dir = tempfile::tempdir().unwrap();
    let mut task = suite().remove(0);
    // A copy of the task whose recording changes the file to something else.
    let copy = dir.path().join(&task.id);
    std::fs::create_dir_all(copy.join("replay")).unwrap();
    task.dir = copy.clone();
    let wrong = serde_json::json!({"calls": [
        {"name": "read", "arguments": {"path": "src/lib.rs"}},
        {"name": "write", "arguments": {"path": "src/lib.rs", "content": "pub fn sum_to(_: u32) -> u32 { 0 }\n"}}
    ]});
    std::fs::write(
        copy.join("replay/whole_file.json"),
        serde_json::to_string_pretty(&wrong).unwrap(),
    )
    .unwrap();
    let error = check(&task, EditFormat::WholeFile).unwrap_err();
    assert!(error.contains("src/lib.rs"), "{error}");
}
