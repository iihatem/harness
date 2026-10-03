//! rust-analyzer, where it is installed: a real server reporting a real type error. Skipped
//! (with a note) where `rust-analyzer --version` does not run, unless `HARNESS_REQUIRE_RA` is
//! set, which CI sets where it installs it.

use std::{collections::BTreeMap, path::PathBuf, time::Duration};

use harness_core::{
    diag::{Diagnostics, EditedFiles},
    permission::FsAccess,
};
use harness_lsp::{LspDiagnostics, Manager, Settings};

fn installed() -> bool {
    std::process::Command::new("rust-analyzer")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

#[tokio::test]
async fn rust_analyzer_reports_a_type_error_after_an_edit() {
    if !installed() {
        assert!(
            std::env::var_os("HARNESS_REQUIRE_RA").is_none_or(|v| v.is_empty()),
            "HARNESS_REQUIRE_RA is set, but rust-analyzer is not installed"
        );
        eprintln!("skipped: rust-analyzer is not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    let lib = root.join("src/lib.rs");
    std::fs::write(&lib, "pub fn one() -> i32 { 1 }\n").unwrap();
    let path: Vec<PathBuf> = std::env::split_paths(&std::env::var_os("PATH").unwrap()).collect();
    let d = LspDiagnostics::new(
        Manager::new(
            Settings {
                enabled: true,
                wait: Duration::from_secs(20),
                // Indexing, and the first `cargo check`, take a while.
                first_wait: Duration::from_secs(120),
                servers: BTreeMap::new(),
                trusted: true,
                allowed: None,
                path,
                init_timeout: Duration::from_secs(60),
            },
            root.clone(),
        ),
        root.clone(),
    );
    std::fs::write(&lib, "pub fn one() -> i32 { \"one\" }\n").unwrap();
    let said = d
        .after_edit(&EditedFiles {
            paths: &[lib],
            workspace: &root,
            sandbox: None,
            access: FsAccess::WorkspaceWrite,
            unsandboxed_ok: true,
        })
        .await
        .unwrap_or_default();
    d.shutdown().await;
    assert!(said.contains("src/lib.rs:1:"), "{said}");
    assert!(said.contains("mismatched types"), "{said}");
}
