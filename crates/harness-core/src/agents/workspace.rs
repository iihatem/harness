//! Where a child agent works. A child works in its parent's workspace, or in a workspace harness
//! itself made for the same repository (a worktree, from M3d). It is never a path a model or a
//! definition named: the spec carries a [`ChildWorkspace`], which only harness code can make.

use std::path::{Path, PathBuf};

/// The workspace of a child. Made by [`ChildWorkspace::parent`] (its parent's own), by
/// [`ChildWorkspace::from_path`] (a path someone named, accepted only when it is the parent's), or
/// by harness code that created a workspace for the child itself (`created`, crate-internal until
/// the code that creates worktrees lands).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChildWorkspace(Choice);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
enum Choice {
    #[default]
    Parent,
    /// A workspace harness made for the same repository.
    Created(PathBuf),
}

/// A path that cannot be a child's workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceRefused {
    pub path: PathBuf,
    pub parent: PathBuf,
}

impl std::fmt::Display for WorkspaceRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "an agent can only work in its parent's workspace ({}), not in {}",
            self.parent.display(),
            self.path.display()
        )
    }
}

impl std::error::Error for WorkspaceRefused {}

impl ChildWorkspace {
    /// The workspace of the child's parent.
    pub fn parent() -> Self {
        ChildWorkspace(Choice::Parent)
    }

    /// A workspace that harness created for the child, for the parent's repository.
    #[allow(dead_code, reason = "for the code that creates worktrees")]
    pub(crate) fn created(path: PathBuf) -> Self {
        ChildWorkspace(Choice::Created(path))
    }

    /// The workspace for a path someone named: only the parent's own is accepted (the same
    /// directory, however it is spelled); any other is refused.
    pub fn from_path(path: &Path, parent: &Path) -> Result<Self, WorkspaceRefused> {
        let same = match (path.canonicalize(), parent.canonicalize()) {
            (Ok(a), Ok(b)) => a == b,
            _ => path == parent,
        };
        if same {
            Ok(ChildWorkspace::parent())
        } else {
            Err(WorkspaceRefused {
                path: path.to_path_buf(),
                parent: parent.to_path_buf(),
            })
        }
    }

    /// The directory, given the parent's.
    pub fn resolve(&self, parent: &Path) -> PathBuf {
        match &self.0 {
            Choice::Parent => parent.to_path_buf(),
            Choice::Created(path) => path.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_parents_own_directory_is_accepted_from_a_path() {
        let parent = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let spelled = parent.path().join(".");
        assert_eq!(
            ChildWorkspace::from_path(&spelled, parent.path()),
            Ok(ChildWorkspace::parent())
        );
        let refused = ChildWorkspace::from_path(other.path(), parent.path()).unwrap_err();
        assert!(refused.to_string().contains("parent's workspace"));
    }

    #[test]
    fn a_created_workspace_resolves_to_itself_and_the_default_to_the_parents() {
        let (parent, made) = (Path::new("/p"), PathBuf::from("/made"));
        assert_eq!(ChildWorkspace::default().resolve(parent), parent);
        assert_eq!(ChildWorkspace::created(made.clone()).resolve(parent), made);
    }
}
