//! Linux git-metadata protection end to end: `LinuxSandbox::prepare` runs real commands with
//! the guard, in the basic tier, and tracks the processes they leave running.
//!
//! Like `linux_sandbox.rs`, a test skips when the sandbox or a tool it needs is missing, unless
//! `HARNESS_REQUIRE_LINUX_SANDBOX=1`.
//!
//! The tests run one at a time ([`SERIAL`]): this process is the "harness" whose descendants the
//! survivor check looks at, so a process another test left running would count here.

#![cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use harness_core::tool::{CommandSandbox, GitProtection, GuardReport};
use harness_sandbox::{FsAccess, LinuxSandbox, SandboxSettings, linux_sandbox_available};

/// Held by every test for its whole run.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn skip_or_require(reason: &str) -> bool {
    assert!(
        std::env::var("HARNESS_REQUIRE_LINUX_SANDBOX").as_deref() != Ok("1"),
        "{reason} (HARNESS_REQUIRE_LINUX_SANDBOX=1 is set)"
    );
    eprintln!("skipping: {reason}");
    true
}

/// Whether `name` is on `PATH`; if not, the test skips (or fails, when the sandbox is required).
fn have(name: &str) -> bool {
    let found = std::process::Command::new("sh")
        .args(["-c", &format!("command -v {name}")])
        .stdout(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    found || !skip_or_require(&format!("{name} is not installed"))
}

/// A git repository under `$HOME` (not `/tmp`, which the sandbox always makes writable), with a
/// commit identity, and a quarantine directory next to it. Both are removed on drop.
struct Env {
    ws: PathBuf,
    quarantine: PathBuf,
}

impl Env {
    fn new() -> Option<Env> {
        if !linux_sandbox_available() {
            skip_or_require("linux sandbox unavailable");
            return None;
        }
        let Some(home) = std::env::var_os("HOME") else {
            skip_or_require("$HOME not set");
            return None;
        };
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = format!(
            "{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let ws = PathBuf::from(&home).join(format!(".harness-guard-test-ws-{id}"));
        let quarantine = PathBuf::from(&home).join(format!(".harness-guard-test-q-{id}"));
        std::fs::create_dir(&ws).unwrap();
        let ws = ws.canonicalize().unwrap();
        let env = Env { ws, quarantine };
        for args in [
            &["init", "-q"][..],
            &["config", "user.email", "t@example.com"],
            &["config", "user.name", "original"],
            &["config", "commit.gpgsign", "false"],
        ] {
            if !env.git(args).status.success() {
                skip_or_require("git is not installed");
                return None;
            }
        }
        Some(env)
    }

    fn git(&self, args: &[&str]) -> Output {
        std::process::Command::new("git")
            .args(args)
            .current_dir(&self.ws)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap_or_else(|e| panic!("git {args:?}: {e}"))
    }

    fn settings(&self) -> SandboxSettings {
        SandboxSettings {
            quarantine_dir: Some(self.quarantine.clone()),
            ..SandboxSettings::default()
        }
    }

    /// A basic-tier sandbox whose session has started, as `harness ask` starts it.
    fn basic(&self) -> LinuxSandbox {
        let sandbox = LinuxSandbox::with_git_protection(
            self.settings(),
            GitProtection::Basic {
                reason: "forced by the test".into(),
            },
        );
        sandbox.start_session(&self.ws);
        sandbox
    }

    /// The one entry the quarantine holds for `rel` (relative to the workspace), where every
    /// `.git` in the path is stored as `dot-git` and every `HEAD` as `HEAD.quarantined`.
    fn quarantined(&self, rel: &str) -> PathBuf {
        let stored: PathBuf = Path::new(rel)
            .iter()
            .map(|name| match name.to_str() {
                Some(".git") => OsStr::new("dot-git"),
                Some("HEAD") => OsStr::new("HEAD.quarantined"),
                _ => name,
            })
            .collect();
        let found: Vec<PathBuf> = std::fs::read_dir(&self.quarantine)
            .unwrap_or_else(|e| panic!("no quarantine at {:?}: {e}", self.quarantine))
            .map(|e| e.unwrap().path().join(&stored))
            .filter(|p| std::fs::symlink_metadata(p).is_ok())
            .collect();
        assert_eq!(found.len(), 1, "{rel} in quarantine: {found:?}");
        found.into_iter().next().unwrap()
    }

    fn exists(&self, rel: &str) -> bool {
        std::fs::symlink_metadata(self.ws.join(rel)).is_ok()
    }

    fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.ws.join(rel)).unwrap()
    }

    fn append(&self, rel: &str, text: &str) {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(self.ws.join(rel))
            .unwrap();
        file.write_all(text.as_bytes()).unwrap();
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.ws);
        let _ = std::fs::remove_dir_all(&self.quarantine);
    }
}

