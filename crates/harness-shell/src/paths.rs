//! Workspace containment and working-directory tracking.
//!
//! Paths are resolved lexically (`.`/`..` folded), then the longest existing ancestor
//! is canonicalized so symlinked prefixes (e.g. `/tmp` → `/private/tmp`) compare
//! equal. Paths that do not exist never touch the filesystem beyond that probe.

use std::path::{Component, Path, PathBuf};

/// More possible working directories than this collapses to "unknown".
const MAX_CWDS: usize = 8;

pub(crate) struct Workspace {
    root: PathBuf,
}

impl Workspace {
    pub(crate) fn new(root: &Path) -> Self {
        let abs = std::path::absolute(root).unwrap_or_else(|_| root.to_path_buf());
        Self {
            root: canonical(&abs),
        }
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// True if `path` is below the workspace root (the root itself does not count).
    pub(crate) fn strictly_contains(&self, path: &Path) -> bool {
        path != self.root && path.starts_with(&self.root)
    }
}

/// The set of directories the shell may be in at some point of a command line.
/// `None` stands for a directory that cannot be known statically.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Cwd(Vec<Option<PathBuf>>);

impl Cwd {
    pub(crate) fn at(dir: &Path) -> Self {
        Self(vec![Some(dir.to_path_buf())])
    }

    pub(crate) fn unknown() -> Self {
        Self(vec![None])
    }

    /// Adds the possibilities of `other` (used where control flow may or may not
    /// have run a `cd`).
    pub(crate) fn union(&mut self, other: &Cwd) {
        for dir in &other.0 {
            if !self.0.contains(dir) {
                self.0.push(dir.clone());
            }
        }
        if self.0.len() > MAX_CWDS {
            *self = Self::unknown();
        }
    }

    /// Applies `cd target`; `None` (e.g. `cd -`, `cd "$d"`) makes the directory unknown.
    pub(crate) fn cd(&mut self, target: Option<&str>) {
        let next = match target {
            Some(t) => self.resolve(t),
            None => vec![None],
        };
        self.0.clear();
        self.union(&Cwd(next));
    }

    /// Every absolute location `path` may name; `None` for a relative path under an
    /// unknown directory.
    pub(crate) fn resolve(&self, path: &str) -> Vec<Option<PathBuf>> {
        if Path::new(path).is_absolute() {
            return vec![Some(canonical(Path::new(path)))];
        }
        self.0
            .iter()
            .map(|dir| dir.as_ref().map(|d| canonical(&d.join(path))))
            .collect()
    }
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

fn canonical(path: &Path) -> PathBuf {
    let path = normalize(path);
    let mut rest = Vec::new();
    let mut probe = path.as_path();
    loop {
        if let Ok(real) = std::fs::canonicalize(probe) {
            return rest
                .iter()
                .rev()
                .fold(real, |acc: PathBuf, part| acc.join(part));
        }
        match (probe.parent(), probe.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                probe = parent;
            }
            _ => return path,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexical_resolution() {
        let cwd = Cwd::at(Path::new("/work/proj"));
        assert_eq!(cwd.resolve("../x"), [Some(PathBuf::from("/work/x"))]);
        assert_eq!(cwd.resolve("./"), [Some(PathBuf::from("/work/proj"))]);
        assert_eq!(cwd.resolve("/../../a"), [Some(PathBuf::from("/a"))]);
        assert_eq!(Cwd::unknown().resolve("x"), [None]);
    }

    #[test]
    fn containment() {
        let ws = Workspace::new(Path::new("/work/proj"));
        assert!(ws.strictly_contains(Path::new("/work/proj/target")));
        assert!(!ws.strictly_contains(Path::new("/work/proj")));
        assert!(!ws.strictly_contains(Path::new("/work/project")));
        assert!(!ws.strictly_contains(Path::new("/work")));
    }
}
