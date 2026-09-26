//! Integration tests for the Linux sandbox backend.
//!
//! These only compile — and only run — on Linux (`cfg(target_os =
//! "linux")` below strips the whole file to nothing on any other target,
//! notably the macOS host this prototype was authored on). They are
//! exercised by `cargo test --target {aarch64,x86_64}-unknown-linux-gnu` in
//! CI, never locally on this machine.
//!
//! Every test calls [`skip_if_unavailable`] first and returns early (with a
//! diagnostic on stderr) rather than failing outright when the CI kernel
//! turns out not to support Landlock ABI >= 2 — the point of these tests is
//! to exercise the sandbox where it *is* available, not to require every CI
//! runner to have a recent-enough kernel.

#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use harness_sandbox::{
    FsAccess, SandboxPolicy, landlock_abi, linux_sandbox_available, linux_sandbox_command,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Creates a fresh, empty directory via `mktemp -d` and removes it (best
/// effort) when the returned guard is dropped.
struct TempWorkspace(PathBuf);

impl TempWorkspace {
    fn new() -> Self {
        let output = std::process::Command::new("mktemp")
            .arg("-d")
            .output()
            .expect("mktemp -d should exist on any Linux CI runner");
        assert!(output.status.success(), "mktemp -d failed");
        let path = PathBuf::from(String::from_utf8(output.stdout).unwrap().trim());
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn workspace_write_policy(workspace: &Path) -> SandboxPolicy {
    SandboxPolicy {
        access: FsAccess::WorkspaceWrite,
        workspace: workspace.to_path_buf(),
        extra_writable: Vec::new(),
        allow_localhost: false,
    }
}

fn read_only_policy(workspace: &Path) -> SandboxPolicy {
    SandboxPolicy {
        access: FsAccess::ReadOnly,
        workspace: workspace.to_path_buf(),
        extra_writable: Vec::new(),
        allow_localhost: false,
    }
}

/// Runs `program args...` under `policy`, with `policy.workspace` as the
/// working directory, and collects its output.
async fn run(policy: &SandboxPolicy, program: &str, args: &[&str]) -> std::process::Output {
    let mut cmd = linux_sandbox_command(policy, program, args).expect("build sandboxed command");
    cmd.current_dir(&policy.workspace);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.output().await.expect("spawn sandboxed command")
}

fn stderr_of(output: &std::process::Output) -> std::borrow::Cow<'_, str> {
    String::from_utf8_lossy(&output.stderr)
}

/// Returns `true` (and logs a skip reason) when the current kernel/arch
/// cannot run the Linux sandbox at all, so callers can bail out early
/// instead of failing a test that Landlock/seccomp support was never a
/// precondition of exercising.
fn skip_if_unavailable() -> bool {
    if linux_sandbox_available() {
        false
    } else {
        eprintln!(
            "skipping: linux sandbox unavailable (landlock_abi = {:?})",
            landlock_abi()
        );
        true
    }
}

fn command_exists(name: &str) -> bool {
    std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {name}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

// ---------------------------------------------------------------------------
// Detection
// ---------------------------------------------------------------------------

#[test]
fn detection_requires_landlock_abi_at_least_2() {
    if linux_sandbox_available() {
        let abi = landlock_abi().expect("linux_sandbox_available() implies landlock_abi() is Some");
        assert!(abi >= 2, "sandbox reported available at Landlock ABI {abi}");
    }
}

// ---------------------------------------------------------------------------
// WorkspaceWrite: filesystem
// ---------------------------------------------------------------------------

#[tokio::test]
async fn workspace_write_allows_touch_inside_workspace() {
    if skip_if_unavailable() {
        return;
    }
    let ws = TempWorkspace::new();
    let policy = workspace_write_policy(ws.path());
    let target = ws.path().join("touched");

    let output = run(&policy, "touch", &[target.to_str().unwrap()]).await;

    assert!(output.status.success(), "stderr: {}", stderr_of(&output));
    assert!(target.exists());
}

#[tokio::test]
async fn workspace_write_denies_touch_outside_workspace_in_home() {
    if skip_if_unavailable() {
        return;
    }
    let Some(home) = std::env::var_os("HOME") else {
        eprintln!("skipping: $HOME not set");
        return;
    };
    let ws = TempWorkspace::new();
    let policy = workspace_write_policy(ws.path());
    let target = PathBuf::from(home).join(format!("proto-landlock-denied-{}", std::process::id()));

    let output = run(&policy, "touch", &[target.to_str().unwrap()]).await;

    assert!(!output.status.success());
    assert!(
        stderr_of(&output).contains("Permission denied"),
        "stderr: {}",
        stderr_of(&output)
    );
    assert!(!target.exists());
}

#[tokio::test]
async fn workspace_write_allows_mktemp() {
    if skip_if_unavailable() {
        return;
    }
    let ws = TempWorkspace::new();
    let policy = workspace_write_policy(ws.path());

    let output = run(&policy, "mktemp", &[]).await;

    assert!(output.status.success(), "stderr: {}", stderr_of(&output));
}

#[tokio::test]
async fn always_writable_devices_allow_redirect_to_dev_null() {
    if skip_if_unavailable() {
        return;
    }
    let ws = TempWorkspace::new();
    let policy = workspace_write_policy(ws.path());

    let output = run(&policy, "sh", &["-c", ": > /dev/null"]).await;

    assert!(output.status.success(), "stderr: {}", stderr_of(&output));
}

#[tokio::test]
async fn always_writable_devices_are_writable_even_under_read_only() {
    if skip_if_unavailable() {
        return;
    }
    let ws = TempWorkspace::new();
    let policy = read_only_policy(ws.path());

    let output = run(&policy, "sh", &["-c", ": > /dev/null"]).await;

    assert!(output.status.success(), "stderr: {}", stderr_of(&output));
}

#[tokio::test]
async fn workspace_write_allows_git_init_and_empty_commit() {
    if skip_if_unavailable() {
        return;
    }
    if !command_exists("git") {
        eprintln!("skipping: git not installed");
        return;
    }
    let ws = TempWorkspace::new();
    let policy = workspace_write_policy(ws.path());
    let script = "set -e; git init -q; \
                  git -c user.email=test@example.com -c user.name=test \
                      commit --allow-empty -q -m init";

    let output = run(&policy, "sh", &["-c", script]).await;

    assert!(output.status.success(), "stderr: {}", stderr_of(&output));
}

// ---------------------------------------------------------------------------
// ReadOnly: filesystem
// ---------------------------------------------------------------------------

#[tokio::test]
async fn read_only_denies_touch_inside_workspace() {
    if skip_if_unavailable() {
        return;
    }
    let ws = TempWorkspace::new();
    let policy = read_only_policy(ws.path());
    let target = ws.path().join("should-not-exist");

    let output = run(&policy, "touch", &[target.to_str().unwrap()]).await;

    assert!(!output.status.success());
    assert!(!target.exists());
}

// ---------------------------------------------------------------------------
// Network
// ---------------------------------------------------------------------------

#[tokio::test]
async fn network_denies_dev_tcp_redirect() {
    if skip_if_unavailable() {
        return;
    }
    if !command_exists("bash") {
        eprintln!("skipping: bash not installed");
        return;
    }
    let ws = TempWorkspace::new();
    let policy = workspace_write_policy(ws.path());

    let output = run(&policy, "bash", &["-c", "exec 3<>/dev/tcp/1.1.1.1/53"]).await;

    assert!(!output.status.success());
}

#[tokio::test]
async fn network_denies_af_inet_af_inet6_and_udp_socket_creation_with_eperm() {
    if skip_if_unavailable() {
        return;
    }
    if !command_exists("python3") {
        eprintln!("skipping: python3 not installed");
        return;
    }
    let ws = TempWorkspace::new();
    let policy = workspace_write_policy(ws.path());
    let script = r#"
import errno
import socket
import sys

cases = [
    (socket.AF_INET, socket.SOCK_STREAM, "AF_INET"),
    (socket.AF_INET6, socket.SOCK_STREAM, "AF_INET6"),
    (socket.AF_INET, socket.SOCK_DGRAM, "UDP"),
]
for family, kind, label in cases:
    try:
        socket.socket(family, kind)
    except OSError as exc:
        if exc.errno != errno.EPERM:
            print(f"{label}: expected EPERM, got {exc.errno}", file=sys.stderr)
            sys.exit(1)
    else:
        print(f"{label}: socket() unexpectedly succeeded", file=sys.stderr)
        sys.exit(1)
print("ok")
"#;

    let output = run(&policy, "python3", &["-c", script]).await;

    assert!(output.status.success(), "stderr: {}", stderr_of(&output));
}

#[tokio::test]
async fn network_allows_af_unix_socketpair() {
    if skip_if_unavailable() {
        return;
    }
    if !command_exists("python3") {
        eprintln!("skipping: python3 not installed");
        return;
    }
    let ws = TempWorkspace::new();
    let policy = workspace_write_policy(ws.path());
    let script = "import socket; socket.socketpair(); print('ok')";

    let output = run(&policy, "python3", &["-c", script]).await;

    assert!(output.status.success(), "stderr: {}", stderr_of(&output));
}

// ---------------------------------------------------------------------------
// Process hierarchy
// ---------------------------------------------------------------------------

#[tokio::test]
async fn grandchild_inherits_the_sandbox() {
    if skip_if_unavailable() {
        return;
    }
    let Some(home) = std::env::var_os("HOME") else {
        eprintln!("skipping: $HOME not set");
        return;
    };
    let ws = TempWorkspace::new();
    let policy = workspace_write_policy(ws.path());
    let target =
        PathBuf::from(&home).join(format!("proto-landlock-grandchild-{}", std::process::id()));
    let script = format!("sh -c 'touch {}'", target.display());

    let output = run(&policy, "sh", &["-c", &script]).await;

    assert!(!output.status.success());
    assert!(!target.exists());
}

#[tokio::test]
async fn parent_process_is_not_sandboxed_after_spawning() {
    if skip_if_unavailable() {
        return;
    }
    let ws = TempWorkspace::new();
    let policy = read_only_policy(ws.path());

    // A fully-restricted child runs and exits normally...
    let output = run(&policy, "true", &[]).await;
    assert!(output.status.success());

    // ...and this test process itself (the parent) was never restricted:
    // `pre_exec` only ever runs in the forked child, so the sandbox must
    // not have leaked back onto the caller.
    let home_tmp = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join(format!(
            "proto-landlock-parent-check-{}",
            std::process::id()
        ));
    std::fs::write(&home_tmp, b"still unrestricted")
        .expect("parent process should still be able to write under $HOME");
    let _ = std::fs::remove_file(&home_tmp);

    let listener = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("parent process should still be able to bind a loopback socket");
    drop(listener);
}

#[tokio::test]
async fn killpg_of_child_pid_kills_background_grandchild() {
    if skip_if_unavailable() {
        return;
    }
    let ws = TempWorkspace::new();
    let policy = workspace_write_policy(ws.path());
    let pid_file = ws.path().join("grandchild.pid");
    // Backgrounds a long sleep (the grandchild) and waits on it, so the
    // direct child (`sh`) stays alive while its child does the sleeping.
    let script = format!(
        "sleep 60 & echo $! > {pid} ; wait",
        pid = pid_file.display()
    );

    let mut cmd = linux_sandbox_command(&policy, "sh", &["-c", &script]).expect("build command");
    cmd.current_dir(&policy.workspace);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::null());
    let mut child = cmd.spawn().expect("spawn sandboxed command");
    let pid = child
        .id()
        .expect("child should have a pid right after spawn") as libc::pid_t;

    // Give the grandchild a moment to start and record its pid.
    let grandchild_pid: libc::pid_t = loop {
        if let Ok(contents) = std::fs::read_to_string(&pid_file)
            && let Ok(parsed) = contents.trim().parse()
        {
            break parsed;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    // `pre_exec` called `setsid()`, making `sh` (pid == pgid) the leader of
    // its own process group; `sleep`, as its child, inherited that pgid. So
    // killing the *group* by `sh`'s pid must also reach `sleep`.
    // SAFETY: `pid` is a process this test just spawned and still owns.
    let rc = unsafe { libc::killpg(pid, libc::SIGKILL) };
    assert_eq!(rc, 0, "killpg failed: {}", std::io::Error::last_os_error());

    let _ = child.wait().await;
    // Give the kernel a moment to reap/deliver the signal to the grandchild.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // SAFETY: `kill(pid, 0)` only probes for existence; it sends no signal.
    let grandchild_alive = unsafe { libc::kill(grandchild_pid, 0) } == 0;
    assert!(
        !grandchild_alive,
        "grandchild {grandchild_pid} survived killpg({pid})"
    );
}

// ---------------------------------------------------------------------------
// Process attributes observed from inside the sandbox
// ---------------------------------------------------------------------------

#[tokio::test]
async fn child_proc_status_reports_no_new_privs_and_seccomp_filter_active() {
    if skip_if_unavailable() {
        return;
    }
    let ws = TempWorkspace::new();
    let policy = workspace_write_policy(ws.path());

    let output = run(&policy, "cat", &["/proc/self/status"]).await;

    assert!(output.status.success(), "stderr: {}", stderr_of(&output));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.lines().any(|line| line == "NoNewPrivs:\t1"),
        "missing NoNewPrivs:\\t1 in:\n{stdout}"
    );
    // 2 == SECCOMP_MODE_FILTER.
    assert!(
        stdout.lines().any(|line| line == "Seccomp:\t2"),
        "missing Seccomp:\\t2 in:\n{stdout}"
    );
}
