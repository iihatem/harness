//! What the README documents: its example configuration loads, and the sections for gates, language
//! servers, edit formats and the eval are there.

use harness_config::config;

fn readme() -> String {
    std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../README.md"),
    )
    .unwrap()
}

/// The text of each ```toml block.
fn toml_blocks(text: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        match (&mut current, line.trim_end()) {
            (None, "```toml") => current = Some(String::new()),
            (Some(block), "```") => {
                blocks.push(std::mem::take(block));
                current = None;
            }
            (Some(block), _) => {
                block.push_str(line);
                block.push('\n');
            }
            _ => {}
        }
    }
    blocks
}

#[test]
fn every_example_configuration_in_the_readme_loads() {
    let blocks = toml_blocks(&readme());
    assert!(!blocks.is_empty());
    for block in blocks {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, &block).unwrap();
        let parsed = config::parse_file(&path).unwrap_or_else(|e| panic!("{e}\n{block}"));
        assert!(parsed.is_some());
    }
}

#[test]
fn the_example_sets_gates_language_servers_and_an_edit_format() {
    let blocks = toml_blocks(&readme()).join("\n");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, &blocks).unwrap();
    let file = config::parse_file(&path).unwrap().unwrap();
    assert!(file.gates.test.is_some() && file.gates.after_edit.is_some());
    assert!(file.lsp.servers.contains_key("python"));
    assert!(file.profiles.values().any(|p| p.edit_format.is_some()));
}

#[test]
fn the_readme_documents_gates_language_servers_edit_formats_and_the_eval() {
    let text = readme();
    for word in [
        "[gates]",
        "gate_failed",
        "max_retries",
        "[lsp",
        "lsp.wait_ms",
        "diagnostics pending",
        "edit_format",
        "apply_patch",
        "whole_file",
        "hashline",
        "str_replace",
        "cargo xtask eval replay",
        "cargo xtask eval run",
        "eval/results",
        "harness trust",
    ] {
        assert!(text.contains(word), "the README does not mention {word}");
    }
}

#[test]
fn the_example_sets_roles_a_hand_off_a_fallback_chain_and_an_escalation_model() {
    let blocks = toml_blocks(&readme()).join("\n");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, &blocks).unwrap();
    let file = config::parse_file(&path).unwrap().unwrap();
    assert!(file.roles.plan.is_some() && file.roles.build.is_some());
    assert!(file.roles.background.is_some());
    assert!(file.roles.handoff.mode.is_some());
    assert!(!file.fallback.is_empty());
    assert!(file.escalation.to.is_some());
    // And the whole of it is accepted as a configuration, not only as TOML.
    let trust = harness_config::trust::TrustStore::load(&dir.path().join("data")).unwrap();
    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let loaded = config::load(&path, &workspace, &trust).unwrap();
    assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
    assert!(loaded.roles.build.is_some() && loaded.escalation_to.is_some());
}

#[test]
fn the_readme_documents_roles_hand_off_fallback_and_escalation() {
    let text = readme();
    for word in [
        "[roles]",
        "/roles",
        "/model --role",
        "[roles.handoff]",
        "plan_only",
        "HandoffReduced",
        "[fallback]",
        "ModelSwitched",
        "enforced_spend_limit_reached",
        "[escalation]",
        "/escalate",
        "EscalationSuggested",
        "selected_by",
    ] {
        assert!(text.contains(word), "the README does not mention {word}");
    }
    // What needs trust is said.
    let trust = text
        .lines()
        .find(|line| line.starts_with("Project-level `.harness/config.toml` settings"))
        .unwrap();
    for word in ["[roles]", "[fallback]", "[escalation]"] {
        assert!(trust.contains(word), "the trust paragraph omits {word}");
    }
}