/// Runs `script` with `/bin/sh -c` through `sandbox.prepare`, in `cwd`, as the bash tool does:
/// the guard is told the pid right after the spawn, and finished once the command has been
/// waited for.
async fn run_in(
    sandbox: &LinuxSandbox,
    ws: &Path,
    cwd: &Path,
    script: &str,
) -> (std::io::Result<Output>, Option<GuardReport>) {
    let prepared = sandbox
        .prepare(FsAccess::WorkspaceWrite, ws, "/bin/sh", &["-c", script])
        .expect("prepare the sandboxed command");
    let mut guard = prepared.guard.expect("a workspace-write guard");
    let mut cmd = prepared.command;
    cmd.current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = match cmd.spawn() {
        Ok(child) => {
            if let Some(pid) = child.id() {
                guard.started(pid);
            }
            child.wait_with_output().await
        }
        Err(e) => Err(e),
    };
    (output, guard.finish())
}

async fn run(sandbox: &LinuxSandbox, env: &Env, script: &str) -> (Output, Option<GuardReport>) {
    let (output, report) = run_in(sandbox, &env.ws, &env.ws, script).await;
    (output.expect("spawn the sandboxed command"), report)
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Polls `done` every 20 ms for up to 10 s.
async fn wait_until(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// What `/proc/<pid>/stat` says about a process.
#[derive(Debug)]
struct Process {
    pid: i32,
    state: u8,
    ppid: i32,
    sid: i32,
}

/// Every process there is. One that exits while this looks is left out.
fn processes() -> Vec<Process> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir("/proc").unwrap().flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        let Ok(stat) = std::fs::read(entry.path().join("stat")) else {
            continue;
        };
        // The command name, in parentheses, can hold anything; the fields after it cannot.
        let Some(close) = stat.iter().rposition(|&b| b == b')') else {
            continue;
        };
        let text = String::from_utf8_lossy(&stat[close + 1..]).into_owned();
        let fields: Vec<&str> = text.split_whitespace().collect();
        let (Some(state), Some(ppid), Some(sid)) = (
            fields.first().map(|state| state.as_bytes()[0]),
            fields.get(1).and_then(|ppid| ppid.parse().ok()),
            fields.get(3).and_then(|sid| sid.parse().ok()),
        ) else {
            continue;
        };
        found.push(Process {
            pid,
            state,
            ppid,
            sid,
        });
    }
    found
}

/// This process's pid and session id.
fn me() -> (i32, i32) {
    let pid = i32::try_from(std::process::id()).unwrap();
    let sid = processes()
        .into_iter()
        .find(|process| process.pid == pid)
        .expect("this process in /proc")
        .sid;
    (pid, sid)
}

/// This process's zombie children in another session than its own: exited orphans of
/// sandboxed commands, left for it to reap.
fn orphaned_zombies() -> Vec<i32> {
    let (me, my_sid) = me();
    processes()
        .into_iter()
        .filter(|process| process.ppid == me && process.state == b'Z' && process.sid != my_sid)
        .map(|process| process.pid)
        .collect()
}

/// This process's live descendants in another session than its own: what sandboxed commands
/// left running.
fn live_descendants_elsewhere() -> Vec<i32> {
    let (me, my_sid) = me();
    let all = processes();
    let mut below = std::collections::BTreeSet::from([me]);
    // Parents are not always listed before their children: go again until nothing is added.
    loop {
        let known = below.len();
        for process in &all {
            if below.contains(&process.ppid) {
                below.insert(process.pid);
            }
        }
        if below.len() == known {
            break;
        }
    }
    all.iter()
        .filter(|process| {
            process.pid != me
                && below.contains(&process.pid)
                && !matches!(process.state, b'Z' | b'X')
                && process.sid != my_sid
        })
        .map(|process| process.pid)
        .collect()
}

/// The state letter of `pid`, if it exists.
fn state_of(pid: u32) -> Option<u8> {
    let pid = i32::try_from(pid).unwrap();
    processes()
        .into_iter()
        .find(|process| process.pid == pid)
        .map(|process| process.state)
}

/// A child process killed and waited for when dropped.
struct Killed(std::process::Child);

