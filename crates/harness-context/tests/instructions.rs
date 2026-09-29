use std::path::{Path, PathBuf};

use harness_context::instructions::{self, Instructions};

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// A temp directory resolved through symlinks (macOS's `/var` is `/private/var`), holding
/// `config/` (the harness config directory), `home/`, and a repository at `home/repo`.
fn setup() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    std::fs::create_dir_all(base.join("config")).unwrap();
    std::fs::create_dir_all(base.join("home/repo/.git")).unwrap();
    (dir, base)
}

fn load(base: &Path, cwd: &str) -> Instructions {
    let cwd = base.join(cwd);
    std::fs::create_dir_all(&cwd).unwrap();
    instructions::discover(&cwd, &base.join("config"), Some(&base.join("home")))
}

fn contents(loaded: &Instructions) -> Vec<&str> {
    loaded.files.iter().map(|f| f.content.as_str()).collect()
}

#[test]
fn agents_md_is_preferred_over_claude_md() {
    let (_dir, base) = setup();
    write(&base.join("home/repo/AGENTS.md"), "agents\n");
    write(&base.join("home/repo/CLAUDE.md"), "claude\n");
    let loaded = load(&base, "home/repo");
    assert_eq!(contents(&loaded), ["agents\n"]);
    assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
}

#[test]
fn claude_md_is_loaded_when_there_is_no_agents_md() {
    let (_dir, base) = setup();
    write(&base.join("home/repo/CLAUDE.md"), "claude\n");
    assert_eq!(contents(&load(&base, "home/repo")), ["claude\n"]);
}

#[test]
fn files_run_from_the_global_one_to_the_working_directory() {
    let (_dir, base) = setup();
    write(&base.join("config/AGENTS.md"), "global\n");
    write(&base.join("home/repo/AGENTS.md"), "root\n");
    write(&base.join("home/repo/a/CLAUDE.md"), "a\n");
    write(&base.join("home/repo/a/b/AGENTS.md"), "b\n");
    let loaded = load(&base, "home/repo/a/b");
    assert_eq!(contents(&loaded), ["global\n", "root\n", "a\n", "b\n"]);
    assert_eq!(loaded.files[0].path, base.join("config/AGENTS.md"));
    assert_eq!(loaded.files[2].path, base.join("home/repo/a/CLAUDE.md"));
}

#[test]
fn discovery_stops_at_the_repository_root() {
    let (_dir, base) = setup();
    write(&base.join("home/AGENTS.md"), "home\n");
    write(&base.join("home/repo/AGENTS.md"), "root\n");
    assert_eq!(contents(&load(&base, "home/repo")), ["root\n"]);
}

#[test]
fn outside_a_repository_discovery_starts_at_the_home_directory() {
    let (_dir, base) = setup();
    write(&base.join("AGENTS.md"), "above home\n");
    write(&base.join("home/AGENTS.md"), "home\n");
    write(&base.join("home/notes/AGENTS.md"), "notes\n");
    assert_eq!(contents(&load(&base, "home/notes")), ["home\n", "notes\n"]);
}

#[test]
fn outside_the_home_directory_only_the_working_directory_counts() {
    let (_dir, base) = setup();
    write(&base.join("elsewhere/AGENTS.md"), "parent\n");
    write(&base.join("elsewhere/sub/AGENTS.md"), "sub\n");
    assert_eq!(contents(&load(&base, "elsewhere/sub")), ["sub\n"]);
}

#[test]
fn a_claude_md_importing_the_parent_agents_md_includes_it_once() {
    let (_dir, base) = setup();
    write(&base.join("home/repo/AGENTS.md"), "parent rules\n");
    write(
        &base.join("home/repo/sub/CLAUDE.md"),
        "@../AGENTS.md\nsub rules\n",
    );
    let loaded = load(&base, "home/repo/sub");
    let all = contents(&loaded).concat();
    assert_eq!(all.matches("parent rules").count(), 1, "{all}");
    assert!(all.contains("sub rules"));
    assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
}

