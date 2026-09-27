//! Runs on every host: whichever sandbox `detect` returns must confine writes to the workspace.

use std::process::Stdio;

use harness_sandbox::{FsAccess, SandboxSettings, detect};

/// Called when `detect` found no sandbox: the test is skipped, unless
/// `HARNESS_REQUIRE_LINUX_SANDBOX=1` says this host must have one (CI's Linux job sets it), in which
/// case it fails, as the other sandbox tests do.
fn skip_unless_required() {
    assert!(
        std::env::var("HARNESS_REQUIRE_LINUX_SANDBOX").as_deref() != Ok("1"),
        "HARNESS_REQUIRE_LINUX_SANDBOX=1 but detect() found no sandbox on this host"
    );
    eprintln!("skipping: no OS sandbox on this host");
}

#[tokio::test]
async fn the_detected_sandbox_confines_writes_to_the_workspace() {
    let Some(sandbox) = detect(SandboxSettings::default()) else {
        skip_unless_required();
        return;
    };
    assert!(
        ["seatbelt", "landlock+seccomp"].contains(&sandbox.name()),
        "{}",
        sandbox.name()
    );
    let ws = tempfile::tempdir().unwrap();
    let ws_path = ws.path().canonicalize().unwrap();
    let outside = std::path::PathBuf::from(std::env::var("HOME").unwrap())
        .join(format!(".harness-sandbox-detect-{}", std::process::id()));
    let script = format!("touch inside.txt && touch {}", outside.display());
    let status = sandbox
        .command(
            FsAccess::WorkspaceWrite,
            &ws_path,
            "/bin/sh",
            &["-c", &script],
        )
        .unwrap()
        .current_dir(&ws_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .unwrap();
    let escaped = outside.exists();
    let _ = std::fs::remove_file(&outside);
    assert!(
        ws_path.join("inside.txt").exists(),
        "writes inside the workspace must work"
    );
    assert!(
        !escaped,
        "the sandbox let a write outside the workspace through"
    );
    assert!(!status.success());
}

#[tokio::test]
async fn read_only_access_blocks_workspace_writes() {
    let Some(sandbox) = detect(SandboxSettings::default()) else {
        skip_unless_required();
        return;
    };
    let ws = tempfile::tempdir().unwrap();
    let ws_path = ws.path().canonicalize().unwrap();
    let status = sandbox
        .command(
            FsAccess::ReadOnly,
            &ws_path,
            "/bin/sh",
            &["-c", "touch nope.txt"],
        )
        .unwrap()
        .current_dir(&ws_path)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .unwrap();
    assert!(!status.success());
    assert!(!ws_path.join("nope.txt").exists());
}
