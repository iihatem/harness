//! Runs the real `claude` and `codex` on PATH with `--version` only (no prompt, no login, no
//! configuration read by harness) to check the version parsing against what they print. Ignored
//! by default; run with `cargo test -p harness-delegate --test real_binaries -- --ignored`.

use harness_delegate::{
    BackendKind,
    detect::{DetectOptions, detect},
};

#[tokio::test]
#[ignore = "needs claude on PATH"]
async fn the_installed_claude_reports_a_readable_version() {
    let found = detect(
        BackendKind::Claude,
        &DetectOptions {
            allow_untested: true,
            ..DetectOptions::default()
        },
    )
    .await
    .unwrap();
    println!("claude {} tested={}", found.version, found.tested);
}

#[tokio::test]
#[ignore = "needs codex on PATH"]
async fn the_installed_codex_reports_a_readable_version() {
    let found = detect(
        BackendKind::Codex,
        &DetectOptions {
            allow_untested: true,
            ..DetectOptions::default()
        },
    )
    .await
    .unwrap();
    println!("codex {} tested={}", found.version, found.tested);
}
