use std::path::{Path, PathBuf};

use harness_context::commands::{
    self, Invocation, Scope,
    frontmatter::{self, Frontmatter},
    parse_invocation, split_args,
};

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

fn discover(base: &Path) -> commands::Commands {
    commands::discover(
        &base.join("project"),
        &base.join("config"),
        Some(&base.join("home")),
    )
}

#[test]
fn frontmatter_fields_are_read_and_unknown_ones_ignored() {
    let text = "---\nname: \"OPSX: Propose\"\ndescription: \"Propose a new change\"\nargument-hint: [pr-number] [priority]\nmodel: ollama/qwen3:14b\nallowed-tools: Bash(git add:*), Bash(git status:*), Read\ncategory: \"Workflow\"\ntags: [\"workflow\", \"artifacts\"]\n---\nBody $ARGUMENTS\n";
    let (front, body) = frontmatter::parse(text);
    assert_eq!(
        front,
        Frontmatter {
            description: Some("Propose a new change".into()),
            argument_hint: Some("[pr-number] [priority]".into()),
            model: Some("ollama/qwen3:14b".into()),
            allowed_tools: vec![
                "Bash(git add:*)".into(),
                "Bash(git status:*)".into(),
                "Read".into()
            ],
        }
    );
    assert_eq!(body, "Body $ARGUMENTS\n");
}

#[test]
fn allowed_tools_may_be_a_yaml_list() {
    let block = "---\nallowed-tools:\n  - Bash(openspec:*)\n  - 'Edit'\n---\nx";
    assert_eq!(
        frontmatter::parse(block).0.allowed_tools,
        ["Bash(openspec:*)", "Edit"]
    );
    let flow = "---\nallowed-tools: [Bash(a, b), \"Write\"]\n---\nx";
    assert_eq!(
        frontmatter::parse(flow).0.allowed_tools,
        ["Bash(a, b)", "Write"]
    );
    let unindented = "---\nallowed-tools:\n- Read\n- Grep\ndescription: d\n---\nx";
    let (front, _) = frontmatter::parse(unindented);
    assert_eq!(front.allowed_tools, ["Read", "Grep"]);
    assert_eq!(front.description.as_deref(), Some("d"));
}

#[test]
fn quoted_and_block_values_are_unwrapped() {
    let text = "---\ndescription: 'it''s \"fine\"'\nargument-hint: \"a \\\"b\\\"\"\nmodel: >\n  ollama/\n  x\n---\n";
    let (front, body) = frontmatter::parse(text);
    assert_eq!(front.description.as_deref(), Some("it's \"fine\""));
    assert_eq!(front.argument_hint.as_deref(), Some("a \"b\""));
    assert_eq!(front.model.as_deref(), Some("ollama/ x"));
    assert_eq!(body, "");
}

#[test]
fn a_file_without_closed_frontmatter_is_all_body() {
    assert_eq!(frontmatter::parse("no frontmatter\n").1, "no frontmatter\n");
    assert_eq!(
        frontmatter::parse("---\ndescription: x\n").1,
        "---\ndescription: x\n"
    );
    assert_eq!(frontmatter::parse("---\n---\nbody").1, "body");
}

#[test]
fn a_namespaced_openspec_command_is_found_and_listed() {
    let (_dir, base) = setup();
    write(
        &base.join("project/.claude/commands/opsx/propose.md"),
        "---\ndescription: \"Propose a new change\"\nallowed-tools: Bash(openspec:*)\n---\nPropose.\n",
    );
    let found = discover(&base);
    let command = found.get("opsx:propose").expect("found");
    assert_eq!(command.description.as_deref(), Some("Propose a new change"));
    assert_eq!(command.allowed_tools, ["Bash(openspec:*)"]);
    assert_eq!(command.body, "Propose.\n");
    assert!(
        found
            .listing()
            .contains(&("opsx:propose".into(), "Propose a new change".into()))
    );
    assert_eq!(found.listing()[0].0, "help");
    assert!(found.warnings.is_empty(), "{:?}", found.warnings);
}

#[test]
fn a_command_named_like_a_builtin_is_ignored_with_a_warning() {
    let (_dir, base) = setup();
    write(&base.join("project/.claude/commands/help.md"), "mine\n");
    let found = discover(&base);
    assert!(found.get("help").is_none());
    assert!(found.warnings[0].contains("/help is a built-in command"));
}

#[test]
fn the_first_definition_of_a_name_wins() {
    let (_dir, base) = setup();
    for (dir, body) in [
        ("project/.harness/commands", "harness"),
        ("project/.claude/commands", "claude"),
        ("project/.opencode/commands", "opencode"),
        ("config/commands", "global"),
        ("home/.claude/commands", "claude global"),
    ] {
        write(&base.join(dir).join("x.md"), body);
    }
    write(&base.join("project/.opencode/commands/y.md"), "opencode y");
    write(&base.join("config/commands/y.md"), "global y");
    write(&base.join("home/.claude/commands/z.md"), "claude global z");
    let found = discover(&base);
    assert_eq!(found.get("x").unwrap().body, "harness");
    assert_eq!(found.get("y").unwrap().body, "opencode y");
    assert_eq!(found.get("z").unwrap().body, "claude global z");
    let names: Vec<&str> = found.custom.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["x", "y", "z"]);
}