#[test]
fn an_import_outside_the_repository_is_skipped_with_a_warning() {
    let (_dir, base) = setup();
    write(&base.join("secret.txt"), "TOKEN=abc\n");
    write(
        &base.join("home/repo/AGENTS.md"),
        "@/etc/passwd\n@../../secret.txt\nrules\n",
    );
    let loaded = load(&base, "home/repo");
    let all = contents(&loaded).concat();
    assert!(!all.contains("root:") && !all.contains("TOKEN"), "{all}");
    assert!(all.contains("@/etc/passwd\n") && all.contains("rules"));
    assert_eq!(loaded.warnings.len(), 2, "{:?}", loaded.warnings);
    assert!(loaded.warnings[0].contains("@/etc/passwd"));
    assert!(loaded.warnings[1].contains("outside the project"));
}

#[test]
fn imports_resolve_against_the_importing_file_and_nest() {
    let (_dir, base) = setup();
    write(&base.join("home/repo/AGENTS.md"), "top\n@docs/a.md\n");
    write(&base.join("home/repo/docs/a.md"), "A\n@b.md\n");
    write(&base.join("home/repo/docs/b.md"), "B");
    assert_eq!(contents(&load(&base, "home/repo")), ["top\nA\nB\n"]);
}

#[test]
fn imports_nest_at_most_five_levels() {
    let (_dir, base) = setup();
    write(&base.join("home/repo/AGENTS.md"), "@l1.md\n");
    let names = ["one", "two", "three", "four", "five", "six", "seven"];
    for (i, name) in names.iter().enumerate() {
        write(
            &base.join(format!("home/repo/l{}.md", i + 1)),
            &format!("{name}\n@l{}.md\n", i + 2),
        );
    }
    let loaded = load(&base, "home/repo");
    let all = contents(&loaded).concat();
    assert!(all.contains("five") && !all.contains("six"), "{all}");
    assert!(
        loaded.warnings.iter().any(|w| w.contains("5 levels")),
        "{:?}",
        loaded.warnings
    );
}

#[test]
fn a_missing_import_is_a_warning_and_the_line_stays() {
    let (_dir, base) = setup();
    write(&base.join("home/repo/AGENTS.md"), "@missing.md\nrules\n");
    let loaded = load(&base, "home/repo");
    assert_eq!(contents(&loaded), ["@missing.md\nrules\n"]);
    assert!(loaded.warnings[0].contains("does not exist"));
}

#[test]
fn import_cycles_include_each_file_once() {
    let (_dir, base) = setup();
    write(&base.join("home/repo/AGENTS.md"), "top\n@a.md\n");
    write(&base.join("home/repo/a.md"), "a\n@AGENTS.md\n");
    assert_eq!(contents(&load(&base, "home/repo")), ["top\na\n"]);
}

#[test]
fn import_lines_inside_code_fences_are_text() {
    let (_dir, base) = setup();
    write(&base.join("home/repo/b.md"), "B\n");
    write(
        &base.join("home/repo/AGENTS.md"),
        "```\n@b.md\n```\n@b.md\nsee @b.md inline\n",
    );
    assert_eq!(
        contents(&load(&base, "home/repo")),
        ["```\n@b.md\n```\nB\nsee @b.md inline\n"]
    );
}

// Review Focus: a cloned repository's instruction file that is a symlink to a secret outside the
// project must not be sent to the model.
#[test]
fn an_instruction_file_linking_outside_the_project_is_skipped() {
    let (_dir, base) = setup();
    write(&base.join("secret.txt"), "TOKEN=abc\n");
    std::os::unix::fs::symlink(base.join("secret.txt"), base.join("home/repo/AGENTS.md")).unwrap();
    let loaded = load(&base, "home/repo");
    assert!(loaded.files.is_empty(), "{:?}", loaded.files);
    assert!(loaded.warnings[0].contains("outside the project"));
}

