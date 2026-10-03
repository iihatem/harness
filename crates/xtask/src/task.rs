//! The eval suite's tasks: a small repository before and after the change that is asked for, a
//! prompt, and the command whose result decides pass or fail.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use serde::Deserialize;

/// The text files of a directory, by path relative to it.
pub type Tree = BTreeMap<String, String>;

/// The languages tasks are written in.
pub const LANGUAGES: [&str; 4] = ["rust", "python", "typescript", "go"];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Meta {
    language: String,
    title: String,
    prompt: String,
    /// Run in the repository; passes when it exits with 0.
    test: String,
}

#[derive(Debug, Clone)]
pub struct Task {
    pub id: String,
    pub dir: PathBuf,
    pub language: String,
    pub title: String,
    pub prompt: String,
    pub test: String,
    /// The repository the model starts from: its test fails.
    pub before: Tree,
    /// The repository when the task is done: its test passes.
    pub after: Tree,
}

/// Every task in `dir`, one subdirectory each, in name order.
pub fn load_all(dir: &Path) -> Result<Vec<Task>, String> {
    let mut names: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| format!("cannot read {}: {e}", dir.display()))?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .collect();
    names.sort();
    names.iter().map(|path| load(path)).collect()
}

fn load(dir: &Path) -> Result<Task, String> {
    let id = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let toml_path = dir.join("task.toml");
    let text = std::fs::read_to_string(&toml_path)
        .map_err(|e| format!("{id}: cannot read task.toml: {e}"))?;
    let meta: Meta = toml::from_str(&text).map_err(|e| format!("{id}: task.toml: {e}"))?;
    if !LANGUAGES.contains(&meta.language.as_str()) {
        return Err(format!(
            "{id}: task.toml: unknown language `{}` (expected {})",
            meta.language,
            LANGUAGES.join(", ")
        ));
    }
    let tree = |name: &str| -> Result<Tree, String> {
        let path = dir.join(name);
        if !path.is_dir() {
            return Err(format!("{id}: the directory `{name}` is missing"));
        }
        read_tree(&path).map_err(|e| format!("{id}: {name}: {e}"))
    };
    Ok(Task {
        before: tree("before")?,
        after: tree("after")?,
        id,
        dir: dir.to_path_buf(),
        language: meta.language,
        title: meta.title,
        prompt: meta.prompt,
        test: meta.test,
    })
}

/// The files below `dir`, as text.
pub fn read_tree(dir: &Path) -> Result<Tree, String> {
    let mut tree = Tree::new();
    collect(dir, dir, &mut tree)?;
    Ok(tree)
}

fn collect(root: &Path, dir: &Path, tree: &mut Tree) -> Result<(), String> {
    let entries =
        std::fs::read_dir(dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
    for entry in entries {
        let path = entry.map_err(|e| e.to_string())?.path();
        if path.is_dir() {
            collect(root, &path, tree)?;
        } else {
            let relative = path
                .strip_prefix(root)
                .expect("below the root")
                .to_string_lossy()
                .replace('\\', "/");
            let text = std::fs::read_to_string(&path)
                .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            tree.insert(relative, text);
        }
    }
    Ok(())
}

/// Writes `tree` below `dir`.
pub fn write_tree(dir: &Path, tree: &Tree) -> Result<(), String> {
    for (path, text) in tree {
        let target = dir.join(path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(&target, text)
            .map_err(|e| format!("cannot write {}: {e}", target.display()))?;
    }
    Ok(())
}
