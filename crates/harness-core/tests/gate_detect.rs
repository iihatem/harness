//! Which gate commands harness proposes for a workspace, from the files in it.

use harness_core::gate::{Proposal, detect};

fn workspace(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (name, text) in files {
        std::fs::write(dir.path().join(name), text).unwrap();
    }
    dir
}

fn proposal(files: &[(&str, &str)]) -> Option<Proposal> {
    detect(workspace(files).path())
}

// Spec "Proposal in a Rust workspace".
#[test]
fn cargo_toml_proposes_cargo_test() {
    let p = proposal(&[("Cargo.toml", "[package]\nname = \"x\"\n")]).unwrap();
    assert_eq!(p.source, "Cargo.toml");
    assert_eq!(p.test.as_deref(), Some("cargo test"));
    assert_eq!(p.after_edit, None);
}

#[test]
fn package_json_proposes_its_test_and_lint_scripts() {
    let p = proposal(&[(
        "package.json",
        r#"{"scripts": {"test": "vitest run", "lint": "eslint ."}}"#,
    )])
    .unwrap();
    assert_eq!(p.source, "package.json");
    assert_eq!(p.test.as_deref(), Some("npm test"));
    assert_eq!(p.after_edit.as_deref(), Some("npm run lint"));
}

#[test]
fn package_json_with_only_one_script_proposes_only_that() {
    let p = proposal(&[("package.json", r#"{"scripts": {"lint": "eslint ."}}"#)]).unwrap();
    assert_eq!(p.test, None);
    assert_eq!(p.after_edit.as_deref(), Some("npm run lint"));
}

// `npm init` writes a test script that only fails: it is no gate.
#[test]
fn package_jsons_placeholder_test_script_is_not_proposed() {
    let placeholder = r#"{"scripts": {"test": "echo \"Error: no test specified\" && exit 1"}}"#;
    assert_eq!(proposal(&[("package.json", placeholder)]), None);
}

#[test]
fn package_json_without_scripts_or_not_json_proposes_nothing() {
    assert_eq!(proposal(&[("package.json", "{}")]), None);
    assert_eq!(proposal(&[("package.json", "not json")]), None);
    assert_eq!(proposal(&[("package.json", r#"{"scripts": 3}"#)]), None);
}

#[test]
fn pyproject_toml_that_uses_pytest_proposes_pytest() {
    let p = proposal(&[(
        "pyproject.toml",
        "[project]\nname = \"x\"\n[tool.pytest.ini_options]\ntestpaths = [\"tests\"]\n",
    )])
    .unwrap();
    assert_eq!(p.source, "pyproject.toml");
    assert_eq!(p.test.as_deref(), Some("pytest"));
    let p = proposal(&[(
        "pyproject.toml",
        "[project]\nname = \"x\"\n[project.optional-dependencies]\ndev = [\"pytest>=8\"]\n",
    )])
    .unwrap();
    assert_eq!(p.test.as_deref(), Some("pytest"));
}

#[test]
fn pyproject_toml_without_pytest_proposes_nothing() {
    assert_eq!(
        proposal(&[("pyproject.toml", "[project]\nname = \"x\"\n")]),
        None
    );
}

#[test]
fn go_mod_proposes_go_test() {
    let p = proposal(&[("go.mod", "module x\n\ngo 1.22\n")]).unwrap();
    assert_eq!(p.source, "go.mod");
    assert_eq!(p.test.as_deref(), Some("go test ./..."));
}

#[test]
fn a_workspace_with_none_of_the_files_proposes_nothing() {
    assert_eq!(proposal(&[("README.md", "hi")]), None);
}

// A repository with several of them gets one proposal, in the spec's order: the user can edit it.
#[test]
fn the_first_known_file_wins() {
    let p = proposal(&[("go.mod", "module x\n"), ("Cargo.toml", "[package]\n")]).unwrap();
    assert_eq!(p.source, "Cargo.toml");
}

#[test]
fn the_proposal_describes_itself() {
    let p = proposal(&[("Cargo.toml", "[package]\n")]).unwrap();
    assert_eq!(
        p.describe(),
        "Cargo.toml: run `cargo test` when a turn that changed files ends"
    );
    let p = proposal(&[(
        "package.json",
        r#"{"scripts": {"test": "jest", "lint": "eslint ."}}"#,
    )])
    .unwrap();
    assert_eq!(
        p.describe(),
        "package.json: run `npm run lint` after each edit, and `npm test` when a turn that changed files ends"
    );
}
