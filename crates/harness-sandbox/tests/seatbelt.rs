#![cfg(target_os = "macos")]
//! End-to-end tests against the real `/usr/bin/sandbox-exec` on this Mac.
//!
//! Each test builds its own temp workspace via `tempfile::tempdir()`. Every
//! test skips itself (with a note) when `HARNESS_SANDBOX` is set, since that
//! means these tests are themselves running nested inside a sandbox and
//! `sandbox-exec` would not behave the same way (or might not be reachable at
//! all).

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Instant;

use harness_core::tool::CommandSandbox;
use harness_sandbox::{
    FsAccess, SandboxPolicy, SandboxSettings, Seatbelt, looks_like_sandbox_denial, seatbelt_command,
};

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
    let made = PathBuf::from(text.lines().next().unwrap_or_default().trim());
    assert!(made.is_absolute(), "mktemp printed no path: {text}");
    std::fs::remove_file(&made).expect("remove the mktemp file");

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
    let status = host_git(&ws)
        .args(["init", "-q"])
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

// ===========================================================================
// Escape and metadata-protection regression tests (T1–T15).
//
// These run through `Seatbelt::command`, the entry point harness itself uses
// (it also puts the child in its own process group). Every file they create,
// link, rename or write lives in temp dirs the test creates; the only
// "outside the writable roots" location is a temp dir under Cargo's
// `CARGO_TARGET_TMPDIR`, or under `$HOME` when that one is writable (see
// `outside_tempdir`). Git only ever runs in scratch repos inside those temp
// dirs. Network tests only touch a listener the test opens on 127.0.0.1, or
// an unroutable address.
// ===========================================================================

/// Collects failed sub-checks so one run reports every hole a test finds,
/// not just the first.
#[derive(Default)]
struct Checks(Vec<String>);

impl Checks {
    fn check(&mut self, ok: bool, what: impl FnOnce() -> String) {
        if !ok {
            self.0.push(what());
        }
    }

    #[track_caller]
    fn finish(self) {
        assert!(
            self.0.is_empty(),
            "{} check(s) failed:\n  - {}",
            self.0.len(),
            self.0.join("\n  - ")
        );
    }
}

/// Git environment variables that would point git at another repo or inject
/// config (a git hook running `cargo test` sets some of them). Every git the
/// tests start, sandboxed or not, runs without them, so it can only find the
/// scratch repo it is run in.
const GIT_ENV_TO_CLEAR: [&str; 8] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
];

/// Runs `/bin/sh -c script` in `ws` through [`Seatbelt::command`] and returns
/// `(exit_code, combined_stdout_stderr)`. Git is isolated from the user's
/// global and system config.
async fn sh_in(
    settings: &SandboxSettings,
    access: FsAccess,
    ws: &Path,
    script: &str,
) -> (Option<i32>, String) {
    let sandbox = Seatbelt::new(settings.clone());
    let mut cmd = sandbox
        .command(access, ws, "/bin/sh", &["-c", script])
        .expect("build seatbelt command");
    for var in GIT_ENV_TO_CLEAR {
        cmd.env_remove(var);
    }
    cmd.current_dir(ws)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let output = cmd.output().await.expect("spawn sandboxed command");
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    (output.status.code(), text)
}

/// [`sh_in`] with default settings and [`FsAccess::WorkspaceWrite`].
async fn ws_sh(ws: &Path, script: &str) -> (Option<i32>, String) {
    sh_in(
        &SandboxSettings::default(),
        FsAccess::WorkspaceWrite,
        ws,
        script,
    )
    .await
}

/// `sandbox-exec`'s own exit codes when it cannot compile the profile (65,
/// EX_DATAERR) or cannot exec the command (71, EX_OSERR).
const SANDBOX_EXEC_FAILURES: [i32; 2] = [65, 71];

/// Whether the sandbox refused a command: it failed, and not because
/// `sandbox-exec` itself failed to start it. Counting 65 or 71 as a denial
/// would let a broken profile pass every deny-only check.
fn denied(code: Option<i32>) -> bool {
    code != Some(0) && !code.is_some_and(|c| SANDBOX_EXEC_FAILURES.contains(&c))
}