#[test]
fn each_command_knows_whether_the_project_or_the_user_defined_it() {
    let (_dir, base) = setup();
    write(&base.join("project/.claude/commands/p.md"), "project");
    write(
        &base.join("project/.opencode/commands/shared.md"),
        "project",
    );
    write(&base.join("config/commands/g.md"), "global");
    write(&base.join("config/commands/shared.md"), "global");
    write(&base.join("home/.claude/commands/h.md"), "claude global");
    let found = discover(&base);
    assert_eq!(found.get("p").unwrap().scope, Scope::Project);
    assert_eq!(found.get("g").unwrap().scope, Scope::Global);
    assert_eq!(found.get("h").unwrap().scope, Scope::Global);
    // The project's definition wins, and with it the project's scope.
    assert_eq!(found.get("shared").unwrap().scope, Scope::Project);
}

#[test]
fn nested_directories_become_namespaces_and_hidden_files_are_skipped() {
    let (_dir, base) = setup();
    write(&base.join("project/.claude/commands/a/b/c.md"), "deep");
    write(&base.join("project/.claude/commands/.hidden.md"), "no");
    write(&base.join("project/.claude/commands/notes.txt"), "no");
    write(&base.join("project/.claude/commands/has space.md"), "no");
    let found = discover(&base);
    let names: Vec<&str> = found.custom.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["a:b:c"]);
    assert!(found.warnings[0].contains("cannot contain spaces"));
}

// Review Focus: a cloned repository's command file that is a symlink to a secret must not be sent
// to the model when someone runs it.
#[test]
fn a_project_command_linking_outside_the_project_is_skipped() {
    let (_dir, base) = setup();
    write(&base.join("secret.txt"), "TOKEN=abc");
    std::fs::create_dir_all(base.join("project/.claude/commands")).unwrap();
    std::os::unix::fs::symlink(
        base.join("secret.txt"),
        base.join("project/.claude/commands/leak.md"),
    )
    .unwrap();
    let found = discover(&base);
    assert!(found.get("leak").is_none());
    assert!(
        found.warnings[0].contains("outside"),
        "{:?}",
        found.warnings
    );
}

#[test]
fn slash_input_is_a_command_only_when_it_names_one() {
    assert_eq!(
        parse_invocation("/opsx:propose add-login"),
        Some(Invocation {
            name: "opsx:propose",
            args: "add-login"
        })
    );
    assert_eq!(
        parse_invocation("  /help"),
        Some(Invocation {
            name: "help",
            args: ""
        })
    );
    assert_eq!(
        parse_invocation("/review \"src/lib.rs\" strict\n"),
        Some(Invocation {
            name: "review",
            args: "\"src/lib.rs\" strict"
        })
    );
    assert_eq!(parse_invocation("/usr/bin/env is missing"), None);
    assert_eq!(parse_invocation("why does /x fail"), None);
    assert_eq!(parse_invocation("/ x"), None);
    assert_eq!(parse_invocation("/"), None);
}

#[test]
fn arguments_split_on_whitespace_and_respect_quotes() {
    assert_eq!(
        split_args("\"src/lib.rs\" strict"),
        ["src/lib.rs", "strict"]
    );
    assert_eq!(split_args("  a   b "), ["a", "b"]);
    assert_eq!(
        split_args("'two words' \"say \\\"hi\\\"\""),
        ["two words", "say \"hi\""]
    );
    assert_eq!(split_args("x\"y z\""), ["xy z"]);
    assert_eq!(split_args("\"\""), [""]);
    assert!(split_args("   ").is_empty());
}

// Decision 6: the user's own command files may link anywhere, as dotfiles managers do.
#[test]
fn a_global_command_may_link_anywhere() {
    let (_dir, base) = setup();
    write(&base.join("dotfiles/mine.md"), "mine");
    write(&base.join("dotfiles/tool.md"), "tool");
    std::fs::create_dir_all(base.join("home/.claude/commands")).unwrap();
    std::fs::create_dir_all(base.join("config/commands")).unwrap();
    std::os::unix::fs::symlink(
        base.join("dotfiles/mine.md"),
        base.join("home/.claude/commands/mine.md"),
    )
    .unwrap();
    std::os::unix::fs::symlink(
        base.join("dotfiles/tool.md"),
        base.join("config/commands/tool.md"),
    )
    .unwrap();
    let found = discover(&base);
    assert!(found.warnings.is_empty(), "{:?}", found.warnings);
    assert_eq!(found.get("mine").unwrap().body, "mine");
    assert_eq!(found.get("mine").unwrap().scope, Scope::Global);
    assert_eq!(found.get("tool").unwrap().body, "tool");
}

