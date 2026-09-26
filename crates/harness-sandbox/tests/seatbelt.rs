#![cfg(target_os = "macos")]
//! End-to-end tests against the real `/usr/bin/sandbox-exec` on this Mac.
//!
//! Each test builds its own temp workspace via `tempfile::tempdir()`. Per
//! the task brief, the whole file is skipped (with a note) when
//! `HARNESS_SANDBOX` is set, since that means these tests are themselves
//! running nested inside a sandbox and `sandbox-exec` would not behave the
//! same way (or might not be reachable at all).

use std::path::{Path, PathBuf};
use std::time::Instant;

use harness_sandbox::{FsAccess, SandboxPolicy, looks_like_sandbox_denial, seatbelt_command};

/// Returns `true` (after printing a note) if this whole test should be
/// skipped because we're running nested inside another sandbox.
fn skip_if_nested(test_name: &str) -> bool {
    if std::env::var_os("HARNESS_SANDBOX").is_some() {
        eprintln!(
            "[seatbelt.rs] skipping `{test_name}`: HARNESS_SANDBOX is set, \
             so we appear to be running nested inside a sandbox already"
        );
        true
    } else {
        false
    }
}

fn read_only(workspace: &Path) -> SandboxPolicy {
    SandboxPolicy {
        access: FsAccess::ReadOnly,
        workspace: workspace.to_path_buf(),
        extra_writable: Vec::new(),
        allow_localhost: false,
    }
}

fn workspace_write(workspace: &Path) -> SandboxPolicy {
    SandboxPolicy {
        access: FsAccess::WorkspaceWrite,
        workspace: workspace.to_path_buf(),
        extra_writable: Vec::new(),
        allow_localhost: false,
    }
}

/// Runs `program args…` under `policy` with `cwd` as the working directory,
/// waits for completion, and returns `(exit_code, combined_stdout_stderr)`.
async fn run(
    policy: &SandboxPolicy,
    program: &str,
    args: &[&str],
    cwd: &Path,
) -> (Option<i32>, String) {
    let mut cmd = seatbelt_command(policy, program, args).expect("build seatbelt command");
    cmd.current_dir(cwd);
    cmd.kill_on_drop(true);
    let output = cmd
        .output()
        .await
        .expect("spawn/wait for sandboxed command");
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    (output.status.code(), text)
}

fn canonical_tempdir() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("create tempdir");
    let canon = dir.path().canonicalize().expect("canonicalize tempdir");
    (dir, canon)
}

// ---------------------------------------------------------------------------
// 1. availability
// ---------------------------------------------------------------------------

#[test]
fn test_1_seatbelt_available() {
    if skip_if_nested("test_1_seatbelt_available") {
        return;
    }
    assert!(
        harness_sandbox::seatbelt_available(),
        "expected sandbox-exec to be available on this Mac"
    );
}

// ---------------------------------------------------------------------------
// 2. ReadOnly
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_2_read_only() {
    if skip_if_nested("test_2_read_only") {
        return;
    }
    let (_dir, ws) = canonical_tempdir();
    let policy = read_only(&ws);

    let (code, text) = run(&policy, "cat", &["/etc/hosts"], &ws).await;
    assert_eq!(code, Some(0), "cat /etc/hosts should succeed: {text}");

    let (code, text) = run(&policy, "touch", &["a"], &ws).await;
    assert_ne!(
        code,
        Some(0),
        "touch ws/a should fail under ReadOnly: {text}"
    );
    assert!(!ws.join("a").exists());

    let tmpdir = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_string());
    let probe = format!(
        "{}/harness_proto_ro_probe_{}",
        tmpdir.trim_end_matches('/'),
        std::process::id()
    );
    let _ = std::fs::remove_file(&probe);
    let (code, text) = run(&policy, "touch", &[probe.as_str()], &ws).await;
    assert_ne!(
        code,
        Some(0),
        "touch $TMPDIR/x should fail under ReadOnly: {text}"
    );
    assert!(!Path::new(&probe).exists());
    let _ = std::fs::remove_file(&probe);
}

// ---------------------------------------------------------------------------
// 3. WorkspaceWrite
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_3_workspace_write() {
    if skip_if_nested("test_3_workspace_write") {
        return;
    }
    let (_dir, ws) = canonical_tempdir();
    let policy = workspace_write(&ws);

    let (code, text) = run(&policy, "touch", &["a"], &ws).await;
    assert_eq!(code, Some(0), "touch ws/a should succeed: {text}");
    assert!(ws.join("a").exists());

    let (code, text) = run(&policy, "mkdir", &["-p", "b/c"], &ws).await;
    assert_eq!(code, Some(0), "mkdir -p ws/b/c should succeed: {text}");
    assert!(ws.join("b/c").is_dir());

    let (code, text) = run(&policy, "mktemp", &["-t", "harnessproto"], &ws).await;
    assert_eq!(code, Some(0), "mktemp should succeed: {text}");

    let home = std::env::var("HOME").expect("HOME must be set");
    let probe = format!("{home}/.harness_proto_home_probe_{}", std::process::id());
    let _ = std::fs::remove_file(&probe);
    let (code, text) = run(&policy, "touch", &[probe.as_str()], &ws).await;
    assert_ne!(
        code,
        Some(0),
        "touch $HOME/.harness_proto_home_probe_<pid> should fail: {text}"
    );
    assert!(
        !Path::new(&probe).exists(),
        "probe file must not exist after the denied write"
    );
    let _ = std::fs::remove_file(&probe);
}

