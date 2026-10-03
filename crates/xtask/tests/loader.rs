//! Loading tasks from a directory.

use xtask::task::{Task, load_all};

fn write(root: &std::path::Path, path: &str, text: &str) {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn task_toml() -> &'static str {
    "language = \"python\"\ntitle = \"t\"\nprompt = \"do it\"\ntest = \"python3 -m unittest\"\n"
}

#[test]
fn a_task_is_its_metadata_and_its_before_and_after_trees() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "x1/task.toml", task_toml());
    write(dir.path(), "x1/before/a.py", "old\n");
    write(dir.path(), "x1/before/sub/b.py", "b\n");
    write(dir.path(), "x1/after/a.py", "new\n");
    write(dir.path(), "x1/after/sub/b.py", "b\n");
    let tasks = load_all(dir.path()).unwrap();
    let task: &Task = &tasks[0];
    assert_eq!(task.id, "x1");
    assert_eq!(
        (
            task.language.as_str(),
            task.prompt.as_str(),
            task.test.as_str()
        ),
        ("python", "do it", "python3 -m unittest")
    );
    assert_eq!(task.before["a.py"], "old\n");
    assert_eq!(task.before["sub/b.py"], "b\n");
    assert_eq!(task.after["a.py"], "new\n");
}

#[test]
fn tasks_load_in_name_order_and_replay_files_are_not_part_of_the_trees() {
    let dir = tempfile::tempdir().unwrap();
    for id in ["b2", "a1"] {
        write(dir.path(), &format!("{id}/task.toml"), task_toml());
        write(dir.path(), &format!("{id}/before/f.py"), "1\n");
        write(dir.path(), &format!("{id}/after/f.py"), "2\n");
        write(dir.path(), &format!("{id}/replay/hashline.json"), "{}");
    }
    let ids: Vec<String> = load_all(dir.path())
        .unwrap()
        .into_iter()
        .map(|t| t.id)
        .collect();
    assert_eq!(ids, ["a1", "b2"]);
}

#[test]
fn a_task_that_is_not_complete_is_an_error_naming_it() {
    for (missing, files) in [
        ("task.toml", vec!["m/before/f", "m/after/f"]),
        ("before", vec!["m/task.toml", "m/after/f"]),
        ("after", vec!["m/task.toml", "m/before/f"]),
    ] {
        let dir = tempfile::tempdir().unwrap();
        for f in files {
            write(
                dir.path(),
                f,
                if f.ends_with("task.toml") {
                    task_toml()
                } else {
                    "x\n"
                },
            );
        }
        let error = load_all(dir.path()).unwrap_err();
        assert!(
            error.contains('m') && error.contains(missing),
            "{missing}: {error}"
        );
    }
}

#[test]
fn a_task_toml_that_lacks_a_key_or_has_an_unknown_one_is_an_error() {
    for text in [
        "language = \"python\"\ntitle = \"t\"\nprompt = \"p\"\n",
        "language = \"cobol\"\ntitle = \"t\"\nprompt = \"p\"\ntest = \"x\"\n",
        "language = \"go\"\ntitle = \"t\"\nprompt = \"p\"\ntest = \"x\"\nextra = 1\n",
    ] {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "m/task.toml", text);
        write(dir.path(), "m/before/f", "x\n");
        write(dir.path(), "m/after/f", "y\n");
        assert!(load_all(dir.path()).is_err(), "{text}");
    }
}