#[test]
fn the_readme_documents_subagents() {
    let text = readme();
    for word in [
        "spawn_agents",
        "/agents",
        ".harness/agents/",
        ".claude/agents/",
        "max_spawns_per_turn",
        "permissionMode",
        "explore",
        "`[roles] background`",
        "`model: inherit` means the parent's current model",
        "`sonnet`",
        "is ignored with one note",
        "never turned into a paid API model",
        "may name only a model already in play",
        "`[agents] spawn = false`",
        "`apply_patch`",
        "`hashline_edit`",
        "whether or not the directory is trusted",
        "start a new session",
        "\"unchecked\"",
        "A subagent may run in Claude Code or Codex",
        "`per_delegated`",
        "always works in a worktree",
        "cannot narrow them",
        "`spawn_agents` is not used",
    ] {
        assert!(text.contains(word), "the README does not mention {word}");
    }
}

#[test]
fn the_example_sets_the_agent_limits_and_the_delegation_settings_as_the_code_defaults_have_them() {
    let blocks = toml_blocks(&readme()).join("\n");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, &blocks).unwrap();
    let file = config::parse_file(&path).unwrap().unwrap();
    let a = &file.agents;
    // Every key of `[agents]` the README names is there, and shows the default.
    let d = config::Agents::default();
    assert_eq!(a.spawn, Some(d.spawn));
    assert_eq!(a.per_local, Some(d.per_local));
    assert_eq!(a.per_hosted, Some(d.per_hosted));
    assert_eq!(a.per_delegated, Some(d.per_delegated));
    assert_eq!(a.max_total, Some(d.max_total));
    assert_eq!(a.max_spawns_per_turn, Some(d.max_spawns_per_turn));
    // `[delegate]` and `[delegate.claude]`.
    assert_eq!(file.delegate.allow_untested, Some(false));
    assert_eq!(file.delegate.claude.use_subscription, Some(true));
    // The whole example is accepted as a configuration, with no warning.
    let trust = harness_config::trust::TrustStore::load(&dir.path().join("data")).unwrap();
    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let loaded = config::load(&path, &workspace, &trust).unwrap();
    assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
    assert_eq!(loaded.agents, d);
}

#[test]
fn the_readme_reads_top_to_bottom() {
    let text = readme();
    // The status says what is built, not what comes next.
    let status = text
        .lines()
        .find(|line| line.starts_with("> **Status"))
        .unwrap();
    for word in ["Milestone 1", "Milestone 2", "Milestone 3", "pending"] {
        assert!(status.contains(word), "the status omits {word}");
    }
    assert!(!text.contains("comes next"), "a shipped feature is future");
    // Each part has its own section, with a worked example.
    for heading in [
        "## Subagents",
        "## Claude Code and Codex",
        "## Worktrees and background agents",
        "## The interactive terminal in detail",
    ] {
        assert!(text.lines().any(|l| l == heading), "no {heading}");
    }
    for word in [
        "**Example.**",
        "/bg ",
        "/results",
        "harness worktrees clean",
    ] {
        assert!(text.contains(word), "the README does not mention {word}");
    }
    // "What works today" stays a list of short bullets.
    let start = text.find("## What works today").unwrap();
    let end = text[start..].find("**Sandboxed by default.**").unwrap() + start;
    for line in text[start..end].lines().filter(|l| l.starts_with("- ")) {
        assert!(line.len() < 1300, "too long for the list: {}", &line[..60]);
    }
}

#[test]
fn the_readme_names_the_csv_columns_that_say_which_agent_made_a_request() {
    // The last columns of `usage export --format csv`, as the code writes them.
    let source = include_str!("../../harness-usage/src/export.rs");
    let header = source
        .lines()
        .find_map(|l| l.strip_prefix("const CSV_HEADER: &str = \""))
        .and_then(|l| l.strip_suffix("\";"))
        .expect("the CSV header is in export.rs");
    let columns: Vec<&str> = header.split(',').collect();
    let last = columns[columns.len() - 3..].join(",");
    assert!(
        readme().contains(&last),
        "the README does not name the columns {last}"
    );
    // And what the delegation section says about limits that the code does not have.
    let text = readme();
    for word in ["ignores `max_steps`", "`143` or `129`", "Git 2.31 or newer"] {
        assert!(text.contains(word), "the README does not mention {word}");
    }
}