// ---------------------------------------------------------------------------
// 4. non-canonical workspace path
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_4_non_canonical_workspace() {
    if skip_if_nested("test_4_non_canonical_workspace") {
        return;
    }
    let dir = tempfile::tempdir().expect("create tempdir");
    // Deliberately use the raw (possibly non-canonical, e.g. /var/folders/...
    // symlinked from /private/var/folders/...) path rather than canonicalizing
    // it ourselves — the builder is responsible for that.
    let raw_ws = dir.path().to_path_buf();
    let policy = workspace_write(&raw_ws);

    let (code, text) = run(&policy, "touch", &["a"], &raw_ws).await;
    assert_eq!(
        code,
        Some(0),
        "touch should succeed even with a non-canonical workspace path: {text}"
    );
    assert!(raw_ws.join("a").exists());
}

// ---------------------------------------------------------------------------
// 5. .git protections
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_5_git_protections() {
    if skip_if_nested("test_5_git_protections") {
        return;
    }
    let (_dir, ws) = canonical_tempdir();
    let policy = workspace_write(&ws);

    // git init runs OUTSIDE the sandbox.
    let status = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&ws)
        .status()
        .expect("run git init");
    assert!(status.success(), "git init should succeed");

    let (code, text) = run(&policy, "sh", &["-c", "echo x >> .git/config"], &ws).await;
    assert_ne!(code, Some(0), "writing .git/config should fail: {text}");

    let (code, text) = run(&policy, "touch", &[".git/hooks/pre-commit"], &ws).await;
    assert_ne!(
        code,
        Some(0),
        "writing .git/hooks/pre-commit should fail: {text}"
    );

    let (code, text) = run(&policy, "mv", &[".git", ".gitx"], &ws).await;
    assert_ne!(code, Some(0), "mv .git should fail: {text}");
    assert!(
        ws.join(".git").exists(),
        ".git must still be at its original path"
    );

    let (code, text) = run(&policy, "touch", &["HEAD"], &ws).await;
    assert_ne!(code, Some(0), "touch ws/HEAD should fail: {text}");
    assert!(!ws.join("HEAD").exists());

    // git commit / stash writes inside .git must still work.
    let (code, text) = run(
        &policy,
        "git",
        &[
            "-c",
            "user.email=a@b.com",
            "-c",
            "user.name=a",
            "commit",
            "--allow-empty",
            "-m",
            "m",
        ],
        &ws,
    )
    .await;
    assert_eq!(
        code,
        Some(0),
        "git commit --allow-empty should succeed: {text}"
    );

    std::fs::write(ws.join("f.txt"), b"hello").expect("write f.txt");
    let (code, text) = run(&policy, "git", &["add", "f.txt"], &ws).await;
    assert_eq!(code, Some(0), "git add should succeed: {text}");

    let (code, text) = run(
        &policy,
        "git",
        &["-c", "user.email=a@b.com", "-c", "user.name=a", "stash"],
        &ws,
    )
    .await;
    assert_eq!(code, Some(0), "git stash should succeed: {text}");

    // Unlinking the workspace itself must be denied.
    let (_dir2, other_cwd) = canonical_tempdir();
    let ws_str = ws
        .to_str()
        .expect("workspace path is valid UTF-8")
        .to_string();
    let (code, text) = run(&policy, "rmdir", &[ws_str.as_str()], &other_cwd).await;
    assert_ne!(
        code,
        Some(0),
        "rmdir of the workspace itself should fail: {text}"
    );
    assert!(ws.exists(), "workspace directory must still exist");
}

// ---------------------------------------------------------------------------
// 6. network
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_6_network_denied() {
    if skip_if_nested("test_6_network_denied") {
        return;
    }
    let (_dir, ws) = canonical_tempdir();
    let policy = workspace_write(&ws);

    let (code, text) = run(
        &policy,
        "curl",
        &["-sS", "--max-time", "5", "https://example.com"],
        &ws,
    )
    .await;
    assert_ne!(code, Some(0), "curl should fail with no network: {text}");
    assert!(
        looks_like_sandbox_denial(code, &text, true),
        "curl failure should be classified as a sandbox denial; output was: {text}"
    );

    let python_script = "\
import socket, sys
s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
s.settimeout(3)
try:
    s.connect(('1.1.1.1', 53))
    print('CONNECTED')
    sys.exit(0)
except OSError as e:
    print('CONNECT_FAILED', e)
    sys.exit(1)
";
    let (code, text) = run(&policy, "python3", &["-c", python_script], &ws).await;
    assert_ne!(
        code,
        Some(0),
        "python socket connect should fail with no network: {text}"
    );
}

