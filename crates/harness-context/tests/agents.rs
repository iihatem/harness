use std::path::{Path, PathBuf};

use harness_context::{agents, commands::frontmatter};
use harness_core::{agents::DefinitionSource, permission::Mode};

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// A temp directory resolved through symlinks, holding a project at `project/`, the harness config
/// directory at `config/`, and a home directory at `home/`.
fn setup() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    for sub in ["project", "config", "home"] {
        std::fs::create_dir_all(base.join(sub)).unwrap();
    }
    (dir, base)
}

fn discover(base: &Path) -> agents::Found {
    agents::discover(
        &base.join("project"),
        &base.join("config"),
        Some(&base.join("home")),
    )
}

fn get<'a>(found: &'a agents::Found, name: &str) -> &'a harness_core::agents::AgentDefinition {
    found
        .definitions
        .iter()
        .find(|d| d.name == name)
        .unwrap_or_else(|| panic!("no definition `{name}` in {:?}", found.definitions))
}

#[test]
fn the_generic_reader_gives_every_key_as_text_and_as_a_list() {
    let text = "---\nname: x\ntools: Read, Grep\nlist:\n  - a\n  - 'b'\nflow: [c, d]\ndescription: |\n  two\n  lines\n---\nbody\n";
    let (pairs, body) = frontmatter::parse_map(text);
    let find = |key: &str| pairs.iter().find(|(k, _)| k == key).unwrap().1.clone();
    assert_eq!(find("name").text.as_deref(), Some("x"));
    assert_eq!(find("tools").list, ["Read", "Grep"]);
    assert_eq!(find("list").list, ["a", "b"]);
    assert_eq!(find("flow").list, ["c", "d"]);
    assert_eq!(find("description").text.as_deref(), Some("two\nlines"));
    assert_eq!(body, "body\n");
}

#[test]
fn a_file_with_frontmatter_becomes_a_definition() {
    let (_dir, base) = setup();
    write(
        &base.join("project/.harness/agents/triage.md"),
        "---\nname: triage\ndescription: Sorts failing tests\ntools: Read, Grep, Bash\nmodel: ollama/qwen3:14b\nmode: read-only\nmax_steps: 12\nisolation: worktree\nbackground: true\ncolor: red\n---\nLook at the failures.\n",
    );
    let found = discover(&base);
    assert!(found.warnings.is_empty(), "{:?}", found.warnings);
    let def = get(&found, "triage");
    assert_eq!(def.description, "Sorts failing tests");
    assert_eq!(
        def.tools.as_deref(),
        Some(&["read".to_string(), "grep".into(), "bash".into()][..])
    );
    assert_eq!(def.model.as_deref(), Some("ollama/qwen3:14b"));
    assert_eq!(def.mode, Some(Mode::ReadOnly));
    assert_eq!(def.max_steps, Some(12));
    assert_eq!(def.isolation.as_deref(), Some("worktree"));
    assert_eq!(def.background, Some(true));
    assert_eq!(def.body, "Look at the failures.\n");
    assert_eq!(
        def.source,
        DefinitionSource::Project(base.join("project/.harness/agents/triage.md"))
    );
}

#[test]
fn the_name_is_the_files_when_the_frontmatter_has_none_and_claudes_max_turns_works() {
    let (_dir, base) = setup();
    write(
        &base.join("project/.claude/agents/code-reviewer.md"),
        "---\ndescription: Reviews\nmaxTurns: 7\n---\nReview.\n",
    );
    let found = discover(&base);
    let def = get(&found, "code-reviewer");
    assert_eq!(def.max_steps, Some(7));
    assert_eq!(def.tools, None);
}

#[test]
fn a_file_without_frontmatter_is_all_instructions_and_its_description_the_first_line() {
    let (_dir, base) = setup();
    write(
        &base.join("config/agents/plain.md"),
        "\nDo the thing carefully.\nMore.\n",
    );
    let found = discover(&base);
    let def = get(&found, "plain");
    assert_eq!(def.description, "Do the thing carefully.");
    assert_eq!(def.body, "\nDo the thing carefully.\nMore.\n");
    assert!(matches!(def.source, DefinitionSource::User(_)));
}

#[test]
fn keys_that_would_widen_permissions_are_ignored_with_one_warning_for_the_file() {
    let (_dir, base) = setup();
    let path = base.join("project/.claude/agents/risky.md");
    write(
        &path,
        "---\nname: risky\ndescription: x\npermissionMode: bypassPermissions\nallowed-tools: Bash(rm:*)\nallow: [Bash]\nmode: ask\n---\nb\n",
    );
    let found = discover(&base);
    let def = get(&found, "risky");
    // The mode it asked for is kept (it can only narrow); nothing else widens.
    assert_eq!(def.mode, Some(Mode::Ask));
    assert_eq!(found.warnings.len(), 1, "{:?}", found.warnings);
    let warning = &found.warnings[0];
    assert!(warning.contains(&path.display().to_string()), "{warning}");
    for key in ["permissionMode", "allowed-tools", "allow"] {
        assert!(warning.contains(key), "{warning}");
    }
}