#[test]
fn a_claude_md_linked_to_an_agents_md_in_the_project_is_loaded_once() {
    let (_dir, base) = setup();
    write(&base.join("home/repo/AGENTS.md"), "rules\n");
    std::fs::create_dir_all(base.join("home/repo/sub")).unwrap();
    std::os::unix::fs::symlink("../AGENTS.md", base.join("home/repo/sub/CLAUDE.md")).unwrap();
    let loaded = load(&base, "home/repo/sub");
    assert_eq!(contents(&loaded), ["rules\n"]);
    assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
}

#[test]
fn imports_may_reach_the_harness_config_directory() {
    let (_dir, base) = setup();
    write(&base.join("config/shared.md"), "shared\n");
    write(
        &base.join("home/repo/AGENTS.md"),
        &format!("@{}\n", base.join("config/shared.md").display()),
    );
    assert_eq!(contents(&load(&base, "home/repo")), ["shared\n"]);
}

#[test]
fn outside_a_repository_imports_stay_in_the_importing_files_directory() {
    let (_dir, base) = setup();
    write(&base.join("home/.ssh/id"), "KEY\n");
    write(&base.join("home/dl/pkg/notes.md"), "notes\n");
    write(
        &base.join("home/dl/pkg/AGENTS.md"),
        "@../../.ssh/id\n@notes.md\n",
    );
    let loaded = load(&base, "home/dl/pkg");
    assert_eq!(contents(&loaded), ["@../../.ssh/id\nnotes\n"]);
    assert!(loaded.warnings[0].contains("outside the project"));
}

#[test]
fn the_global_file_may_link_anywhere() {
    let (_dir, base) = setup();
    write(&base.join("dotfiles/agents.md"), "mine\n");
    std::os::unix::fs::symlink(
        base.join("dotfiles/agents.md"),
        base.join("config/AGENTS.md"),
    )
    .unwrap();
    assert_eq!(contents(&load(&base, "home/repo")), ["mine\n"]);
}

#[test]
fn a_directory_named_agents_md_is_skipped_with_a_warning() {
    let (_dir, base) = setup();
    std::fs::create_dir_all(base.join("home/repo/AGENTS.md")).unwrap();
    let loaded = load(&base, "home/repo");
    assert!(loaded.files.is_empty());
    assert!(loaded.warnings[0].contains("not a regular file"));
}

// Final review, critical 1: outside a repository the discovery root can be the home directory,
// so a downloaded folder's instruction file that links elsewhere in the home directory (to
// `~/.aws/credentials`, say) must be skipped, as its imports would be.
#[test]
fn outside_a_repository_an_instruction_file_linking_out_of_its_directory_is_skipped() {
    let (_dir, base) = setup();
    write(&base.join("home/.aws/credentials"), "SECRET=abc\n");
    for name in ["AGENTS.md", "CLAUDE.md"] {
        let pkg = base.join(format!("home/dl/{name}-pkg"));
        std::fs::create_dir_all(&pkg).unwrap();
        std::os::unix::fs::symlink("../../.aws/credentials", pkg.join(name)).unwrap();
        let loaded = load(&base, &format!("home/dl/{name}-pkg"));
        assert!(
            !contents(&loaded).concat().contains("SECRET"),
            "{name}: {:?}",
            loaded.files
        );
        assert!(loaded.files.is_empty(), "{name}: {:?}", loaded.files);
        assert_eq!(loaded.warnings.len(), 1, "{name}: {:?}", loaded.warnings);
        assert!(
            loaded.warnings[0].contains(&format!("skipped {}", pkg.join(name).display()))
                && loaded.warnings[0].contains("outside the project"),
            "{name}: {}",
            loaded.warnings[0]
        );
    }
}

#[test]
fn outside_a_repository_an_instruction_file_may_link_within_its_directory() {
    let (_dir, base) = setup();
    write(&base.join("home/dl/pkg/docs/agents.md"), "rules\n");
    std::os::unix::fs::symlink("docs/agents.md", base.join("home/dl/pkg/AGENTS.md")).unwrap();
    let loaded = load(&base, "home/dl/pkg");
    assert_eq!(contents(&loaded), ["rules\n"]);
    assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
}
