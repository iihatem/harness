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