// Review B, minor 4: a project's commands directory that links elsewhere is not walked.
#[test]
fn a_project_commands_directory_linking_outside_the_project_is_skipped() {
    let (_dir, base) = setup();
    write(&base.join("elsewhere/x.md"), "x");
    write(&base.join("other/commands/z.md"), "z");
    std::fs::create_dir_all(base.join("project/.claude")).unwrap();
    std::os::unix::fs::symlink(
        base.join("elsewhere"),
        base.join("project/.claude/commands"),
    )
    .unwrap();
    std::os::unix::fs::symlink(base.join("other"), base.join("project/.opencode")).unwrap();
    let found = discover(&base);
    assert!(found.get("x").is_none() && found.get("z").is_none());
    assert_eq!(found.warnings.len(), 2, "{:?}", found.warnings);
    assert!(
        found
            .warnings
            .iter()
            .all(|w| w.contains("outside the project")),
        "{:?}",
        found.warnings
    );
}

// One inside the project is read: its files are inside the project too.
#[test]
fn a_project_commands_directory_linking_inside_the_project_is_read() {
    let (_dir, base) = setup();
    write(&base.join("project/tools/commands/y.md"), "y");
    std::fs::create_dir_all(base.join("project/.opencode")).unwrap();
    std::os::unix::fs::symlink("../tools/commands", base.join("project/.opencode/commands"))
        .unwrap();
    let found = discover(&base);
    assert_eq!(found.get("y").unwrap().body, "y");
    assert!(found.warnings.is_empty(), "{:?}", found.warnings);
}

// Review C, minor 6: run from the home directory, the user's own commands stay global.
#[test]
fn commands_in_the_home_directory_stay_global_when_it_is_the_project() {
    let (_dir, base) = setup();
    write(
        &base.join("home/.claude/commands/h.md"),
        "---\nmodel: m/x\n---\nmine",
    );
    let found = commands::discover(
        &base.join("home"),
        &base.join("config"),
        Some(&base.join("home")),
    );
    assert_eq!(found.get("h").unwrap().scope, Scope::Global);
    assert!(found.warnings.is_empty(), "{:?}", found.warnings);
}

// Review B, minor 3: a command file cut at the size limit says so.
#[test]
fn a_command_file_over_the_size_limit_is_cut_with_a_warning() {
    let (_dir, base) = setup();
    let mut text = "---\ndescription: big\n---\n".to_string();
    text.push_str(&"x".repeat(1024 * 1024));
    write(&base.join("project/.claude/commands/big.md"), &text);
    let found = discover(&base);
    let big = found.get("big").unwrap();
    assert_eq!(big.description.as_deref(), Some("big"));
    assert_eq!(
        big.body.len(),
        1024 * 1024 - "---\ndescription: big\n---\n".len()
    );
    assert_eq!(found.warnings.len(), 1, "{:?}", found.warnings);
    assert!(
        found.warnings[0].contains("big.md") && found.warnings[0].contains("only the start"),
        "{:?}",
        found.warnings
    );
}

// Final review, minor 4: a whole namespace kept in a dotfiles repository,
// `~/.claude/commands/opsx -> ~/dotfiles/opsx`, was skipped silently. In a global directory a
// linked subdirectory is followed, and a loop of links ends.
#[test]
fn a_linked_subdirectory_in_a_global_commands_directory_is_followed() {
    let (_dir, base) = setup();
    write(&base.join("dotfiles/opsx/propose.md"), "Propose.\n");
    write(&base.join("dotfiles/opsx/deep/apply.md"), "Apply.\n");
    std::fs::create_dir_all(base.join("home/.claude/commands")).unwrap();
    std::os::unix::fs::symlink(
        base.join("dotfiles/opsx"),
        base.join("home/.claude/commands/opsx"),
    )
    .unwrap();
    // A link back up to a directory being read.
    std::os::unix::fs::symlink("..", base.join("dotfiles/opsx/deep/up")).unwrap();
    std::fs::create_dir_all(base.join("config/commands")).unwrap();
    std::os::unix::fs::symlink(".", base.join("config/commands/self")).unwrap();
    let found = discover(&base);
    let names: Vec<&str> = found.custom.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["opsx:deep:apply", "opsx:propose"], "{names:?}");
    assert!(
        found.custom.iter().all(|c| c.scope == Scope::Global),
        "{:?}",
        found.custom
    );
    assert!(found.warnings.is_empty(), "{:?}", found.warnings);
}

// In a project commands directory a linked subdirectory is still skipped, now with a warning.
#[test]
fn a_linked_subdirectory_in_a_project_commands_directory_is_skipped_with_a_warning() {
    let (_dir, base) = setup();
    write(&base.join("project/tools/opsx/propose.md"), "Propose.\n");
    std::fs::create_dir_all(base.join("project/.claude/commands")).unwrap();
    let link = base.join("project/.claude/commands/opsx");
    std::os::unix::fs::symlink("../../tools/opsx", &link).unwrap();
    let found = discover(&base);
    assert!(found.custom.is_empty(), "{:?}", found.custom);
    assert_eq!(found.warnings.len(), 1, "{:?}", found.warnings);
    assert!(
        found.warnings[0].contains(&format!("skipped {}", link.display()))
            && found.warnings[0].contains("linked directory"),
        "{}",
        found.warnings[0]
    );
}
