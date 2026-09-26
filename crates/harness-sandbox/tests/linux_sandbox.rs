//! Integration tests for the Linux sandbox backend.
//!
//! These only compile — and only run — on Linux, and only on the two
//! architectures the sandbox itself supports (the `cfg` below strips the
//! whole file to nothing otherwise, notably on the macOS host this crate
//! was authored on, and matches the same gate on `mod linux` in `lib.rs`).
//! They are exercised by `cargo test --target {aarch64,x86_64}-unknown-linux-gnu`
//! in CI, never locally on this machine.
//!
//! Every test calls [`skip_if_unavailable`] (or [`new_workspace`], which
//! calls it) first and returns early rather than failing outright when a
//! precondition — Landlock ABI >= 3, `$HOME` being set, an external tool
//! being installed — is not met on the runner. Unless the environment
//! variable `HARNESS_REQUIRE_LINUX_SANDBOX=1` is set, in which case
//! [`skip_or_require`] panics instead: CI (Task 11) sets this so a runner
//! that is supposed to support the sandbox, but doesn't, fails the build
//! loudly instead of every dependent test quietly no-op'ing.

#![cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]

use std::ffi::OsStr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use harness_sandbox::{
    FsAccess, SandboxPolicy, SandboxSettings, detect, landlock_abi, linux_sandbox_available,
    linux_sandbox_command,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn unique_id() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

fn require_env_is_set() -> bool {
    std::env::var_os("HARNESS_REQUIRE_LINUX_SANDBOX").as_deref() == Some(OsStr::new("1"))
}

/// Prints `reason` to stderr and returns `true` (skip), unless
/// `HARNESS_REQUIRE_LINUX_SANDBOX=1` is set, in which case it panics
/// instead. See the module docs.
fn skip_or_require(reason: &str) -> bool {
    assert!(
        !require_env_is_set(),
        "{reason} (HARNESS_REQUIRE_LINUX_SANDBOX=1 is set)"
    );
    eprintln!("skipping: {reason}");
    true
}

/// Returns `true` when the current kernel/arch cannot run the Linux sandbox
/// at all, so callers can bail out early instead of failing a test that
/// Landlock/seccomp support was never a precondition of exercising.
fn skip_if_unavailable() -> bool {
    if linux_sandbox_available() {
        false
    } else {
        skip_or_require(&format!(
            "linux sandbox unavailable (landlock_abi = {:?})",
            landlock_abi()
        ))
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

/// `true` (skip) when `name` is not on `$PATH`.
fn require_command(name: &str) -> bool {
    if command_exists(name) {
        false
    } else {
        skip_or_require(&format!("{name} not installed"))
    }
}

/// Creates a fresh, empty directory under `$HOME` and removes it (best
/// effort) when the returned guard is dropped.
///
/// Deliberately rooted under `$HOME`, not `/tmp` (which earlier versions of
/// this file used, via `mktemp -d`): `/tmp` is unconditionally writable by
/// every sandboxed command regardless of `policy` (see
/// `linux::fs::writable_roots`), so a workspace located there would still
/// *look* writable even if the workspace-specific Landlock rule were broken
/// or missing entirely — silently masking exactly the bug the positive
/// `WorkspaceWrite` tests below exist to catch. A directory under `$HOME`
/// has no such fallback: it is writable only because the workspace rule
/// specifically grants it.
struct TempWorkspace(PathBuf);

impl TempWorkspace {
    /// `None` when `$HOME` is not set or the directory could not be
    /// created.
    fn try_new() -> Option<Self> {
        let home = std::env::var_os("HOME")?;
        let dir = PathBuf::from(home).join(format!(
            ".harness-sandbox-test-ws-{}-{}",
            std::process::id(),
            unique_id()
        ));
        std::fs::create_dir(&dir).ok()?;
        Some(Self(dir))
    }

    fn path(&self) -> &Path {
        &self.0
    }

    /// A path under the same `$HOME` this workspace lives in, but outside
    /// it and every other writable root — for negative tests.
    fn outside(&self, label: &str) -> PathBuf {
        self.0
            .parent()
            .expect("workspace has a parent ($HOME)")
            .join(format!(
                "proto-landlock-outside-{label}-{}-{}",
                std::process::id(),
                unique_id()
            ))
    }
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The fresh workspace tests should use, or `None` (having already
/// skipped/panicked) when the sandbox is unavailable or `$HOME` is not set.
/// Every test that needs a workspace starts with
/// `let Some(ws) = new_workspace() else { return; };`.
fn new_workspace() -> Option<TempWorkspace> {
    if skip_if_unavailable() {
        return None;
    }
    match TempWorkspace::try_new() {
        Some(ws) => Some(ws),
        None => {
            skip_or_require("$HOME not set (or the test workspace could not be created under it)");
            None
        }
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

/// A pty this test process allocates for itself via `posix_openpt`, so the
/// `/dev/pts/*` tests exercise a slave this process actually owns and can
/// verify (via the control check) is otherwise writable — rather than an
/// existing node that, on a hosted CI runner, either does not exist at all
/// or belongs to someone else's session, where a denied write proves
/// nothing about Landlock specifically (ordinary DAC permissions would deny
/// it either way).
struct Pty {
    // Kept open for the pty's lifetime: closing the master would make the
    // slave disappear.
    _master: OwnedFd,
    slave_path: PathBuf,
}

impl Pty {
    /// `None` (after printing why) when `posix_openpt` or a later step
    /// fails on this runner. Deliberately does not go through
    /// `skip_or_require`: a missing pty subsystem is a test-environment
    /// prerequisite, like a missing external tool, not a sandbox failure —
    /// so this must never panic even under `HARNESS_REQUIRE_LINUX_SANDBOX=1`.
    fn open() -> Option<Self> {
        // SAFETY: `posix_openpt` takes only an integer flag; no pointers.
        let master_fd = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
        if master_fd < 0 {
            eprintln!(
                "posix_openpt failed (test-environment prerequisite, not the sandbox): {}",
                std::io::Error::last_os_error()
            );
            return None;
        }
        // SAFETY: `master_fd` was just returned by `posix_openpt` above and
        // is a valid, open, owned fd from this point on.
        let master = unsafe { OwnedFd::from_raw_fd(master_fd) };

        // SAFETY: `grantpt`/`unlockpt` take only the fd; no pointers.
        if unsafe { libc::grantpt(master.as_raw_fd()) } != 0 {
            eprintln!(
                "grantpt failed (test-environment prerequisite, not the sandbox): {}",
                std::io::Error::last_os_error()
            );
            return None;
        }
        if unsafe { libc::unlockpt(master.as_raw_fd()) } != 0 {
            eprintln!(
                "unlockpt failed (test-environment prerequisite, not the sandbox): {}",
                std::io::Error::last_os_error()
            );
            return None;
        }

        let mut buf = [0u8; 64];
        // SAFETY: `buf` is a valid stack buffer of the given length;
        // `ptsname_r` writes at most `buf.len()` bytes, including the NUL.
        let rc = unsafe {
            libc::ptsname_r(
                master.as_raw_fd(),
                buf.as_mut_ptr().cast::<libc::c_char>(),
                buf.len(),
            )
        };
        if rc != 0 {
            eprintln!(
                "ptsname_r failed (test-environment prerequisite, not the sandbox): {}",
                std::io::Error::last_os_error()
            );
            return None;
        }
        let Some(nul) = buf.iter().position(|&b| b == 0) else {
            eprintln!(
                "ptsname_r returned a name with no NUL terminator in {} bytes",
                buf.len()
            );
            return None;
        };
        let slave_path = PathBuf::from(OsStr::from_bytes(&buf[..nul]));

        Some(Self {
            _master: master,
            slave_path,
        })
    }

    fn slave_path(&self) -> &Path {
        &self.slave_path
    }
}

// ---------------------------------------------------------------------------
// Detection
// ---------------------------------------------------------------------------

#[test]
fn detection_requires_landlock_abi_at_least_3() {
    if linux_sandbox_available() {
        let abi = landlock_abi().expect("linux_sandbox_available() implies landlock_abi() is Some");
        assert!(abi >= 3, "sandbox reported available at Landlock ABI {abi}");
    } else if require_env_is_set() {
        panic!(
            "HARNESS_REQUIRE_LINUX_SANDBOX=1 but linux_sandbox_available() is false (landlock_abi = {:?})",
            landlock_abi()
        );
    }

    if require_env_is_set() {
        assert!(
            detect(SandboxSettings::default()).is_some(),
            "HARNESS_REQUIRE_LINUX_SANDBOX=1 but detect() returned None"
        );
    }
}

// ---------------------------------------------------------------------------
// WorkspaceWrite: filesystem
// ---------------------------------------------------------------------------

#[tokio::test]
async fn workspace_write_allows_touch_inside_workspace() {
    let Some(ws) = new_workspace() else {
        return;
    };
    let policy = workspace_write_policy(ws.path());
    let target = ws.path().join("touched");

    let output = run(&policy, "touch", &[target.to_str().unwrap()]).await;

    assert!(output.status.success(), "stderr: {}", stderr_of(&output));
    assert!(target.exists());
}

#[tokio::test]
async fn workspace_write_denies_touch_outside_workspace_in_home() {
    let Some(ws) = new_workspace() else {
        return;
    };
    let policy = workspace_write_policy(ws.path());
    let target = ws.outside("denied-touch");

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
    let Some(ws) = new_workspace() else {
        return;
    };
    let policy = workspace_write_policy(ws.path());

    let output = run(&policy, "mktemp", &[]).await;

    assert!(output.status.success(), "stderr: {}", stderr_of(&output));
}

#[tokio::test]
async fn always_writable_devices_allow_redirect_to_dev_null() {
    let Some(ws) = new_workspace() else {
        return;
    };
    let policy = workspace_write_policy(ws.path());

    let output = run(&policy, "sh", &["-c", ": > /dev/null"]).await;

    assert!(output.status.success(), "stderr: {}", stderr_of(&output));
}

#[tokio::test]
async fn always_writable_devices_are_writable_even_under_read_only() {
    let Some(ws) = new_workspace() else {
        return;
    };
    let policy = read_only_policy(ws.path());

    let output = run(&policy, "sh", &["-c", ": > /dev/null"]).await;

    assert!(output.status.success(), "stderr: {}", stderr_of(&output));
}

#[tokio::test]
async fn workspace_write_allows_git_init_and_empty_commit() {
    let Some(ws) = new_workspace() else {
        return;
    };
    if require_command("git") {
        return;
    }
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
    let Some(ws) = new_workspace() else {
        return;
    };
    let policy = read_only_policy(ws.path());
    let target = ws.path().join("should-not-exist");

    let output = run(&policy, "touch", &[target.to_str().unwrap()]).await;

    assert!(!output.status.success());
    assert!(!target.exists());
}

// ---------------------------------------------------------------------------
// `/dev/shm` and `/dev/pts` (I3)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn workspace_write_allows_creating_a_file_under_dev_shm() {
    let Some(ws) = new_workspace() else {
        return;
    };
    let policy = workspace_write_policy(ws.path());
    let target = format!(
        "/dev/shm/harness-sandbox-test-{}-{}",
        std::process::id(),
        unique_id()
    );

    let output = run(&policy, "touch", &[&target]).await;

    assert!(output.status.success(), "stderr: {}", stderr_of(&output));
    let _ = std::fs::remove_file(&target);
}

#[tokio::test]
async fn read_only_denies_creating_a_file_under_dev_shm() {
    let Some(ws) = new_workspace() else {
        return;
    };
    let policy = read_only_policy(ws.path());
    let target = format!(
        "/dev/shm/harness-sandbox-test-{}-{}",
        std::process::id(),
        unique_id()
    );

    let output = run(&policy, "touch", &[&target]).await;

    assert!(!output.status.success());
    assert!(!Path::new(&target).exists());
}

#[tokio::test]
async fn workspace_write_denies_write_to_a_freshly_allocated_pty_slave() {
    let Some(ws) = new_workspace() else {
        return;
    };
    let Some(pty) = Pty::open() else {
        return;
    };
    // Control: the unsandboxed test process itself must be able to write to
    // its own pty slave, so a denial below can only be attributed to the
    // sandbox, not to ordinary DAC permissions.
    std::fs::OpenOptions::new()
        .write(true)
        .open(pty.slave_path())
        .expect("test process should be able to open its own pty slave for writing");
    let policy = workspace_write_policy(ws.path());

    let output = run(
        &policy,
        "sh",
        &["-c", &format!(": > {}", pty.slave_path().display())],
    )
    .await;

    assert!(!output.status.success());
    assert!(
        stderr_of(&output).contains("Permission denied"),
        "stderr: {}",
        stderr_of(&output)
    );
}

#[tokio::test]
async fn read_only_denies_write_to_a_freshly_allocated_pty_slave() {
    let Some(ws) = new_workspace() else {
        return;
    };
    let Some(pty) = Pty::open() else {
        return;
    };
    // Control: see the WorkspaceWrite variant above.
    std::fs::OpenOptions::new()
        .write(true)
        .open(pty.slave_path())
        .expect("test process should be able to open its own pty slave for writing");
    let policy = read_only_policy(ws.path());

    let output = run(
        &policy,
        "sh",
        &["-c", &format!(": > {}", pty.slave_path().display())],
    )
    .await;

    assert!(!output.status.success());
    assert!(
        stderr_of(&output).contains("Permission denied"),
        "stderr: {}",
        stderr_of(&output)
    );
}

// ---------------------------------------------------------------------------
// Truncation and metadata-preserving writes outside the workspace (C2, I5)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn workspace_write_denies_truncate_of_an_outside_file() {
    let Some(ws) = new_workspace() else {
        return;
    };
    let outside = ws.outside("truncate");
    std::fs::write(&outside, b"original content").expect("write outside file");
    let policy = workspace_write_policy(ws.path());

    let output = run(&policy, "truncate", &["-s0", outside.to_str().unwrap()]).await;

    assert!(!output.status.success(), "stderr: {}", stderr_of(&output));
    assert_eq!(
        std::fs::read(&outside).expect("read outside file"),
        b"original content"
    );
    let _ = std::fs::remove_file(&outside);
}

#[tokio::test]
async fn workspace_write_denies_append_to_an_outside_file() {
    let Some(ws) = new_workspace() else {
        return;
    };
    let outside = ws.outside("append");
    std::fs::write(&outside, b"original content").expect("write outside file");
    let policy = workspace_write_policy(ws.path());
    let script = format!("echo appended >> {}", outside.display());

    let output = run(&policy, "sh", &["-c", &script]).await;

    assert!(!output.status.success(), "stderr: {}", stderr_of(&output));
    assert_eq!(
        std::fs::read(&outside).expect("read outside file"),
        b"original content"
    );
    let _ = std::fs::remove_file(&outside);
}

/// The two tests above go through coreutils `truncate`/shell `>>`, both of
/// which open the file `O_WRONLY` (`truncate` internally, `>>` via
/// `O_WRONLY|O_APPEND`) — denied by `WRITE_FILE`, a right that has existed
/// since Landlock ABI 1, so those tests would also pass at ABI 2 and prove
/// nothing about the ABI-3 `TRUNCATE` right this fix round exists for. This
/// test instead exercises `TRUNCATE` specifically: `os.truncate(path, 0)`
/// calls `truncate(2)` directly on the path (no `open()` involved at all),
/// and `os.open(path, O_RDONLY | O_TRUNC)` opens read-only but still asks
/// the kernel to truncate — a request `WRITE_FILE` alone does not gate.
#[tokio::test]
async fn workspace_write_denies_truncate_right_on_an_outside_file() {
    let Some(ws) = new_workspace() else {
        return;
    };
    if require_command("python3") {
        return;
    }
    let outside = ws.outside("truncate-right");
    std::fs::write(&outside, b"original content").expect("write outside file");
    let policy = workspace_write_policy(ws.path());
    let outside_literal = format!("{outside:?}");
    let script = format!(
        r#"
import errno
import os
import sys

path = {outside_literal}

try:
    os.truncate(path, 0)
except OSError as exc:
    if exc.errno != errno.EPERM:
        print(f"os.truncate: expected EPERM, got {{exc.errno}}", file=sys.stderr)
        sys.exit(1)
else:
    print("os.truncate unexpectedly succeeded", file=sys.stderr)
    sys.exit(1)

try:
    fd = os.open(path, os.O_RDONLY | os.O_TRUNC)
except OSError as exc:
    if exc.errno != errno.EPERM:
        print(f"os.open(O_RDONLY|O_TRUNC): expected EPERM, got {{exc.errno}}", file=sys.stderr)
        sys.exit(1)
else:
    os.close(fd)
    print("os.open(O_RDONLY|O_TRUNC) unexpectedly succeeded", file=sys.stderr)
    sys.exit(1)

print("ok")
"#
    );

    let output = run(&policy, "python3", &["-c", &script]).await;

    assert!(output.status.success(), "stderr: {}", stderr_of(&output));
    assert_eq!(
        std::fs::read(&outside).expect("read outside file"),
        b"original content"
    );
    let _ = std::fs::remove_file(&outside);
}

// ---------------------------------------------------------------------------
// Network
// ---------------------------------------------------------------------------

#[tokio::test]
async fn network_denies_dev_tcp_redirect() {
    let Some(ws) = new_workspace() else {
        return;
    };
    if require_command("bash") {
        return;
    }
    let policy = workspace_write_policy(ws.path());

    let output = run(&policy, "bash", &["-c", "exec 3<>/dev/tcp/1.1.1.1/53"]).await;

    assert!(!output.status.success());
    assert!(
        stderr_of(&output)
            .to_lowercase()
            .contains("operation not permitted"),
        "expected a permission-denial message, got stderr: {}",
        stderr_of(&output)
    );
}

#[tokio::test]
async fn network_denies_socket_creation_for_non_af_unix_families_with_eperm() {
    let Some(ws) = new_workspace() else {
        return;
    };
    if require_command("python3") {
        return;
    }
    let policy = workspace_write_policy(ws.path());
    let script = r#"
import errno
import socket
import sys

cases = [
    (socket.AF_INET, socket.SOCK_STREAM, "AF_INET"),
    (socket.AF_INET6, socket.SOCK_STREAM, "AF_INET6"),
    (socket.AF_INET, socket.SOCK_DGRAM, "UDP"),
    (socket.AF_NETLINK, socket.SOCK_RAW, "AF_NETLINK"),
    (getattr(socket, "AF_PACKET", None), socket.SOCK_RAW, "AF_PACKET"),
]
for family, kind, label in cases:
    if family is None:
        print(f"{label}: not available in this python build, skipping", file=sys.stderr)
        continue
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
    let Some(ws) = new_workspace() else {
        return;
    };
    if require_command("python3") {
        return;
    }
    let policy = workspace_write_policy(ws.path());
    let script = "import socket; socket.socketpair(); print('ok')";

    let output = run(&policy, "python3", &["-c", script]).await;

    assert!(output.status.success(), "stderr: {}", stderr_of(&output));
}

#[tokio::test]
async fn network_denies_af_unix_connect_to_a_real_socket() {
    let Some(ws) = new_workspace() else {
        return;
    };
    if require_command("python3") {
        return;
    }
    let socket_path = ws.path().join("real.sock");
    // A genuinely listening AF_UNIX endpoint: `connect` is denied
    // unconditionally in `linux::seccomp` (regardless of socket domain), so
    // this must fail with EPERM even though something is actually there to
    // connect to — ruling out "it failed only because nothing was
    // listening" as an alternative explanation.
    let listener =
        std::os::unix::net::UnixListener::bind(&socket_path).expect("bind test AF_UNIX listener");
    let policy = workspace_write_policy(ws.path());
    // `{:?}` (not `{}`/`.display()`) so the path renders as a quoted,
    // escaped Python string literal.
    let socket_path_literal = format!("{socket_path:?}");
    let script = format!(
        r#"
import errno
import socket
import sys

s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
try:
    s.connect({socket_path_literal})
except OSError as exc:
    if exc.errno != errno.EPERM:
        print(f"expected EPERM, got {{exc.errno}}", file=sys.stderr)
        sys.exit(1)
    print("ok")
else:
    print("connect() unexpectedly succeeded", file=sys.stderr)
    sys.exit(1)
"#
    );

    let output = run(&policy, "python3", &["-c", &script]).await;

    assert!(output.status.success(), "stderr: {}", stderr_of(&output));
    drop(listener);
}

#[tokio::test]
async fn network_denies_io_uring_setup_with_eperm() {
    let Some(ws) = new_workspace() else {
        return;
    };
    if require_command("python3") {
        return;
    }
    let policy = workspace_write_policy(ws.path());
    // `io_uring_setup` is syscall 425 on both x86_64 and aarch64 (the
    // "asm-generic" numbering both architectures share for syscalls added
    // this recently). Seccomp runs in `syscall_enter_from_user_mode`,
    // before the kernel dispatches to the syscall's own implementation, so
    // `ENOSYS` is not an acceptable outcome here the way it might be for a
    // syscall the *kernel* doesn't implement: if our filter is doing its
    // job, this always returns `EPERM`, on every kernel that has `seccomp`
    // at all. `ENOSYS` would mean the filter never matched this syscall.
    let script = r#"
import ctypes
import errno
import sys

libc = ctypes.CDLL(None, use_errno=True)
rc = libc.syscall(425, 1, None)
if rc == -1:
    err = ctypes.get_errno()
    if err == errno.EPERM:
        print("ok")
    else:
        print(f"expected EPERM, got {err}", file=sys.stderr)
        sys.exit(1)
else:
    print("io_uring_setup unexpectedly succeeded", file=sys.stderr)
    sys.exit(1)
"#;

    let output = run(&policy, "python3", &["-c", script]).await;

    assert!(output.status.success(), "stderr: {}", stderr_of(&output));
}

#[cfg(target_arch = "x86_64")]
#[tokio::test]
async fn network_denies_x32_socket_with_eperm() {
    let Some(ws) = new_workspace() else {
        return;
    };
    if require_command("python3") {
        return;
    }
    let policy = workspace_write_policy(ws.path());
    // 0x4000_0029 = `__X32_SYSCALL_BIT` | 41, the x32 ABI's own number for
    // `socket` (native x86_64's `socket` is also 41; x32 mostly reuses
    // native numbers for syscalls like this one, just with the bit set).
    // See `linux::seccomp`'s module docs for why this needs its own filter
    // logic beyond the ordinary per-syscall rules. As in the io_uring test
    // above, `ENOSYS` is not accepted: seccomp runs before syscall
    // dispatch, so a working filter always returns `EPERM` here.
    let script = r#"
import ctypes
import errno
import sys

libc = ctypes.CDLL(None, use_errno=True)
rc = libc.syscall(0x40000029, 2, 1, 0)
if rc == -1:
    err = ctypes.get_errno()
    if err == errno.EPERM:
        print("ok")
    else:
        print(f"expected EPERM, got {err}", file=sys.stderr)
        sys.exit(1)
else:
    print("x32 socket() unexpectedly succeeded", file=sys.stderr)
    sys.exit(1)
"#;

    let output = run(&policy, "python3", &["-c", script]).await;

    assert!(output.status.success(), "stderr: {}", stderr_of(&output));
}

// ---------------------------------------------------------------------------
// Process hierarchy
// ---------------------------------------------------------------------------

#[tokio::test]
async fn grandchild_inherits_the_sandbox() {
    let Some(ws) = new_workspace() else {
        return;
    };
    let policy = workspace_write_policy(ws.path());
    let target = ws.outside("grandchild");
    let script = format!("sh -c 'touch {}'", target.display());

    let output = run(&policy, "sh", &["-c", &script]).await;

    assert!(!output.status.success());
    assert!(!target.exists());
}

#[tokio::test]
async fn parent_process_is_not_sandboxed_after_spawning() {
    let Some(ws) = new_workspace() else {
        return;
    };
    let policy = read_only_policy(ws.path());

    // A fully-restricted child runs and exits normally...
    let output = run(&policy, "true", &[]).await;
    assert!(output.status.success());

    // ...and this test process itself (the parent) was never restricted:
    // `pre_exec` only ever runs in the forked child, so the sandbox must
    // not have leaked back onto the caller.
    let home_tmp = ws.outside("parent-check");
    std::fs::write(&home_tmp, b"still unrestricted")
        .expect("parent process should still be able to write under $HOME");
    let _ = std::fs::remove_file(&home_tmp);

    let listener = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("parent process should still be able to bind a loopback socket");
    drop(listener);
}

#[tokio::test]
async fn killpg_of_child_pid_kills_background_grandchild() {
    let Some(ws) = new_workspace() else {
        return;
    };
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

    // Give the grandchild a moment to start and record its pid, but do not
    // wait forever: fail the test instead of hanging CI if it never shows
    // up.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let grandchild_pid: libc::pid_t = loop {
        if let Ok(contents) = std::fs::read_to_string(&pid_file)
            && let Ok(parsed) = contents.trim().parse()
        {
            break parsed;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "grandchild never wrote its pid file within 10s"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    // `pre_exec` called `setsid()`, making `sh` (pid == pgid) the leader of
    // its own process group; `sleep`, as its child, inherited that pgid. So
    // killing the *group* by `sh`'s pid must also reach `sleep`.
    // SAFETY: `pid` is a process this test just spawned and still owns.
    let rc = unsafe { libc::killpg(pid, libc::SIGKILL) };
    assert_eq!(rc, 0, "killpg failed: {}", std::io::Error::last_os_error());

    let _ = child.wait().await;
    // Give the kernel a moment to reap/deliver the signal to the
    // grandchild, again bounded rather than a fixed hope-it's-enough sleep.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        // SAFETY: `kill(pid, 0)` only probes for existence; it sends no
        // signal.
        if unsafe { libc::kill(grandchild_pid, 0) } != 0 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "grandchild {grandchild_pid} survived killpg({pid}) for over 10s"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ---------------------------------------------------------------------------
// Process attributes observed from inside the sandbox
// ---------------------------------------------------------------------------

#[tokio::test]
async fn child_proc_status_reports_no_new_privs_and_seccomp_filter_active() {
    let Some(ws) = new_workspace() else {
        return;
    };
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