impl Drop for Killed {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

// ---------------------------------------------------------------------------
// The basic tier: undone after the fact
// ---------------------------------------------------------------------------

#[tokio::test]
async fn basic_tier_quarantines_a_planted_hook() {
    let _serial = SERIAL.lock().await;
    let Some(env) = Env::new() else { return };
    let (output, report) = run(
        &env.basic(),
        &env,
        "echo 'echo pwned' > .git/hooks/pre-commit",
    )
    .await;
    assert!(output.status.success(), "{}", stderr(&output));
    let report = report.expect("a report");
    assert!(report.blocked);
    assert!(
        report.message.contains("- .git/hooks/pre-commit: "),
        "{}",
        report.message
    );
    assert!(!env.exists(".git/hooks/pre-commit"));
    assert_eq!(
        std::fs::read_to_string(env.quarantined(".git/hooks/pre-commit")).unwrap(),
        "echo pwned\n"
    );
}

#[tokio::test]
async fn basic_tier_restores_a_changed_config() {
    let _serial = SERIAL.lock().await;
    let Some(env) = Env::new() else { return };
    let before = std::fs::read(env.ws.join(".git/config")).unwrap();
    let (output, report) = run(&env.basic(), &env, "git config user.name evil").await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(report.expect("a report").blocked);
    assert_eq!(std::fs::read(env.ws.join(".git/config")).unwrap(), before);
    let changed = std::fs::read_to_string(env.quarantined(".git/config")).unwrap();
    assert!(changed.contains("evil"), "{changed}");
}

#[tokio::test]
async fn basic_tier_quarantines_a_new_commondir() {
    let _serial = SERIAL.lock().await;
    let Some(env) = Env::new() else { return };
    let (_, report) = run(&env.basic(), &env, "printf /tmp/elsewhere > .git/commondir").await;
    assert!(report.expect("a report").blocked);
    assert!(!env.exists(".git/commondir"));
    env.quarantined(".git/commondir");
}

#[tokio::test]
async fn git_init_of_a_nested_repository_is_undone() {
    let _serial = SERIAL.lock().await;
    let Some(env) = Env::new() else { return };
    let (output, report) = run(
        &env.basic(),
        &env,
        "git init -q sub && echo kept > sub/file",
    )
    .await;
    assert!(output.status.success(), "{}", stderr(&output));
    let report = report.expect("a report");
    assert!(report.blocked);
    assert!(
        report
            .message
            .contains("- sub/.git: a new repository; moved to "),
        "{}",
        report.message
    );
    assert!(!env.exists("sub/.git"));
    assert!(env.exists("sub/file"));
    assert!(
        env.quarantined("sub/.git")
            .join("HEAD.quarantined")
            .exists()
    );
}

#[tokio::test]
async fn basic_tier_commit_checkout_and_stash_get_no_report() {
    let _serial = SERIAL.lock().await;
    let Some(env) = Env::new() else { return };
    let script = "set -e; git commit -q --allow-empty -m one; git checkout -q -b topic; \
                  echo x > f; git add f; git stash -q; git stash pop -q";
    let (output, report) = run(&env.basic(), &env, script).await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(report, None);
}

#[tokio::test]
async fn names_a_background_process_plants_later_are_caught_before_the_next_command() {
    let _serial = SERIAL.lock().await;
    let Some(env) = Env::new() else { return };
    let sandbox = env.basic();
    let (_, report) = run(
        &sandbox,
        &env,
        "(sleep 1; printf /tmp/elsewhere > .git/commondir) > /dev/null 2>&1 &",
    )
    .await;
    assert_eq!(report, None);
    wait_until("the background process planted commondir", || {
        env.exists(".git/commondir")
    })
    .await;
    let (_, report) = run(&sandbox, &env, "true").await;
    let report = report.expect("a report");
    assert!(!report.blocked, "{}", report.message);
    assert!(
        report.message.starts_with("[before this command ran"),
        "{}",
        report.message
    );
    assert!(!env.exists(".git/commondir"));
}

#[tokio::test]
async fn the_session_reports_a_workspace_it_cannot_scan_whole_without_blocking() {
    let _serial = SERIAL.lock().await;
    let Some(env) = Env::new() else { return };
    // An ignore file that is a symlink cannot be used as git would use it: no scan is complete.
    std::os::unix::fs::symlink("elsewhere", env.ws.join(".gitignore")).unwrap();
    let sandbox = env.basic();
    let (output, report) = run(&sandbox, &env, "true").await;
    assert!(output.status.success(), "{}", stderr(&output));
    let report = report.expect("a report");
    assert!(!report.blocked, "{}", report.message);
    assert!(
        report
            .message
            .contains("harness could not scan the whole workspace"),
        "{}",
        report.message
    );
    let (_, again) = run(&sandbox, &env, "true").await;
    assert_eq!(again, None, "said once per session");
}

// ---------------------------------------------------------------------------
// Processes a command leaves running (the basic tier)
// ---------------------------------------------------------------------------

/// Runs `script`, which leaves a process running that appends `# evil` to `.git/config` a
/// second after the command ends, and checks the next command puts the config back.
async fn a_config_rewrite_after_the_command_is_undone_before_the_next(env: &Env, script: &str) {
    let sandbox = env.basic();
    let before = env.read(".git/config");
    let (output, report) = run(&sandbox, env, script).await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(report, None);
    wait_until("the process left running rewrote the config", || {
        env.read(".git/config").contains("# evil")
    })
    .await;
    let (output, report) = run(&sandbox, env, "true").await;
    assert!(output.status.success(), "{}", stderr(&output));
    let report = report.expect("a report");
    assert!(!report.blocked, "{}", report.message);
    assert!(
        report.message.starts_with("[before this command ran"),
        "{}",
        report.message
    );
    assert!(
        report
            .message
            .contains("- .git/config: changed; restored the earlier version"),
        "{}",
        report.message
    );
    assert_eq!(env.read(".git/config"), before);
    let changed = std::fs::read_to_string(env.quarantined(".git/config")).unwrap();
    assert!(changed.contains("# evil"), "{changed}");
}

#[tokio::test]
async fn a_background_job_that_rewrites_config_later_is_undone_before_the_next_command() {
    let _serial = SERIAL.lock().await;
    let Some(env) = Env::new() else { return };
    a_config_rewrite_after_the_command_is_undone_before_the_next(
        &env,
        "(sleep 1; echo '# evil' >> .git/config) > /dev/null 2>&1 &",
    )
    .await;
}

#[tokio::test]
async fn a_detached_job_that_rewrites_config_later_is_undone_before_the_next_command() {
    let _serial = SERIAL.lock().await;
    let Some(env) = Env::new() else { return };
    if !have("setsid") {
        return;
    }
    // Its own session, and a double fork: its parent exits at once.
    a_config_rewrite_after_the_command_is_undone_before_the_next(
        &env,
        "setsid -f sh -c '(sleep 1; echo \"# evil\" >> .git/config) &' \
         > /dev/null 2>&1 < /dev/null",
    )
    .await;
}

/// A program whose main thread exits at once while a second thread runs on, and appends
/// `# evil` to `.git/config` a second later. Built with `cc` in `dir`; `None` when that is not
/// possible here.
fn leader_that_exits_first(dir: &Path) -> Option<PathBuf> {
    let source = dir.join("leader-exits.c");
    std::fs::write(
        &source,
        r##"#include <pthread.h>
#include <stdio.h>
#include <unistd.h>

static void *later(void *unused) {
    (void)unused;
    sleep(1);
    FILE *config = fopen(".git/config", "a");
    if (config) {
        fputs("# evil\n", config);
        fclose(config);
    }
    return 0;
}

int main(void) {
    pthread_t thread;
    if (pthread_create(&thread, 0, later, 0) != 0) {
        return 1;
    }
    pthread_exit(0);
}
"##,
    )
    .unwrap();
    let program = dir.join("leader-exits");
    let built = std::process::Command::new("cc")
        .arg("-pthread")
        .arg("-o")
        .arg(&program)
        .arg(&source)
        .output();
    match built {
        Ok(output) if output.status.success() => Some(program),
        Ok(output) => {
            eprintln!(
                "skipping: cc could not build the test helper: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            None
        }
        Err(e) => {
            eprintln!("skipping: no C compiler (cc) to build the test helper: {e}");
            None
        }
    }
}

#[tokio::test]
async fn a_process_whose_main_thread_exited_still_counts_and_its_change_is_undone() {
    let _serial = SERIAL.lock().await;
    let Some(env) = Env::new() else { return };
    let dir = tempfile::tempdir().unwrap();
    let Some(helper) = leader_that_exits_first(dir.path()) else {
        return;
    };
    // `/proc/<pid>/stat` shows the main thread: a zombie, by the time the command ends.
    let script = format!(
        "'{}' > /dev/null 2>&1 & pid=$!; i=0; \
         while [ $i -lt 500 ] && ! grep -q '^State:[[:space:]]*Z' /proc/$pid/status; do \
         i=$((i + 1)); sleep 0.01; done",
        helper.display()
    );
    a_config_rewrite_after_the_command_is_undone_before_the_next(&env, &script).await;
}

#[tokio::test]
async fn without_survivors_a_config_change_between_commands_is_left_alone() {
    let _serial = SERIAL.lock().await;
    let Some(env) = Env::new() else { return };
    let sandbox = env.basic();
    let (_, report) = run(&sandbox, &env, "true").await;
    assert_eq!(report, None);
    env.append(".git/config", "# mine\n");
    let (_, report) = run(&sandbox, &env, "true").await;
    assert_eq!(report, None);
    assert!(env.read(".git/config").ends_with("# mine\n"));
}

#[tokio::test]
async fn a_process_harness_starts_in_its_own_session_is_not_a_survivor() {
    let _serial = SERIAL.lock().await;
    let Some(env) = Env::new() else { return };
    let sandbox = env.basic();
    // Like harness's own helpers: a child in this process's session.
    let _sleeper = Killed(
        std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep"),
    );
    let (_, report) = run(&sandbox, &env, "true").await;
    assert_eq!(report, None);
    env.append(".git/config", "# mine\n");
    let (_, report) = run(&sandbox, &env, "true").await;
    assert_eq!(report, None);
    assert!(env.read(".git/config").ends_with("# mine\n"));
}

#[tokio::test]
async fn no_zombies_remain_after_a_detached_job_exits_and_the_next_command_runs() {
    let _serial = SERIAL.lock().await;
    let Some(env) = Env::new() else { return };
    if !have("setsid") {
        return;
    }
    let sandbox = env.basic();
    let (output, _) = run(
        &sandbox,
        &env,
        "setsid -f sh -c '(sleep 1; touch done) &' > /dev/null 2>&1 < /dev/null",
    )
    .await;
    assert!(output.status.success(), "{}", stderr(&output));
    wait_until("the detached job ran", || env.exists("done")).await;
    wait_until("nothing the command left is still running", || {
        live_descendants_elsewhere().is_empty()
    })
    .await;
    // It was reparented to this process, which has not reaped it yet.
    assert!(
        !orphaned_zombies().is_empty(),
        "the detached job is not a zombie child of this process"
    );
    let (output, _) = run(&sandbox, &env, "true").await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(orphaned_zombies(), Vec::<i32>::new());
}

/// Spawns `echo out; exit 7` with `access` as the bash tool does, waits until it is a zombie,
/// runs another command (which reaps orphans before and after it runs), and checks the first
/// command's exit status and output are still there for tokio.
async fn the_command_harness_waits_for_is_never_reaped(access: FsAccess) {
    let Some(env) = Env::new() else { return };
    let Some(other) = Env::new() else { return };
    let sandbox = env.basic();
    let prepared = sandbox
        .prepare(access, &env.ws, "/bin/sh", &["-c", "echo out; exit 7"])
        .expect("prepare the sandboxed command");
    let mut guard = prepared.guard.expect("a guard that registers the command");
    let mut cmd = prepared.command;
    cmd.current_dir(&env.ws)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = cmd.spawn().expect("spawn the sandboxed command");
    let pid = child.id().expect("a pid");
    guard.started(pid);
    // It exits, and stays a zombie in its own session until tokio waits for it.
    wait_until("the command exited", || state_of(pid) == Some(b'Z')).await;
    let (_, report) = run(&sandbox, &other, "true").await;
    assert_eq!(report, None);
    assert_eq!(state_of(pid), Some(b'Z'), "the guard reaped the command");
    let output = child
        .wait_with_output()
        .await
        .expect("tokio waits for its own child");
    assert_eq!(output.status.code(), Some(7));
    assert_eq!(output.stdout, b"out\n");
    assert_eq!(guard.finish(), None);
}

#[tokio::test]
async fn the_command_harness_waits_for_is_never_reaped_by_the_guard() {
    let _serial = SERIAL.lock().await;
    the_command_harness_waits_for_is_never_reaped(FsAccess::WorkspaceWrite).await;
}

#[tokio::test]
async fn a_read_only_command_harness_waits_for_is_never_reaped_either() {
    let _serial = SERIAL.lock().await;
    the_command_harness_waits_for_is_never_reaped(FsAccess::ReadOnly).await;
}
