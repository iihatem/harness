use std::path::Path;

use harness_context::project::{discovery_root, project_key, project_root, repo_root};

#[test]
fn the_repository_root_is_the_nearest_directory_with_a_git_entry() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    std::fs::create_dir_all(base.join("repo/sub/deep")).unwrap();
    // A gitfile, as in a linked worktree or a submodule, counts too.
    std::fs::write(base.join("repo/.git"), "gitdir: /elsewhere\n").unwrap();
    assert_eq!(
        repo_root(&base.join("repo/sub/deep")),
        Some(base.join("repo"))
    );
    assert_eq!(project_root(&base.join("repo/sub")), base.join("repo"));
    assert_eq!(repo_root(&base), None);
    assert_eq!(project_root(&base), base);
}

#[test]
fn outside_a_repository_the_discovery_root_is_home_or_the_directory() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let home = base.join("home");
    std::fs::create_dir_all(home.join("a/b")).unwrap();
    std::fs::create_dir_all(base.join("other")).unwrap();
    assert_eq!(discovery_root(&home.join("a/b"), Some(&home)), home);
    assert_eq!(
        discovery_root(&base.join("other"), Some(&home)),
        base.join("other")
    );
    assert_eq!(
        discovery_root(&base.join("other"), None),
        base.join("other")
    );
}

#[test]
fn project_keys_are_readable_stable_and_distinct() {
    let key = project_key(Path::new("/work/My Repo"));
    assert!(key.starts_with("My_Repo-"), "{key}");
    assert_eq!(key.len(), "My_Repo-".len() + 16);
    assert_eq!(key, project_key(Path::new("/work/My Repo")));
    assert_ne!(key, project_key(Path::new("/other/My Repo")));
    assert!(project_key(Path::new("/")).starts_with("root-"));
    assert!(!project_key(Path::new("/x/..hidden")).starts_with('.'));
}