// ---------------------------------------------------------------------------
// 7. child inherits sandbox
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_7_child_inherits_sandbox() {
    if skip_if_nested("test_7_child_inherits_sandbox") {
        return;
    }
    let (_dir, ws) = canonical_tempdir();
    let policy = workspace_write(&ws);

    let home = std::env::var("HOME").expect("HOME must be set");
    let probe = format!("{home}/.harness_proto_child_probe_{}", std::process::id());
    let _ = std::fs::remove_file(&probe);

    let script = format!("bash -c \"touch '{probe}'\"");
    let (code, text) = run(&policy, "sh", &["-c", &script], &ws).await;
    assert_ne!(
        code,
        Some(0),
        "grandchild `touch $HOME/...` should still be denied by the inherited sandbox: {text}"
    );
    assert!(!Path::new(&probe).exists());
    let _ = std::fs::remove_file(&probe);
}

// ---------------------------------------------------------------------------
// 8. symlink escape
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_8_symlink_escape() {
    if skip_if_nested("test_8_symlink_escape") {
        return;
    }
    let (_dir, ws) = canonical_tempdir();
    let policy = workspace_write(&ws);

    let home = std::env::var("HOME").expect("HOME must be set");
    let link = ws.join("link");
    std::os::unix::fs::symlink(&home, &link).expect("create symlink ws/link -> $HOME");

    let probe_name = format!(".harness_proto_symlink_probe_{}", std::process::id());
    let probe = format!("{home}/{probe_name}");
    let _ = std::fs::remove_file(&probe);

    let (code, text) = run(&policy, "touch", &[&format!("link/{probe_name}")], &ws).await;
    assert_ne!(
        code,
        Some(0),
        "writing through a symlink that escapes the workspace should fail: {text}"
    );
    assert!(
        !Path::new(&probe).exists(),
        "escaped write must not have happened"
    );
    let _ = std::fs::remove_file(&probe);
}

// ---------------------------------------------------------------------------
// 9. denial classifier
// ---------------------------------------------------------------------------

#[test]
fn test_9_denial_classifier() {
    if skip_if_nested("test_9_denial_classifier") {
        return;
    }
    assert!(looks_like_sandbox_denial(
        Some(1),
        "touch: /workspace/a: Operation not permitted\n",
        false
    ));
    assert!(!looks_like_sandbox_denial(Some(1), "error: boom\n", false));
    assert!(!looks_like_sandbox_denial(
        Some(0),
        "Operation not permitted\n",
        false
    ));
    assert!(!looks_like_sandbox_denial(
        Some(127),
        "sh: foo: command not found\n",
        false
    ));
    assert!(!looks_like_sandbox_denial(
        Some(71),
        "sandbox-exec: some internal failure\n",
        false
    ));
}

// ---------------------------------------------------------------------------
// 10. allow_localhost
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_10_allow_localhost() {
    if skip_if_nested("test_10_allow_localhost") {
        return;
    }
    let (_dir, ws) = canonical_tempdir();

    let bind_script = "\
import socket
s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
s.bind(('127.0.0.1', 0))
print('BOUND', s.getsockname())
";

    let mut allowed = workspace_write(&ws);
    allowed.allow_localhost = true;
    let (code, text) = run(&allowed, "python3", &["-c", bind_script], &ws).await;
    assert_eq!(
        code,
        Some(0),
        "binding 127.0.0.1:0 should succeed with allow_localhost: {text}"
    );

    let denied = workspace_write(&ws);
    assert!(!denied.allow_localhost);
    let (code, text) = run(&denied, "python3", &["-c", bind_script], &ws).await;
    assert_ne!(
        code,
        Some(0),
        "binding 127.0.0.1:0 should fail without allow_localhost: {text}"
    );
}

// ---------------------------------------------------------------------------
// 11. overhead
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_11_overhead() {
    if skip_if_nested("test_11_overhead") {
        return;
    }
    let (_dir, ws) = canonical_tempdir();
    let policy = workspace_write(&ws);

    const ITERATIONS: u32 = 20;
    let start = Instant::now();
    for _ in 0..ITERATIONS {
        let (code, text) = run(&policy, "true", &[], &ws).await;
        assert_eq!(code, Some(0), "true should succeed: {text}");
    }
    let elapsed = start.elapsed();
    let avg_ms = elapsed.as_secs_f64() * 1000.0 / f64::from(ITERATIONS);
    println!(
        "[test_11_overhead] {ITERATIONS} sandboxed spawns of `true` took {:.2} ms total, {avg_ms:.2} ms/spawn average",
        elapsed.as_secs_f64() * 1000.0
    );
}
