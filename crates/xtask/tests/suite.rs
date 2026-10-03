//! The checked-in suite: its tasks load, and every recorded output applies to its task.

use harness_core::edit_format::EditFormat;
use xtask::{
    record::{path_of, recorded, regenerate},
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

// The check run in CI also finds a recording that applies but is not what the recorder makes
// today.
#[test]
fn a_stale_recording_fails_the_check() {
    let dir = tempfile::tempdir().unwrap();
    let mut task = suite().remove(0);
    let copy = dir.path().join(&task.id);
    std::fs::create_dir_all(copy.join("replay")).unwrap();
    for format in EditFormat::ALL {
        std::fs::copy(
            path_of(&task, format),
            copy.join("replay").join(format!("{format}.json")),
        )
        .unwrap();
    }
    task.dir = copy.clone();
    assert!(check(&task, EditFormat::WholeFile).is_ok());
    // The same edit, with a read that the recorder would not make.
    let mut calls = recorded(&task, EditFormat::WholeFile).unwrap();
    calls.insert(0, calls[0].clone());
    let file = serde_json::json!({"calls": calls});
    std::fs::write(
        copy.join("replay/whole_file.json"),
        serde_json::to_string_pretty(&file).unwrap(),
    )
    .unwrap();
    let error = check(&task, EditFormat::WholeFile).unwrap_err();
    assert!(
        error.contains("stale") && error.contains("eval record"),
        "{error}"
    );
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

// Spec: "30 small tasks across Rust, Python, TypeScript and Go".
#[test]
fn the_suite_has_30_tasks_across_the_four_languages() {
    let tasks = suite();
    assert_eq!(tasks.len(), 30);
    let count = |language: &str| tasks.iter().filter(|t| t.language == language).count();
    assert_eq!(
        [
            count("rust"),
            count("python"),
            count("typescript"),
            count("go")
        ],
        [8, 8, 7, 7]
    );
}

// Each task has a test command and, for the formats' sake, at least one task changes several
// files, one adds code and one fixes code.
#[test]
fn the_tasks_cover_new_code_fixes_and_several_files() {
    let tasks = suite();
    assert!(
        tasks
            .iter()
            .any(|t| t.after.len() > t.before.len() || t.title.starts_with("add"))
    );
    assert!(tasks.iter().filter(|t| changed_files(t) >= 2).count() >= 4);
    assert!(tasks.iter().any(|t| changed_files(t) == 1));
}

fn changed_files(task: &Task) -> usize {
    task.after
        .iter()
        .filter(|(path, text)| task.before.get(*path) != Some(*text))
        .count()
}

// The recordings of every task cover every format, and each is the full set of files changed.
#[test]
fn every_task_has_a_recording_in_every_format() {
    for task in suite() {
        for format in EditFormat::ALL {
            assert!(recorded(&task, format).is_ok(), "{} {format}", task.id);
        }
    }
}