#[test]
fn a_mode_that_is_not_one_is_ignored_with_a_warning_and_unknown_tools_are_dropped() {
    let (_dir, base) = setup();
    write(
        &base.join("config/agents/odd.md"),
        "---\ndescription: x\nmode: yolo\ntools: Read, WebFetch\n---\nb\n",
    );
    let found = discover(&base);
    let def = get(&found, "odd");
    assert_eq!(def.mode, None);
    assert_eq!(def.tools.as_deref(), Some(&["read".to_string()][..]));
    assert!(
        found.warnings.iter().any(|w| w.contains("mode")),
        "{:?}",
        found.warnings
    );
    assert!(
        found.warnings.iter().any(|w| w.contains("WebFetch")),
        "{:?}",
        found.warnings
    );
}

#[test]
fn project_beats_user_and_harness_beats_claude_and_the_search_order_is_stable() {
    let (_dir, base) = setup();
    write(
        &base.join("project/.claude/agents/a.md"),
        "---\ndescription: project claude\n---\n",
    );
    write(
        &base.join("project/.harness/agents/a.md"),
        "---\ndescription: project harness\n---\n",
    );
    write(
        &base.join("config/agents/a.md"),
        "---\ndescription: user config\n---\n",
    );
    write(
        &base.join("home/.claude/agents/a.md"),
        "---\ndescription: user claude\n---\n",
    );
    write(
        &base.join("home/.claude/agents/b.md"),
        "---\ndescription: only user claude\n---\n",
    );
    let found = discover(&base);
    let all: Vec<_> = found
        .definitions
        .iter()
        .map(|d| (d.name.as_str(), d.description.as_str()))
        .collect();
    assert_eq!(all[0], ("a", "project harness"));
    assert!(all.contains(&("b", "only user claude")));
    // Every candidate is returned, in order of precedence, for `Definitions::new` to pick from.
    let a_descriptions: Vec<_> = all
        .iter()
        .filter(|(n, _)| *n == "a")
        .map(|(_, d)| *d)
        .collect();
    assert_eq!(
        a_descriptions,
        [
            "project harness",
            "project claude",
            "user config",
            "user claude"
        ]
    );
    let defs = harness_core::agents::Definitions::new(found.definitions);
    assert_eq!(defs.get("a").unwrap().description, "project harness");
}

#[test]
fn a_bad_name_or_a_non_markdown_file_is_skipped_and_a_project_link_outside_is_refused() {
    let (_dir, base) = setup();
    write(
        &base.join("project/.harness/agents/Bad Name.md"),
        "---\ndescription: x\n---\n",
    );
    write(&base.join("project/.harness/agents/notes.txt"), "no");
    write(
        &base.join("project/.harness/agents/.hidden.md"),
        "---\ndescription: x\n---\n",
    );
    let outside = base.join("elsewhere");
    write(&outside.join("sneaky.md"), "---\ndescription: x\n---\n");
    std::os::unix::fs::symlink(&outside, base.join("project/.claude/agents")).unwrap_or_else(
        |_| {
            // `.claude` does not exist yet: make it, then link the agents directory.
            std::fs::create_dir_all(base.join("project/.claude")).unwrap();
            std::os::unix::fs::symlink(&outside, base.join("project/.claude/agents")).unwrap();
        },
    );
    let found = discover(&base);
    assert!(found.definitions.is_empty(), "{:?}", found.definitions);
    assert!(
        found.warnings.iter().any(|w| w.contains("Bad Name.md")),
        "{:?}",
        found.warnings
    );
    assert!(
        found
            .warnings
            .iter()
            .any(|w| w.contains("outside the project")),
        "{:?}",
        found.warnings
    );
}

#[test]
fn a_linked_file_in_a_project_directory_is_skipped_but_the_users_own_may_link() {
    let (_dir, base) = setup();
    let target = base.join("target.md");
    write(&target, "---\ndescription: x\n---\n");
    std::fs::create_dir_all(base.join("project/.harness/agents")).unwrap();
    std::os::unix::fs::symlink(&target, base.join("project/.harness/agents/linked.md")).unwrap();
    std::fs::create_dir_all(base.join("config/agents")).unwrap();
    std::os::unix::fs::symlink(&target, base.join("config/agents/mine.md")).unwrap();
    let found = discover(&base);
    assert!(found.definitions.iter().all(|d| d.name != "linked"));
    assert!(
        found.warnings.iter().any(|w| w.contains("linked.md")),
        "{:?}",
        found.warnings
    );
    assert_eq!(get(&found, "mine").description, "x");
}

#[test]
fn a_long_description_is_one_line_and_cut() {
    let (_dir, base) = setup();
    let long = "word ".repeat(200);
    write(
        &base.join("config/agents/long.md"),
        &format!("---\ndescription: |\n  {long}\n  next line\n---\nb\n"),
    );
    let found = discover(&base);
    let description = &get(&found, "long").description;
    assert!(!description.contains('\n'));
    assert!(
        description.chars().count() <= 301,
        "{}",
        description.chars().count()
    );
}
