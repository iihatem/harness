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