#[test]
fn sandbox_exec_startup_failures_are_not_counted_as_denials() {
    if skip_if_nested("sandbox_exec_startup_failures_are_not_counted_as_denials") {
        return;
    }
    for (label, profile) in [
        (
            "profile that does not compile",
            "(version 1)(no-such-operation)",
        ),
        ("profile that refuses the exec", "(version 1)(deny default)"),
    ] {
        let out = std::process::Command::new("/usr/bin/sandbox-exec")
            .args(["-p", profile, "/usr/bin/true"])
            .output()
            .expect("run sandbox-exec");
        let code = out.status.code();
        assert!(
            code.is_some_and(|c| SANDBOX_EXEC_FAILURES.contains(&c)) && !denied(code),
            "{label}: sandbox-exec exited {code:?}, expected one of {SANDBOX_EXEC_FAILURES:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// Runs `script` in `ws` (workspace-write) and records a failure unless the
/// sandbox refuses it (see [`denied`]) AND `intact()` holds afterwards.
async fn expect_denied(
    c: &mut Checks,
    ws: &Path,
    label: &str,
    script: &str,
    intact: impl FnOnce() -> bool,
) {
    let (code, out) = ws_sh(ws, script).await;
    let intact = intact();
    c.check(denied(code) && intact, || {
        format!(
            "{label}: `{script}` should be denied: exit={code:?} intact={intact} output={:?}",
            out.trim()
        )
    });
}

/// Runs `script` in `ws` (workspace-write) and records a failure unless it
/// exits zero.
async fn expect_allowed(c: &mut Checks, ws: &Path, label: &str, script: &str) {
    let (code, out) = ws_sh(ws, script).await;
    c.check(code == Some(0), || {
        format!(
            "{label}: `{script}` should succeed: exit={code:?} output={:?}",
            out.trim()
        )
    });
}

/// An unsandboxed `git` in `dir`, isolated from the user's config and from
/// any repo but the one `dir` is in (see [`GIT_ENV_TO_CLEAR`]).
fn host_git(dir: &Path) -> std::process::Command {
    let mut cmd = std::process::Command::new("git");
    for var in GIT_ENV_TO_CLEAR {
        cmd.env_remove(var);
    }
    cmd.current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(Stdio::null());
    cmd
}

/// Runs git outside the sandbox (test setup), isolated from the user's config.
fn git(dir: &Path, args: &[&str]) {
    let out = host_git(dir)
        .args([
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `git init` plus one empty commit in `dir` (outside the sandbox).
fn git_repo(dir: &Path) {
    std::fs::create_dir_all(dir).expect("create repo dir");
    git(dir, &["init", "-q"]);
    git(dir, &["commit", "-q", "--allow-empty", "-m", "init"]);
}

/// A new temp dir under `base` (hidden, so a leftover in `$HOME` stays out of
/// sight) and whether it qualifies as "outside": on the same volume as `ws`
/// (hard links cannot cross volumes), and a sandboxed write there is denied.
/// Returns `(dir, canonical path, same_volume, write_denied, details)`.
async fn outside_candidate(
    ws: &Path,
    base: &Path,
) -> (tempfile::TempDir, PathBuf, bool, bool, String) {
    let dir = tempfile::Builder::new()
        .prefix(".harness-sandbox-test-")
        .tempdir_in(base)
        .unwrap_or_else(|e| panic!("create a temp dir in {base:?}: {e}"));
    let canon = dir.path().canonicalize().expect("canonicalize outside");
    let same_volume =
        std::fs::metadata(&canon).unwrap().dev() == std::fs::metadata(ws).unwrap().dev();
    let probe = canon.join("probe");
    let (code, out) = ws_sh(ws, &format!("touch '{}'", probe.display())).await;
    let write_denied = denied(code) && !probe.exists();
    let details = format!(
        "{canon:?}: same volume as {ws:?}: {same_volume}; sandboxed `touch` exit={code:?} \
         output={:?}",
        out.trim()
    );
    (dir, canon, same_volume, write_denied, details)
}

/// A temp dir OUTSIDE every writable root. The first choice is under
/// `CARGO_TARGET_TMPDIR` (`target/tmp`), which holds no user files. That is
/// itself writable when the target dir lies under a writable root (a target
/// dir under `/tmp`, say), so the fallback is a temp dir under `$HOME`, the
/// location the older tests use. Panics unless the chosen dir is on the same
/// volume as `ws` and a sandboxed write there is denied.
async fn outside_tempdir(ws: &Path) -> (tempfile::TempDir, PathBuf) {
    let target_tmp = Path::new(env!("CARGO_TARGET_TMPDIR"));
    let (dir, canon, same_volume, write_denied, details) = outside_candidate(ws, target_tmp).await;
    if same_volume && write_denied {
        return (dir, canon);
    }
    drop(dir);
    let home = PathBuf::from(std::env::var_os("HOME").expect("HOME must be set"));
    let (dir, canon, same_volume, write_denied, home_details) = outside_candidate(ws, &home).await;
    assert!(
        same_volume,
        "the $HOME fallback is on a different volume from the workspace (target/tmp was \
         rejected too):\n  {details}\n  {home_details}"
    );
    assert!(
        write_denied,
        "the $HOME fallback is writable from the sandbox, so this test would mean nothing \
         (target/tmp was rejected too):\n  {details}\n  {home_details}"
    );
    (dir, canon)
}

fn read(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// T1. case (and APFS Unicode-fold) variants of protected names
// ---------------------------------------------------------------------------

#[tokio::test]
async fn t01_case_variants_of_protected_names_are_denied() {
    if skip_if_nested("t01_case_variants_of_protected_names_are_denied") {
        return;
    }
    let mut c = Checks::default();

    // A workspace that already has `.git` and `.harness/`.
    let (_d1, ws) = canonical_tempdir();
    git_repo(&ws);
    std::fs::create_dir(ws.join(".harness")).unwrap();
    let config = ws.join(".git/config");
    let config_before = read(&config);
    let config_intact = || read(&config) == config_before;

    expect_denied(
        &mut c,
        &ws,
        ".GIT/config",
        "echo '[x]' >> .GIT/config",
        &config_intact,
    )
    .await;
    expect_denied(
        &mut c,
        &ws,
        ".git/CONFIG",
        "echo '[x]' >> .git/CONFIG",
        &config_intact,
    )
    .await;
    expect_denied(
        &mut c,
        &ws,
        ".Git/hooks/pre-commit",
        "touch .Git/hooks/pre-commit",
        || !ws.join(".git/hooks/pre-commit").exists(),
    )
    .await;
    for variant in [".HARNESS", ".Harness", ".harne\u{17F}s"] {
        expect_denied(
            &mut c,
            &ws,
            &format!("{variant}/x"),
            &format!("touch '{variant}/x'"),
            || !ws.join(".harness/x").exists(),
        )
        .await;
    }

    // Top-level HEAD, first when it does not exist ...
    for variant in ["head", "Head", "HEAD"] {
        expect_denied(
            &mut c,
            &ws,
            &format!("{variant} (no HEAD yet)"),
            &format!("touch {variant}"),
            || !ws.join(variant).exists(),
        )
        .await;
        let _ = std::fs::remove_file(ws.join(variant));
    }
    // ... then when it does.
    std::fs::write(ws.join("HEAD"), "x\n").unwrap();
    for variant in ["head", "Head"] {
        expect_denied(
            &mut c,
            &ws,
            &format!("{variant} (HEAD exists)"),
            &format!("echo pwned > {variant}"),
            || read(&ws.join("HEAD")) == b"x\n",
        )
        .await;
    }

    // A repo whose `.git` has no `hooks/` or `modules/` yet: re-creating them
    // under a case variant must be denied too (git would use them).
    let (_d2, ws) = canonical_tempdir();
    git_repo(&ws);
    std::fs::remove_dir_all(ws.join(".git/hooks")).unwrap();
    for variant in ["HOOKS", "hoo\u{212A}s"] {
        let dir = ws.join(".git").join(variant);
        expect_denied(
            &mut c,
            &ws,
            &format!(".git/{variant}/pre-commit"),
            &format!("mkdir '.git/{variant}' && touch '.git/{variant}/pre-commit'"),
            || !dir.join("pre-commit").exists(),
        )
        .await;
        let _ = std::fs::remove_dir_all(&dir);
    }
    for variant in ["MODULES", "module\u{17F}"] {
        let dir = ws.join(".git").join(variant);
        expect_denied(
            &mut c,
            &ws,
            &format!(".git/{variant}/m/config"),
            &format!("mkdir -p '.git/{variant}/m' && echo '[x]' > '.git/{variant}/m/config'"),
            || !dir.join("m/config").exists(),
        )
        .await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    // A workspace with no `.git` at all.
    let (_d3, ws) = canonical_tempdir();
    expect_denied(
        &mut c,
        &ws,
        ".GIT/hooks/x (no .git)",
        "mkdir -p .GIT/hooks && touch .GIT/hooks/x",
        || !ws.join(".GIT").exists(),
    )
    .await;

    c.finish();
}

// ---------------------------------------------------------------------------
// T2. nested `.git` directories cannot be replaced, removed or modified
// ---------------------------------------------------------------------------

#[tokio::test]
async fn t02_nested_git_dirs_cannot_be_replaced_or_modified() {
    if skip_if_nested("t02_nested_git_dirs_cannot_be_replaced_or_modified") {
        return;
    }
    let mut c = Checks::default();
    let (_d, ws) = canonical_tempdir();
    for sub in ["mv", "rm", "ln", "w"] {
        git_repo(&ws.join(sub));
    }
    let is_real_dir = |p: &Path| {
        std::fs::symlink_metadata(p)
            .map(|m| m.file_type().is_dir())
            .unwrap_or(false)
    };

    expect_denied(
        &mut c,
        &ws,
        "rename sub/.git",
        "mv mv/.git mv/.git.bak",
        || is_real_dir(&ws.join("mv/.git")) && !ws.join("mv/.git.bak").exists(),
    )
    .await;

    expect_denied(&mut c, &ws, "rm -rf sub/.git", "rm -rf rm/.git", || {
        is_real_dir(&ws.join("rm/.git")) && ws.join("rm/.git/config").exists()
    })
    .await;

    expect_denied(
        &mut c,
        &ws,
        "replace sub/.git with a symlink",
        "mv ln/.git ln/moved && ln -s moved ln/.git",
        || is_real_dir(&ws.join("ln/.git")),
    )
    .await;

    let config = ws.join("w/.git/config");
    let config_before = read(&config);
    expect_denied(
        &mut c,
        &ws,
        "sub/.git/config",
        "echo '[x]' >> w/.git/config",
        || read(&config) == config_before,
    )
    .await;
    expect_denied(
        &mut c,
        &ws,
        "sub/.git/hooks/x",
        "touch w/.git/hooks/x",
        || !ws.join("w/.git/hooks/x").exists(),
    )
    .await;

    // Planting a new `.git` (dir, gitfile or symlink) anywhere in the workspace.
    expect_denied(&mut c, &ws, "plant dir/.git", "mkdir -p p1/.git", || {
        !ws.join("p1/.git").exists()
    })
    .await;
    expect_denied(
        &mut c,
        &ws,
        "plant dir/.git gitfile",
        "mkdir -p p2 && printf 'gitdir: /tmp/x\\n' > p2/.git",
        || !ws.join("p2/.git").exists(),
    )
    .await;
    expect_denied(
        &mut c,
        &ws,
        "plant dir/.git symlink",
        "mkdir -p p3 && ln -s ../w/.git p3/.git",
        || std::fs::symlink_metadata(ws.join("p3/.git")).is_err(),
    )
    .await;
    expect_denied(&mut c, &ws, "plant dir/.GIT", "mkdir -p p4/.GIT", || {
        std::fs::symlink_metadata(ws.join("p4/.GIT")).is_err()
    })
    .await;
    expect_denied(
        &mut c,
        &ws,
        "plant dir/.Git gitfile",
        "mkdir -p p5 && printf 'gitdir: /tmp/x\\n' > p5/.Git",
        || std::fs::symlink_metadata(ws.join("p5/.Git")).is_err(),
    )
    .await;

    c.finish();
}

// ---------------------------------------------------------------------------
// T3. `.harness/`
// ---------------------------------------------------------------------------

#[tokio::test]
async fn t03_harness_dir_is_protected() {
    if skip_if_nested("t03_harness_dir_is_protected") {
        return;
    }
    let mut c = Checks::default();

    let (_d1, ws) = canonical_tempdir();
    std::fs::create_dir(ws.join(".harness")).unwrap();
    std::fs::write(ws.join(".harness/settings.toml"), "a = 1\n").unwrap();
    expect_denied(&mut c, &ws, "write .harness/x", "touch .harness/x", || {
        !ws.join(".harness/x").exists()
    })
    .await;
    expect_denied(
        &mut c,
        &ws,
        "overwrite .harness/settings.toml",
        "echo 'a = 2' > .harness/settings.toml",
        || read(&ws.join(".harness/settings.toml")) == b"a = 1\n",
    )
    .await;
    expect_denied(
        &mut c,
        &ws,
        "rename .harness away",
        "mv .harness h2",
        || ws.join(".harness/settings.toml").exists(),
    )
    .await;

    let (_d2, ws) = canonical_tempdir();
    for variant in [".harness", ".Harness"] {
        expect_denied(
            &mut c,
            &ws,
            &format!("mkdir {variant}"),
            &format!("mkdir {variant}"),
            || !ws.join(variant).exists(),
        )
        .await;
        let _ = std::fs::remove_dir_all(ws.join(variant));
    }
    expect_denied(
        &mut c,
        &ws,
        "rename a dir to .harness",
        "mkdir d && touch d/settings.toml && mv d .harness",
        || !ws.join(".harness").exists(),
    )
    .await;
    let _ = std::fs::remove_dir_all(ws.join(".harness"));
    expect_denied(&mut c, &ws, "symlink .harness", "ln -s d .harness", || {
        std::fs::symlink_metadata(ws.join(".harness")).is_err()
    })
    .await;

    c.finish();
}

// ---------------------------------------------------------------------------
// T4. everyday git still works
// ---------------------------------------------------------------------------

#[tokio::test]
async fn t04_git_checkout_switch_and_commit_still_work() {
    if skip_if_nested("t04_git_checkout_switch_and_commit_still_work") {
        return;
    }
    let mut c = Checks::default();
    let (_d, ws) = canonical_tempdir();
    git_repo(&ws);
    // The same steps in a nested repo, which the `.git` rules also cover.
    git_repo(&ws.join("nested"));
    const GIT: &str = "git -c user.email=t@t -c user.name=t";

    for dir in [".", "nested"] {
        for (label, step) in [
            ("checkout -b", format!("{GIT} checkout -q -b b")),
            ("checkout -", format!("{GIT} checkout -q -")),
            ("switch", format!("{GIT} switch -q b")),
            (
                "commit after checkout",
                format!("echo x > f && {GIT} add f && {GIT} commit -q -m on-b"),
            ),
            ("checkout main", format!("{GIT} checkout -q main")),
            (
                "commit after checkout main",
                format!("{GIT} commit -q --allow-empty -m on-main"),
            ),
            ("switch -c", format!("{GIT} switch -q -c c")),
            ("status", format!("{GIT} status --short")),
        ] {
            expect_allowed(
                &mut c,
                &ws,
                &format!("[{dir}] {label}"),
                &format!("cd {dir} && {step}"),
            )
            .await;
        }
    }

    c.finish();
}

// ---------------------------------------------------------------------------
// T5. `.git/modules/*/{config,hooks}`
// ---------------------------------------------------------------------------

#[tokio::test]
async fn t05_git_modules_config_and_hooks_are_denied() {
    if skip_if_nested("t05_git_modules_config_and_hooks_are_denied") {
        return;
    }
    let mut c = Checks::default();
    let (_d, ws) = canonical_tempdir();
    git_repo(&ws);
    std::fs::create_dir_all(ws.join(".git/modules/m/hooks")).unwrap();
    std::fs::write(ws.join(".git/modules/m/config"), "[core]\n").unwrap();

    expect_denied(
        &mut c,
        &ws,
        ".git/modules/m/config",
        "echo '[x]' >> .git/modules/m/config",
        || read(&ws.join(".git/modules/m/config")) == b"[core]\n",
    )
    .await;
    expect_denied(
        &mut c,
        &ws,
        ".git/modules/m/hooks/x",
        "touch .git/modules/m/hooks/x",
        || !ws.join(".git/modules/m/hooks/x").exists(),
    )
    .await;
    expect_denied(
        &mut c,
        &ws,
        "new .git/modules/n/config",
        "mkdir -p .git/modules/n && echo '[x]' > .git/modules/n/config",
        || !ws.join(".git/modules/n/config").exists(),
    )
    .await;
    expect_denied(
        &mut c,
        &ws,
        "nested .git/modules/a/modules/b/hooks/x",
        "mkdir -p .git/modules/a/modules/b && mkdir .git/modules/a/modules/b/hooks",
        || !ws.join(".git/modules/a/modules/b/hooks").exists(),
    )
    .await;

    c.finish();
}

// ---------------------------------------------------------------------------
// T6. extra writable roots
// ---------------------------------------------------------------------------

#[tokio::test]
async fn t06_extra_writable_root_is_writable_but_its_sibling_is_not() {
    if skip_if_nested("t06_extra_writable_root_is_writable_but_its_sibling_is_not") {
        return;
    }
    let mut c = Checks::default();
    let (_d, ws) = canonical_tempdir();
    let (_o, outside) = outside_tempdir(&ws).await;
    let extra = outside.join("extra");
    let sibling = outside.join("extra-sibling");
    std::fs::create_dir(&extra).unwrap();
    std::fs::create_dir(&sibling).unwrap();
    let settings = SandboxSettings {
        extra_writable: vec![extra.clone()],
        allow_localhost: false,
    };

    let (code, out) = sh_in(
        &settings,
        FsAccess::WorkspaceWrite,
        &ws,
        &format!("touch '{}/ok'", extra.display()),
    )
    .await;
    c.check(code == Some(0) && extra.join("ok").exists(), || {
        format!("write in the extra root should succeed: exit={code:?} output={out:?}")
    });

    for target in [sibling.join("no"), outside.join("no")] {
        let (code, out) = sh_in(
            &settings,
            FsAccess::WorkspaceWrite,
            &ws,
            &format!("touch '{}'", target.display()),
        )
        .await;
        c.check(denied(code) && !target.exists(), || {
            format!("write to {target:?} should be denied: exit={code:?} output={out:?}")
        });
    }

    c.finish();
}

// ---------------------------------------------------------------------------
// T7. `/tmp` and `/var/tmp` spellings
// ---------------------------------------------------------------------------

#[tokio::test]
async fn t07_tmp_is_writable_under_every_spelling() {
    if skip_if_nested("t07_tmp_is_writable_under_every_spelling") {
        return;
    }
    let mut c = Checks::default();
    let (_d, ws) = canonical_tempdir();
    let tmp = tempfile::tempdir_in("/private/tmp").unwrap();
    let var_tmp = tempfile::tempdir_in("/private/var/tmp").unwrap();
    let tmp_name = tmp.path().file_name().unwrap().to_str().unwrap();
    let var_tmp_name = var_tmp.path().file_name().unwrap().to_str().unwrap();

    for (spelling, real) in [
        (format!("/tmp/{tmp_name}"), tmp.path()),
        (format!("/private/tmp/{tmp_name}"), tmp.path()),
        (format!("/var/tmp/{var_tmp_name}"), var_tmp.path()),
        (format!("/private/var/tmp/{var_tmp_name}"), var_tmp.path()),
    ] {
        let leaf = spelling.replace('/', "_");
        let script = format!("mkdir '{spelling}/{leaf}' && echo ok > '{spelling}/{leaf}/f'");
        let (code, out) = ws_sh(&ws, &script).await;
        c.check(
            code == Some(0) && real.join(&leaf).join("f").exists(),
            || format!("writing via {spelling} should succeed: exit={code:?} output={out:?}"),
        );
    }

    c.finish();
}

// ---------------------------------------------------------------------------
// T8. hard links to files outside the writable roots
// ---------------------------------------------------------------------------

#[tokio::test]
async fn t08a_hard_links_to_outside_or_protected_files_cannot_be_created() {
    if skip_if_nested("t08a_hard_links_to_outside_or_protected_files_cannot_be_created") {
        return;
    }
    let mut c = Checks::default();
    let (_d, ws) = canonical_tempdir();
    git_repo(&ws);
    let (_o, outside) = outside_tempdir(&ws).await;
    let target = outside.join("f");
    std::fs::write(&target, "original\n").unwrap();

    let (code, out) = ws_sh(&ws, &format!("ln '{}' f", target.display())).await;
    c.check(denied(code) && !ws.join("f").exists(), || {
        format!("ln <outside>/f ws/f should be denied: exit={code:?} output={out:?}")
    });
    let _ = ws_sh(&ws, "echo pwned > f").await;
    c.check(
        read(&target) == b"original\n" && std::fs::metadata(&target).unwrap().nlink() == 1,
        || "the outside file changed or gained a link".to_string(),
    );

    // The same trick against a protected file inside the workspace.
    let config = ws.join(".git/config");
    let config_before = read(&config);
    expect_denied(
        &mut c,
        &ws,
        "hard link to .git/config",
        "ln .git/config cfg && echo '[x]' >> cfg",
        || read(&config) == config_before && !ws.join("cfg").exists(),
    )
    .await;

    c.finish();
}

#[tokio::test]
async fn t08b_preexisting_hard_link_to_outside_file_is_not_writable() {
    if skip_if_nested("t08b_preexisting_hard_link_to_outside_file_is_not_writable") {
        return;
    }
    let mut c = Checks::default();
    let (_d, ws) = canonical_tempdir();
    let (_o, outside) = outside_tempdir(&ws).await;

    /// Everything a write through another name could change on the shared inode.
    #[derive(Debug, PartialEq)]
    struct Inode {
        data: Vec<u8>,
        mode: u32,
        flags: u32,
        mtime: i64,
        xattrs: String,
    }
    let inode = |p: &Path| {
        use std::os::macos::fs::MetadataExt as _;
        let meta = std::fs::metadata(p).unwrap();
        let xattrs = std::process::Command::new("/usr/bin/xattr")
            .arg(p)
            .output()
            .unwrap();
        Inode {
            data: read(p),
            mode: meta.mode() & 0o7777,
            flags: meta.st_flags(),
            mtime: meta.mtime(),
            xattrs: String::from_utf8_lossy(&xattrs.stdout).into_owned(),
        }
    };

    for (i, (label, script)) in [
        ("overwrite", "echo pwned > g{i}"),
        ("append", "echo more >> g{i}"),
        (
            "truncate",
            "python3 -c \"import os; os.truncate('g{i}', 0)\"",
        ),
        ("chmod", "chmod 600 g{i}"),
        ("xattr", "xattr -w com.example.k v g{i}"),
        ("chflags", "chflags hidden g{i}"),
        ("utimes", "touch -t 200001010000 g{i}"),
    ]
    .into_iter()
    .enumerate()
    {
        // A fresh outside file and a hard link to it, created by the test,
        // unsandboxed, before the sandboxed command runs.
        let target = outside.join(format!("g{i}"));
        std::fs::write(&target, "original\n").unwrap();
        std::fs::hard_link(&target, ws.join(format!("g{i}"))).unwrap();
        let before = inode(&target);
        let script = script.replace("{i}", &i.to_string());
        let (code, out) = ws_sh(&ws, &script).await;
        let after = inode(&target);
        c.check(denied(code) && after == before, || {
            format!(
                "{label} through a pre-existing hard link: `{script}` exit={code:?} \
                 output={:?}\n      before={before:?}\n      after ={after:?}",
                out.trim()
            )
        });
    }

    // Removing the extra name is harmless and must keep working.
    let target = outside.join("r");
    std::fs::write(&target, "original\n").unwrap();
    std::fs::hard_link(&target, ws.join("r")).unwrap();
    expect_allowed(&mut c, &ws, "rm the hard link", "rm r").await;
    c.check(
        read(&target) == b"original\n" && std::fs::metadata(&target).unwrap().nlink() == 1,
        || "rm of the hard link disturbed the outside file".into(),
    );

    c.finish();
}

// ---------------------------------------------------------------------------
// T9. renames across the workspace boundary
// ---------------------------------------------------------------------------

#[tokio::test]
async fn t09_renames_across_the_workspace_boundary_are_denied() {
    if skip_if_nested("t09_renames_across_the_workspace_boundary_are_denied") {
        return;
    }
    let mut c = Checks::default();
    let (_d, ws) = canonical_tempdir();
    let (_o, outside) = outside_tempdir(&ws).await;
    std::fs::write(ws.join("f"), "inside\n").unwrap();
    std::fs::create_dir(ws.join("d")).unwrap();
    std::fs::write(outside.join("g"), "outside\n").unwrap();

    expect_denied(
        &mut c,
        &ws,
        "mv ws/f <outside>/f",
        &format!("mv f '{}/f'", outside.display()),
        || ws.join("f").exists() && !outside.join("f").exists(),
    )
    .await;
    expect_denied(
        &mut c,
        &ws,
        "mv ws/d <outside>/d",
        &format!("mv d '{}/d'", outside.display()),
        || ws.join("d").is_dir() && !outside.join("d").exists(),
    )
    .await;
    expect_denied(
        &mut c,
        &ws,
        "mv <outside>/g ws/g",
        &format!("mv '{}/g' g", outside.display()),
        || read(&outside.join("g")) == b"outside\n" && !ws.join("g").exists(),
    )
    .await;

    c.finish();
}

// ---------------------------------------------------------------------------
// T10. outbound connections to localhost
// ---------------------------------------------------------------------------

/// Python that connects to `host:port` and prints `CONNECTED` (exit 0) or
/// `ERRNO <n>` (exit 1).
fn connect_script(host: &str, port: u16) -> String {
    format!(
        "python3 -c \"
import socket, sys
s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
s.settimeout(2)
try:
    s.connect(('{host}', {port}))
    print('CONNECTED')
except OSError as e:
    print('ERRNO', e.errno, e)
    sys.exit(1)
\""
    )
}

#[tokio::test]
async fn t10_outbound_localhost_follows_allow_localhost() {
    if skip_if_nested("t10_outbound_localhost_follows_allow_localhost") {
        return;
    }
    let mut c = Checks::default();
    let (_d, ws) = canonical_tempdir();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
    let port = listener.local_addr().unwrap().port();

    let denied = SandboxSettings::default();
    let (code, out) = sh_in(
        &denied,
        FsAccess::WorkspaceWrite,
        &ws,
        &connect_script("127.0.0.1", port),
    )
    .await;
    c.check(code != Some(0) && out.contains("ERRNO 1 "), || {
        format!("connect 127.0.0.1 without allow_localhost should fail with EPERM: exit={code:?} output={out:?}")
    });

    let allowed = SandboxSettings {
        allow_localhost: true,
        ..SandboxSettings::default()
    };
    let (code, out) = sh_in(
        &allowed,
        FsAccess::WorkspaceWrite,
        &ws,
        &connect_script("127.0.0.1", port),
    )
    .await;
    c.check(code == Some(0) && out.contains("CONNECTED"), || {
        format!(
            "connect 127.0.0.1 with allow_localhost should succeed: exit={code:?} output={out:?}"
        )
    });

    // allow_localhost must not open anything else: an unroutable address is
    // refused by the sandbox (EPERM) rather than timing out.
    let (code, out) = sh_in(
        &allowed,
        FsAccess::WorkspaceWrite,
        &ws,
        &connect_script("10.255.255.1", 9),
    )
    .await;
    c.check(code != Some(0) && out.contains("ERRNO 1 "), || {
        format!("connect 10.255.255.1 with allow_localhost should fail with EPERM: exit={code:?} output={out:?}")
    });

    drop(listener);
    c.finish();
}

// ---------------------------------------------------------------------------
// T11. process group
// ---------------------------------------------------------------------------

#[tokio::test]
async fn t11_sandboxed_command_leads_its_own_process_group() {
    if skip_if_nested("t11_sandboxed_command_leads_its_own_process_group") {
        return;
    }
    let (_d, ws) = canonical_tempdir();
    let sandbox = Seatbelt::new(SandboxSettings::default());
    let mut cmd = sandbox
        .command(
            FsAccess::WorkspaceWrite,
            &ws,
            "/bin/sh",
            // `/bin/ps` is setuid, and Seatbelt refuses to exec setuid binaries, so ask
            // the process itself. `exec` keeps the spawned pid.
            &[
                "-c",
                "exec python3 -c 'import os; print(\"pid\", os.getpid()); print(\"pgid\", os.getpgrp())'",
            ],
        )
        .expect("build seatbelt command");
    cmd.current_dir(&ws)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = cmd.spawn().expect("spawn");
    let spawned_pid = child.id().expect("child pid");
    let output = child.wait_with_output().await.expect("wait");
    let text = String::from_utf8_lossy(&output.stdout).into_owned()
        + &String::from_utf8_lossy(&output.stderr);
    let field = |name: &str| -> Option<u32> {
        text.lines()
            .find_map(|l| l.strip_prefix(name))
            .and_then(|v| v.trim().parse().ok())
    };
    assert_eq!(output.status.code(), Some(0), "{text}");
    let pid = field("pid ").unwrap_or_else(|| panic!("no pid in {text:?}"));
    let pgid = field("pgid ").unwrap_or_else(|| panic!("no pgid in {text:?}"));
    // sandbox-exec and the shell both exec in place, so this is the spawned process.
    assert_eq!(pid, spawned_pid, "{text}");
    assert_eq!(
        pgid, pid,
        "the sandboxed child must lead its own process group: {text}"
    );
}

// ---------------------------------------------------------------------------
// T12. read-only mode
// ---------------------------------------------------------------------------

#[tokio::test]
async fn t12_read_only_mode_denies_every_write_and_allows_reads() {
    if skip_if_nested("t12_read_only_mode_denies_every_write_and_allows_reads") {
        return;
    }
    let mut c = Checks::default();
    let (_d, ws) = canonical_tempdir();
    git_repo(&ws);
    std::fs::write(ws.join("r.txt"), "hello\n").unwrap();
    let tmp = tempfile::tempdir_in("/private/tmp").unwrap();
    let tmpdir = tempfile::tempdir().unwrap();
    let settings = SandboxSettings::default();
    let ro = |script: String| {
        let ws = ws.clone();
        let settings = settings.clone();
        async move { sh_in(&settings, FsAccess::ReadOnly, &ws, &script).await }
    };

    let (code, out) = ro("cat r.txt && git log -1 --format=%s".into()).await;
    c.check(
        code == Some(0) && out.contains("hello") && out.contains("init"),
        || format!("reading the workspace should work: exit={code:?} output={out:?}"),
    );

    let config_before = read(&ws.join(".git/config"));
    let head_before = read(&ws.join(".git/HEAD"));
    let tmp_name = tmp.path().file_name().unwrap().to_str().unwrap();
    for (label, script) in [
        ("overwrite a workspace file", "echo x > r.txt".to_string()),
        ("create a workspace file", "touch new".to_string()),
        ("mkdir in the workspace", "mkdir d".to_string()),
        ("/tmp", format!("touch '/tmp/{tmp_name}/x'")),
        ("/private/tmp", format!("touch '/private/tmp/{tmp_name}/y'")),
        ("$TMPDIR", format!("touch '{}/x'", tmpdir.path().display())),
        (".git/config", "echo '[x]' >> .git/config".to_string()),
        (".git/HEAD", "echo x > .git/HEAD".to_string()),
        (
            "git commit",
            "git -c user.email=t@t -c user.name=t commit -q --allow-empty -m ro".to_string(),
        ),
    ] {
        let (code, out) = ro(script.clone()).await;
        c.check(denied(code), || {
            format!("read-only {label}: `{script}` should be denied: exit={code:?} output={out:?}")
        });
    }
    c.check(read(&ws.join("r.txt")) == b"hello\n", || {
        "r.txt changed".into()
    });
    c.check(!ws.join("new").exists() && !ws.join("d").exists(), || {
        "ws file created".into()
    });
    c.check(
        !tmp.path().join("x").exists() && !tmp.path().join("y").exists(),
        || "/tmp file created".into(),
    );
    c.check(!tmpdir.path().join("x").exists(), || {
        "$TMPDIR file created".into()
    });
    c.check(
        read(&ws.join(".git/config")) == config_before
            && read(&ws.join(".git/HEAD")) == head_before,
        || ".git changed".into(),
    );

    c.finish();
}

// ---------------------------------------------------------------------------
// T13–T15. gitdir files that redirect where git loads config and hooks from
// ---------------------------------------------------------------------------
//
// Git reads `<gitdir>/commondir` for every gitdir and takes `config` and
// `hooks` from the dir it names (setup.c `get_common_dir_noenv`, path.c
// `common_list`). It reads `<gitdir>/config.worktree` once
// `extensions.worktreeConfig` is set (config.c `do_git_config_sequence`). A
// sandboxed command that could write either would make the user's next
// unsandboxed `git status` run a command of its choosing (`core.fsmonitor`).

/// Gitdirs that can live inside a workspace, as `(gitdir, the working tree git
/// uses it from)`, relative to the workspace. See [`gitdirs_workspace`].
const TOP: (&str, &str) = (".git", ".");
const NESTED: (&str, &str) = ("sub/.git", "sub");
const MODULE: (&str, &str) = (".git/modules/m", "m");
const LINKED: (&str, &str) = (".git/worktrees/wt", "wt");

/// A fresh temp workspace with one gitdir of each kind, set up outside the
/// sandbox: the top-level repo, a nested repo at `sub/`, a repo at `m/` whose
/// gitdir is `.git/modules/m` (a submodule's layout), and a linked worktree at
/// `wt/` whose gitdir is `.git/worktrees/wt`.
fn gitdirs_workspace() -> (tempfile::TempDir, PathBuf) {
    let (dir, ws) = canonical_tempdir();
    git_repo(&ws);
    git_repo(&ws.join(NESTED.1));
    std::fs::create_dir_all(ws.join(".git/modules")).unwrap();
    let module_gitdir = ws.join(MODULE.0);
    let module_gitdir = module_gitdir.to_str().expect("temp path is valid UTF-8");
    git(
        &ws,
        &["init", "-q", "--separate-git-dir", module_gitdir, MODULE.1],
    );
    git(
        &ws.join(MODULE.1),
        &["commit", "-q", "--allow-empty", "-m", "init"],
    );
    git(&ws, &["worktree", "add", "-q", "-b", "wt", LINKED.1]);
    (dir, ws)
}

/// Every way a sandboxed command could give the gitdir file `rel` content of
/// its own: while the file is absent, create it, rename a file onto it, or
/// make it a symlink or a hard link; while it exists, overwrite it, append to
/// it, replace it (by rename, or by rm and create) or remove it. Records a
/// failure for each attempt that is not denied or that changes the file, and
/// puts the file back as it was afterwards.
async fn expect_gitdir_file_protected(c: &mut Checks, ws: &Path, rel: &str) {
    let path = ws.join(rel);
    let scratch = ws.join("planted.tmp");
    let saved = std::fs::read(&path).ok();
    let original = saved.clone().unwrap_or_else(|| b"original\n".to_vec());
    let remove = |p: &Path| {
        let _ = std::fs::remove_file(p);
    };

    let absent = || std::fs::symlink_metadata(&path).is_err();
    for (label, script) in [
        ("create", format!("echo planted > '{rel}'")),
        (
            "rename onto",
            format!("echo planted > planted.tmp && mv planted.tmp '{rel}'"),
        ),
        (
            "symlink",
            format!("echo planted > planted.tmp && ln -s \"$PWD/planted.tmp\" '{rel}'"),
        ),
        (
            "hard link",
            format!("echo planted > planted.tmp && ln planted.tmp '{rel}'"),
        ),
    ] {
        remove(&path);
        let label = format!("{rel} (absent): {label}");
        expect_denied(c, ws, &label, &script, absent).await;
        remove(&path);
        remove(&scratch);
    }

    let intact = || {
        std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_file())
            && read(&path) == original
    };
    for (label, script) in [
        ("overwrite", format!("echo planted > '{rel}'")),
        ("append", format!("echo planted >> '{rel}'")),
        (
            "replace by rename",
            format!("echo planted > planted.tmp && mv -f planted.tmp '{rel}'"),
        ),
        (
            "replace by rm and create",
            format!("rm -f '{rel}' && echo planted > '{rel}'"),
        ),
        ("rm", format!("rm -f '{rel}'")),
    ] {
        remove(&path);
        std::fs::write(&path, &original).unwrap();
        let label = format!("{rel} (exists): {label}");
        expect_denied(c, ws, &label, &script, intact).await;
        remove(&scratch);
    }

    remove(&path);
    if let Some(saved) = saved {
        std::fs::write(&path, saved).unwrap();
    }
}

/// What an attacker would plant, in a temp dir of its own (any writable root
/// would do). `config` sets a `core.fsmonitor` hook, which `git status` runs,
/// that creates `marker`. `common/` is a minimal common dir (`objects/`,
/// `refs/` and that config) for a planted `commondir` to name.
struct Payload {
    _dir: tempfile::TempDir,
    marker: PathBuf,
    config: PathBuf,
    common: PathBuf,
}

impl Payload {
    fn new() -> Self {
        let (dir, root) = canonical_tempdir();
        let marker = root.join("hook-ran");
        let config = root.join("config");
        // Git runs the hook through the shell with two arguments appended
        // (fsmonitor.c `query_fsmonitor_hook`); `; exit 1; :` swallows them,
        // and the failure makes git fall back to a full scan.
        let fsmonitor = format!("touch '{}'; exit 1; :", marker.display());
        std::fs::write(
            &config,
            format!("[core]\n\trepositoryformatversion = 0\n\tfsmonitor = \"{fsmonitor}\"\n"),
        )
        .unwrap();
        let common = root.join("common");
        for sub in ["objects", "refs"] {
            std::fs::create_dir_all(common.join(sub)).unwrap();
        }
        std::fs::copy(&config, common.join("config")).unwrap();
        Payload {
            _dir: dir,
            marker,
            config,
            common,
        }
    }
}

/// The user's next git command: `git status`, unsandboxed, in `dir`. Returns
/// its exit code and output, for failure messages.
fn user_git_status(dir: &Path) -> String {
    let out = host_git(dir)
        .args(["status", "--short"])
        .output()
        .expect("run git status");
    format!(
        "exit={:?} stdout={:?} stderr={:?}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// End to end: after `plant` (a shell script, run from the workspace root)
/// runs in the sandbox, the user's next `git status` in `worktree` must not
/// run the planted hook. A control first runs the same script unsandboxed in
/// an identical workspace and requires the hook to run, so this check cannot
/// pass just because git ignores the planted file. `prepare` applies the
/// user's own setup to each workspace, outside the sandbox.
async fn expect_planted_hook_not_run(
    c: &mut Checks,
    label: &str,
    worktree: &str,
    prepare: impl Fn(&Path),
    plant: impl Fn(&Payload) -> String,
) {
    let (_ctl_dir, ctl) = gitdirs_workspace();
    prepare(&ctl);
    let payload = Payload::new();
    let script = plant(&payload);
    let out = std::process::Command::new("/bin/sh")
        .args(["-c", &script])
        .current_dir(&ctl)
        .output()
        .expect("run the control plant");
    assert!(
        out.status.success(),
        "{label}: control `{script}` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let status = user_git_status(&ctl.join(worktree));
    assert!(
        payload.marker.exists(),
        "{label}: control failed: `{script}` run outside the sandbox did not make `git status` \
         in {worktree} run the planted hook, so the sandboxed check would prove nothing: {status}"
    );

    let (_ws_dir, ws) = gitdirs_workspace();
    prepare(&ws);
    let payload = Payload::new();
    let script = plant(&payload);
    let (code, out) = ws_sh(&ws, &script).await;
    let status = user_git_status(&ws.join(worktree));
    c.check(!payload.marker.exists(), || {
        format!(
            "{label}: after the sandboxed `{script}` (exit={code:?} output={:?}), the user's \
             `git status` in {worktree} ran the planted hook: {status}",
            out.trim()
        )
    });
}

// ---------------------------------------------------------------------------
// T13. `commondir` in the top-level, a nested and a module gitdir
// ---------------------------------------------------------------------------

#[tokio::test]
async fn t13_commondir_cannot_be_planted_in_any_gitdir() {
    if skip_if_nested("t13_commondir_cannot_be_planted_in_any_gitdir") {
        return;
    }
    let mut c = Checks::default();
    let (_d, ws) = gitdirs_workspace();

    for (gitdir, _) in [TOP, NESTED, MODULE] {
        expect_gitdir_file_protected(&mut c, &ws, &format!("{gitdir}/commondir")).await;
    }
    // On a case-insensitive volume this is the same file.
    expect_denied(
        &mut c,
        &ws,
        ".GIT/CommonDir",
        "echo planted > .GIT/CommonDir",
        || !ws.join(".git/commondir").exists(),
    )
    .await;
    let _ = std::fs::remove_file(ws.join(".git/commondir"));

    for (gitdir, worktree) in [TOP, NESTED, MODULE] {
        expect_planted_hook_not_run(
            &mut c,
            &format!("{gitdir}/commondir"),
            worktree,
            |_| {},
            |p| format!("echo '{}' > '{gitdir}/commondir'", p.common.display()),
        )
        .await;
    }

    c.finish();
}

// ---------------------------------------------------------------------------
// T14. `config.worktree` in every kind of gitdir
// ---------------------------------------------------------------------------

#[tokio::test]
async fn t14_config_worktree_cannot_be_planted_in_any_gitdir() {
    if skip_if_nested("t14_config_worktree_cannot_be_planted_in_any_gitdir") {
        return;
    }
    let mut c = Checks::default();
    let (_d, ws) = gitdirs_workspace();

    for (gitdir, _) in [TOP, NESTED, MODULE, LINKED] {
        expect_gitdir_file_protected(&mut c, &ws, &format!("{gitdir}/config.worktree")).await;
    }

    // Git reads `config.worktree` only once the user's own config sets
    // `extensions.worktreeConfig` (`git sparse-checkout` does, for one).
    for (gitdir, worktree) in [TOP, NESTED, MODULE, LINKED] {
        expect_planted_hook_not_run(
            &mut c,
            &format!("{gitdir}/config.worktree"),
            worktree,
            |ws| {
                git(
                    &ws.join(worktree),
                    &["config", "extensions.worktreeConfig", "true"],
                )
            },
            |p| format!("cat '{}' > '{gitdir}/config.worktree'", p.config.display()),
        )
        .await;
    }

    c.finish();
}

// ---------------------------------------------------------------------------
// T15. a linked worktree's gitdir (`.git/worktrees/<id>`)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn t15_linked_worktree_gitdir_cannot_redirect_config_or_hooks() {
    if skip_if_nested("t15_linked_worktree_gitdir_cannot_redirect_config_or_hooks") {
        return;
    }
    let mut c = Checks::default();
    let (_d, ws) = gitdirs_workspace();
    // Linked worktrees of the nested repo and of the module repo too.
    git(
        &ws.join(NESTED.1),
        &["worktree", "add", "-q", "-b", "w", "../subwt"],
    );
    git(
        &ws.join(MODULE.1),
        &["worktree", "add", "-q", "-b", "w", "../mwt"],
    );
    let (gitdir, worktree) = LINKED;

    // `config` and `hooks` here are ignored while `commondir` names the main
    // gitdir, but git falls back to them if `commondir` is gone.
    for rel in [
        format!("{gitdir}/commondir"),
        format!("{gitdir}/config"),
        "sub/.git/worktrees/subwt/commondir".to_string(),
        ".git/modules/m/worktrees/mwt/commondir".to_string(),
    ] {
        expect_gitdir_file_protected(&mut c, &ws, &rel).await;
    }
    let hooks = ws.join(gitdir).join("hooks");
    expect_denied(
        &mut c,
        &ws,
        "hooks in a linked worktree's gitdir",
        &format!("mkdir -p '{gitdir}/hooks' && touch '{gitdir}/hooks/pre-commit'"),
        || !hooks.exists(),
    )
    .await;
    let _ = std::fs::remove_dir_all(&hooks);

    expect_planted_hook_not_run(
        &mut c,
        &format!("{gitdir}/commondir"),
        worktree,
        |_| {},
        |p| format!("echo '{}' > '{gitdir}/commondir'", p.common.display()),
    )
    .await;
    expect_planted_hook_not_run(
        &mut c,
        &format!("{gitdir}/config after removing {gitdir}/commondir"),
        worktree,
        |_| {},
        |p| {
            format!(
                "rm -f '{gitdir}/commondir' && mkdir -p '{gitdir}/objects' '{gitdir}/refs' \
                 && cat '{}' > '{gitdir}/config'",
                p.config.display()
            )
        },
    )
    .await;

    // Everyday git in the linked worktree still works: its HEAD, index, logs
    // and per-worktree refs live in this gitdir.
    const GIT: &str = "git -c user.email=t@t -c user.name=t";
    for (label, step) in [
        (
            "commit",
            format!("echo x > f && {GIT} add f && {GIT} commit -q -m on-wt"),
        ),
        ("switch -c", format!("{GIT} switch -q -c wt2")),
        ("checkout -", format!("{GIT} checkout -q -")),
        ("stash", format!("echo y > f && {GIT} stash -q")),
        ("status", format!("{GIT} status --short")),
    ] {
        expect_allowed(
            &mut c,
            &ws,
            &format!("[{worktree}] {label}"),
            &format!("cd {worktree} && {step}"),
        )
        .await;
    }

    c.finish();
}

// ---------------------------------------------------------------------------
// T16. `gitweb/` and `pid` in every kind of gitdir
// ---------------------------------------------------------------------------
//
// `git instaweb` runs files from `$GIT_DIR/gitweb/` (it looks for the httpd
// there and uses an existing `gitweb_config.perl`), and `git instaweb --stop`
// runs `kill $(cat "$GIT_DIR/pid")`.

/// A `sleep` this test starts, so a planted `pid` names a process of its own.
/// Killed when dropped.
struct Sleeper(std::process::Child);

impl Sleeper {
    fn start() -> Self {
        Sleeper(
            std::process::Command::new("/bin/sleep")
                .arg("60")
                .stdin(Stdio::null())
                .spawn()
                .expect("start sleep"),
        )
    }

    fn pid(&self) -> u32 {
        self.0.id()
    }

    /// Whether it is still running after `git instaweb --stop` returned (a
    /// signal it sent may take a moment to be delivered and reaped).
    fn survives(&mut self) -> bool {
        for _ in 0..20 {
            if self.0.try_wait().expect("poll sleep").is_some() {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        true
    }
}

impl Drop for Sleeper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The user's `git instaweb --stop`, unsandboxed, in `dir`.
fn user_instaweb_stop(dir: &Path) -> String {
    let out = host_git(dir)
        .args(["instaweb", "--stop"])
        .output()
        .expect("run git instaweb --stop");
    format!(
        "exit={:?} stderr={:?}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// Whether this git has `instaweb` (a minimal git install may not).
fn git_has_instaweb() -> bool {
    let out = host_git(Path::new("/"))
        .arg("--exec-path")
        .output()
        .expect("run git --exec-path");
    let exec_path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Path::new(&exec_path).join("git-instaweb").is_file()
}

/// End to end: after `plant` (run from the workspace root) runs in the
/// sandbox, the user's `git instaweb --stop` in `worktree` must not kill the
/// process whose pid it planted. A control first runs the same script
/// unsandboxed in an identical workspace and requires the process to be
/// killed, so the check cannot pass just because git ignores the file.
async fn expect_planted_pid_not_killed(
    c: &mut Checks,
    label: &str,
    worktree: &str,
    plant: impl Fn(u32) -> String,
) {
    let (_ctl_dir, ctl) = gitdirs_workspace();
    let mut victim = Sleeper::start();
    let script = plant(victim.pid());
    let out = std::process::Command::new("/bin/sh")
        .args(["-c", &script])
        .current_dir(&ctl)
        .output()
        .expect("run the control plant");
    assert!(
        out.status.success(),
        "{label}: control `{script}` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stop = user_instaweb_stop(&ctl.join(worktree));
    assert!(
        !victim.survives(),
        "{label}: control failed: `{script}` run outside the sandbox did not make \
         `git instaweb --stop` in {worktree} kill the planted pid, so the sandboxed check would \
         prove nothing: {stop}"
    );

    let (_ws_dir, ws) = gitdirs_workspace();
    let mut victim = Sleeper::start();
    let script = plant(victim.pid());
    let (code, out) = ws_sh(&ws, &script).await;
    let stop = user_instaweb_stop(&ws.join(worktree));
    c.check(victim.survives(), || {
        format!(
            "{label}: after the sandboxed `{script}` (exit={code:?} output={:?}), the user's \
             `git instaweb --stop` in {worktree} killed the planted pid: {stop}",
            out.trim()
        )
    });
}

#[tokio::test]
async fn t16_gitweb_and_pid_cannot_be_planted_in_any_gitdir() {
    if skip_if_nested("t16_gitweb_and_pid_cannot_be_planted_in_any_gitdir") {
        return;
    }
    let mut c = Checks::default();
    let (_d, ws) = gitdirs_workspace();

    for (gitdir, _) in [TOP, NESTED, MODULE, LINKED] {
        expect_gitdir_file_protected(&mut c, &ws, &format!("{gitdir}/pid")).await;

        let gitweb = ws.join(gitdir).join("gitweb");
        expect_denied(
            &mut c,
            &ws,
            &format!("{gitdir}/gitweb (absent): create"),
            &format!("mkdir '{gitdir}/gitweb' && echo planted > '{gitdir}/gitweb/lighttpd'"),
            || !gitweb.exists(),
        )
        .await;
        let _ = std::fs::remove_dir_all(&gitweb);

        // As an earlier `git instaweb` leaves it.
        std::fs::create_dir_all(gitweb.join("tmp")).unwrap();
        expect_gitdir_file_protected(&mut c, &ws, &format!("{gitdir}/gitweb/gitweb_config.perl"))
            .await;
        expect_denied(
            &mut c,
            &ws,
            &format!("{gitdir}/gitweb (exists): rename away"),
            &format!("mv '{gitdir}/gitweb' '{gitdir}/gitweb.old'"),
            || gitweb.join("tmp").is_dir(),
        )
        .await;
    }

    if git_has_instaweb() {
        for (gitdir, worktree) in [TOP, NESTED, MODULE, LINKED] {
            expect_planted_pid_not_killed(&mut c, &format!("{gitdir}/pid"), worktree, |pid| {
                format!("echo {pid} > '{gitdir}/pid'")
            })
            .await;
        }
    } else {
        eprintln!("[seatbelt.rs] t16: this git has no `instaweb`; skipping the end-to-end checks");
    }

    c.finish();
}
