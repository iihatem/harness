//! Linux git-metadata protection end to end: `LinuxSandbox::prepare` runs real commands with
//! the guard and its watcher, in the basic tier (forced, so it runs on every Linux host), where
//! it also tracks the processes commands leave running, and in the full tier (where user
//! namespaces work).
//!
//! Like `linux_sandbox.rs`, a test skips when the sandbox or a tool it needs is missing, unless
//! `HARNESS_REQUIRE_LINUX_SANDBOX=1`. Full-tier tests skip when the probe picks the basic tier,
//! unless `HARNESS_EXPECT_LINUX_TIER=full` (CI's full-tier job); with
//! `HARNESS_EXPECT_LINUX_TIER=basic` (the stock job) the probe must pick the basic tier.
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
use harness_sandbox::{
    FsAccess, LinuxSandbox, SandboxSettings, linux_git_protection, linux_sandbox_available,
    subreaper_active,
};

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

/// The tier CI expects the probe to pick here: `full`, `basic`, or none.
fn expected_tier() -> Option<String> {
    std::env::var("HARNESS_EXPECT_LINUX_TIER")
        .ok()
        .filter(|tier| !tier.is_empty())
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

    /// `git args`, which must succeed; its error output says why not.
    fn git_ok(&self, args: &[&str]) {
        let output = self.git(args);
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
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

    /// A full-tier sandbox whose session has started, or `None` (having skipped) where the probe
    /// picks the basic tier.
    fn full(&self) -> Option<LinuxSandbox> {
        match linux_git_protection() {
            GitProtection::Full => {
                let sandbox =
                    LinuxSandbox::with_git_protection(self.settings(), GitProtection::Full);
                sandbox.start_session(&self.ws);
                Some(sandbox)
            }
            GitProtection::Basic { reason } => {
                assert!(
                    expected_tier().as_deref() != Some("full"),
                    "HARNESS_EXPECT_LINUX_TIER=full, but the probe picked the basic tier: {reason}"
                );
                eprintln!("skipping: user namespaces are unavailable ({reason})");
                None
            }
        }
    }

    /// The one entry the quarantine holds for `rel` (relative to the workspace): see [`stored`].
    fn quarantined(&self, rel: &str) -> PathBuf {
        let stored = stored(rel);
        let found: Vec<PathBuf> = std::fs::read_dir(&self.quarantine)
            .unwrap_or_else(|e| panic!("no quarantine at {:?}: {e}", self.quarantine))
            .map(|e| e.unwrap().path().join(&stored))
            .filter(|p| std::fs::symlink_metadata(p).is_ok())
            .collect();
        assert_eq!(found.len(), 1, "{rel} in quarantine: {found:?}");
        found.into_iter().next().unwrap()
    }

    /// Whether the quarantine holds anything for `rel` yet.
    fn in_quarantine(&self, rel: &str) -> bool {
        let stored = stored(rel);
        std::fs::read_dir(&self.quarantine).is_ok_and(|dirs| {
            dirs.filter_map(Result::ok)
                .any(|dir| std::fs::symlink_metadata(dir.path().join(&stored)).is_ok())
        })
    }

    /// Waits up to four seconds for the watcher to quarantine `rel`.
    async fn wait_for_quarantine(&self, rel: &str) {
        let deadline = Instant::now() + Duration::from_secs(4);
        while !self.in_quarantine(rel) {
            assert!(
                Instant::now() < deadline,
                "{rel} was not moved while the command ran"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    fn exists(&self, rel: &str) -> bool {
        std::fs::symlink_metadata(self.ws.join(rel)).is_ok()
    }

    fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.ws.join(rel)).unwrap()
    }

    /// What `rel` holds, if it is there: the guard may be moving it right now.
    fn read_now(&self, rel: &str) -> Option<String> {
        std::fs::read_to_string(self.ws.join(rel)).ok()
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

/// `rel` as the quarantine stores it: every `.git` in the path as `dot-git`, and every `HEAD` as
/// `HEAD.quarantined`.
fn stored(rel: &str) -> PathBuf {
    Path::new(rel)
        .iter()
        .map(|name| match name.to_str() {
            Some(".git") => OsStr::new("dot-git"),
            Some("HEAD") => OsStr::new("HEAD.quarantined"),
            _ => name,
        })
        .collect()
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

/// How many threads of this process are named `name`.
fn threads_named(name: &str) -> usize {
    std::fs::read_dir("/proc/self/task")
        .unwrap()
        .flatten()
        .filter(|task| {
            std::fs::read_to_string(task.path().join("comm"))
                .is_ok_and(|comm| comm.trim_end() == name)
        })
        .count()
}

/// A process a command left running, which wrote its pid to a file: killed when dropped.
struct Job(i32);

impl Job {
    async fn from_pid_file(env: &Env, rel: &str) -> Job {
        wait_until("the job wrote its pid", || {
            env.read_now(rel).is_some_and(|pid| pid.ends_with('\n'))
        })
        .await;
        Job(env.read(rel).trim().parse().expect("a pid"))
    }

    fn alive(&self) -> bool {
        state_of(u32::try_from(self.0).unwrap()).is_some_and(|state| !matches!(state, b'Z' | b'X'))
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        // SAFETY: sends a signal to the job's pid; it has not been reaped, since the job is a
        // descendant of this process in another session, which only the guard reaps, and the
        // tests run one at a time.
        unsafe { libc::kill(self.0, libc::SIGKILL) };
    }
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
// The tier
// ---------------------------------------------------------------------------

/// Where the full tier is expected, the probe must find it. Where it is not (user namespaces
/// blocked), the reason must be the setup step the kernel refused, as the child reported it
/// (`<step> failed: <error>`), not a misread outcome of a setup that worked.
#[test]
fn the_probe_picks_the_expected_tier() {
    if !linux_sandbox_available() {
        skip_or_require("linux sandbox unavailable");
        return;
    }
    let tier = linux_git_protection();
    if let GitProtection::Basic { reason } = &tier {
        assert!(!reason.is_empty());
    }
    match (expected_tier().as_deref(), &tier) {
        (Some("full"), _) => assert_eq!(tier, GitProtection::Full),
        (Some("basic"), GitProtection::Basic { reason }) => assert!(
            reason.contains(" failed: "),
            "the basic tier here should come from a setup step the kernel refused: {reason}"
        ),
        (Some("basic"), GitProtection::Full) => {
            panic!("HARNESS_EXPECT_LINUX_TIER=basic, but the probe picked the full tier")
        }
        _ => eprintln!("probe picked {tier:?}"),
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
    // The watcher between commands may have moved it already.
    wait_until("the background process planted commondir", || {
        env.exists(".git/commondir") || env.in_quarantine(".git/commondir")
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
// The watcher
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_watcher_quarantines_a_hook_while_the_command_runs() {
    let _serial = SERIAL.lock().await;
    let Some(env) = Env::new() else { return };
    let sandbox = env.basic();
    let prepared = sandbox
        .prepare(
            FsAccess::WorkspaceWrite,
            &env.ws,
            "/bin/sh",
            &[
                "-c",
                "echo 'echo pwned' > .git/hooks/post-checkout; exec sleep 30",
            ],
        )
        .expect("prepare the sandboxed command");
    let mut guard = prepared.guard.expect("a workspace-write guard");
    let mut cmd = prepared.command;
    cmd.current_dir(&env.ws)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = cmd.spawn().expect("spawn the sandboxed command");
    guard.started(child.id().expect("a pid"));
    env.wait_for_quarantine(".git/hooks/post-checkout").await;
    assert!(
        child.try_wait().unwrap().is_none(),
        "the command should still be running"
    );
    assert!(!env.exists(".git/hooks/post-checkout"));
    child.kill().await.unwrap();
    let report = guard.finish().expect("a report");
    assert!(report.blocked, "{}", report.message);
    assert!(
        report.message.contains("- .git/hooks/post-checkout: "),
        "{}",
        report.message
    );
    assert_eq!(
        std::fs::read_to_string(env.quarantined(".git/hooks/post-checkout")).unwrap(),
        "echo pwned\n"
    );
}

#[tokio::test]
async fn a_hook_planted_again_and_again_gets_a_bounded_report() {
    let _serial = SERIAL.lock().await;
    let Some(env) = Env::new() else { return };
    let started = Instant::now();
    let (output, report) = run(
        &env.basic(),
        &env,
        "i=0; while [ $i -lt 100 ]; do echo 'echo pwned' > .git/hooks/post-checkout; \
         i=$((i + 1)); sleep 0.02; done",
    )
    .await;
    let took = started.elapsed();
    assert!(output.status.success(), "{}", stderr(&output));
    let report = report.expect("a report");
    assert!(report.blocked, "{}", report.message);
    // One line, however often it was moved: where the first and the last went, and a count.
    assert_eq!(
        report.message.matches("\n- ").count(),
        1,
        "{}",
        report.message
    );
    assert!(
        report
            .message
            .contains("\n- .git/hooks/post-checkout: new in a protected directory; moved to "),
        "{}",
        report.message
    );
    let again: usize = report
        .message
        .split(", and ")
        .nth(1)
        .and_then(|rest| rest.split(' ').next())
        .map_or(0, |count| count.parse().unwrap());
    // Each check moves the hook planted last: one as the command starts planting, then one at
    // most every 50 ms, one each two-second tick, and the final one after it ends.
    let found = 1 + again;
    let millis = took.as_millis();
    let most = usize::try_from(millis / 50 + millis / 2000).unwrap() + 3;
    assert!(
        (2..=most).contains(&found),
        "{found} found in {took:?}, at most {most}: {}",
        report.message
    );
    assert!(report.message.len() < 1024, "{}", report.message);
}

#[tokio::test]
async fn a_protected_file_written_through_a_hard_link_is_restored_while_the_command_runs() {
    let _serial = SERIAL.lock().await;
    let Some(env) = Env::new() else { return };
    let before = env.read(".git/config");
    let sandbox = env.basic();
    let prepared = sandbox
        .prepare(
            FsAccess::WorkspaceWrite,
            &env.ws,
            "/bin/sh",
            &[
                "-c",
                "ln .git/config .git/x && echo '# evil' >> .git/x && exec sleep 30",
            ],
        )
        .expect("prepare the sandboxed command");
    let mut guard = prepared.guard.expect("a workspace-write guard");
    let mut cmd = prepared.command;
    cmd.current_dir(&env.ws)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = cmd.spawn().expect("spawn the sandboxed command");
    guard.started(child.id().expect("a pid"));
    // No event names the config: the watcher's tick finds it.
    wait_until("the config is restored while the command runs", || {
        env.read_now(".git/config").as_ref() == Some(&before)
            && env.read_now(".git/x").is_some_and(|x| x.contains("# evil"))
    })
    .await;
    assert!(
        child.try_wait().unwrap().is_none(),
        "the command should still be running"
    );
    child.kill().await.unwrap();
    let report = guard.finish().expect("a report");
    assert!(report.blocked, "{}", report.message);
    assert!(
        report
            .message
            .contains("- .git/config: changed; restored the earlier version"),
        "{}",
        report.message
    );
}

// ---------------------------------------------------------------------------
// Processes a command leaves running (the basic tier)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_job_left_running_is_undone_between_commands_and_reported_with_the_next() {
    let _serial = SERIAL.lock().await;
    let Some(env) = Env::new() else { return };
    let sandbox = env.basic();
    let before = env.read(".git/config");
    let (output, report) = run(
        &sandbox,
        &env,
        r##"sh -c 'echo $$ > job.pid; sleep 1; echo "echo pwned" > .git/hooks/post-checkout; echo "# evil" >> .git/config; exec sleep 30' > /dev/null 2>&1 &"##,
    )
    .await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(report, None);
    let job = Job::from_pid_file(&env, "job.pid").await;
    // No command runs meanwhile.
    wait_until(
        "the watcher between commands undid what the job did",
        || {
            env.in_quarantine(".git/hooks/post-checkout")
                && env.in_quarantine(".git/config")
                && !env.exists(".git/hooks/post-checkout")
                && env.read_now(".git/config").as_ref() == Some(&before)
        },
    )
    .await;
    assert!(job.alive(), "undone while the job still ran");
    drop(job);
    let (output, report) = run(&sandbox, &env, "true").await;
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
            .contains("- .git/hooks/post-checkout: new in a protected directory; moved to "),
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
async fn the_watcher_between_commands_ends_once_the_processes_left_are_gone() {
    let _serial = SERIAL.lock().await;
    let Some(env) = Env::new() else { return };
    let sandbox = env.basic();
    let (output, report) = run(&sandbox, &env, "(sleep 1) > /dev/null 2>&1 &").await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(report, None);
    assert_eq!(
        threads_named("harness-watch"),
        0,
        "the command's watcher stopped"
    );
    wait_until(
        "a watcher runs between commands while the job lives",
        || threads_named("harness-between") == 1,
    )
    .await;
    wait_until("the watcher between commands ended with the job", || {
        threads_named("harness-between") == 0
    })
    .await;
    assert_eq!(
        orphaned_zombies(),
        Vec::<i32>::new(),
        "its last look reaped the job"
    );
    // Nothing the command left is running: what changes now is the user's.
    env.append(".git/config", "# mine\n");
    let (_, report) = run(&sandbox, &env, "true").await;
    assert_eq!(report, None);
    assert!(env.read(".git/config").ends_with("# mine\n"));
}

#[tokio::test]
async fn a_protected_file_written_through_a_hard_link_between_commands_is_restored_within_two_ticks()
 {
    let _serial = SERIAL.lock().await;
    let Some(env) = Env::new() else { return };
    let sandbox = env.basic();
    let before = env.read(".git/config");
    let (output, report) = run(
        &sandbox,
        &env,
        r##"sh -c 'echo $$ > job.pid; sleep 1; ln .git/config .git/x && echo "# evil" >> .git/x; exec sleep 30' > /dev/null 2>&1 &"##,
    )
    .await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(report, None);
    let job = Job::from_pid_file(&env, "job.pid").await;
    wait_until("the job wrote through the link", || {
        env.read_now(".git/x").is_some_and(|x| x.contains("# evil"))
    })
    .await;
    let written = Instant::now();
    // No event names the config: the watcher's tick finds it.
    wait_until("the config is restored", || {
        env.read_now(".git/config").as_ref() == Some(&before)
    })
    .await;
    // Two ticks of two seconds, and some slack.
    assert!(
        written.elapsed() < Duration::from_secs(5),
        "restored after {:?}",
        written.elapsed()
    );
    assert!(job.alive(), "undone while the job still ran");
    drop(job);
    let (_, report) = run(&sandbox, &env, "true").await;
    let report = report.expect("a report");
    assert!(!report.blocked, "{}", report.message);
    assert!(
        report
            .message
            .contains("- .git/config: changed; restored the earlier version"),
        "{}",
        report.message
    );
}

/// Runs `script` read-only through `sandbox.prepare`, as the bash tool does, and gives its
/// guard's report.
async fn run_read_only(sandbox: &LinuxSandbox, env: &Env, script: &str) -> Option<GuardReport> {
    let prepared = sandbox
        .prepare(FsAccess::ReadOnly, &env.ws, "/bin/sh", &["-c", script])
        .expect("prepare the sandboxed command");
    let mut guard = prepared.guard.expect("a guard that registers the command");
    let mut cmd = prepared.command;
    cmd.current_dir(&env.ws)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = cmd.spawn().expect("spawn the sandboxed command");
    guard.started(child.id().expect("a pid"));
    child.wait().await.expect("wait for the command");
    guard.finish()
}

#[tokio::test]
async fn what_the_watcher_between_commands_found_is_said_once_with_a_read_only_command() {
    let _serial = SERIAL.lock().await;
    let Some(env) = Env::new() else { return };
    let sandbox = env.basic();
    let (output, report) = run(
        &sandbox,
        &env,
        "sh -c 'echo $$ > job.pid; sleep 1; printf /tmp/elsewhere > .git/commondir; \
         exec sleep 30' > /dev/null 2>&1 &",
    )
    .await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(report, None);
    let job = Job::from_pid_file(&env, "job.pid").await;
    wait_until("the watcher between commands moved commondir", || {
        env.in_quarantine(".git/commondir")
    })
    .await;
    let report = run_read_only(&sandbox, &env, "true")
        .await
        .expect("a report");
    assert!(!report.blocked, "{}", report.message);
    assert!(
        report.message.starts_with("[before this command ran"),
        "{}",
        report.message
    );
    assert!(
        report.message.contains("- .git/commondir: new; moved to "),
        "{}",
        report.message
    );
    drop(job);
    let (_, report) = run(&sandbox, &env, "true").await;
    assert!(
        report
            .as_ref()
            .is_none_or(|report| !report.message.contains(".git/commondir")),
        "said again: {report:?}"
    );
}

/// Runs `script`, which leaves a process running that appends `# evil` to `.git/config` a
/// second after the command ends, and checks the config is put back, by the watcher between
/// commands or before the next command at the latest, and that the next command reports it.
async fn a_config_rewrite_after_the_command_is_undone_before_the_next(env: &Env, script: &str) {
    let sandbox = env.basic();
    let before = env.read(".git/config");
    let (output, report) = run(&sandbox, env, script).await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(report, None);
    wait_until("the process left running rewrote the config", || {
        env.read_now(".git/config")
            .is_some_and(|config| config.contains("# evil"))
            || env.in_quarantine(".git/config")
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
    // Its parent exited at once: it was reparented to this process.
    wait_until("the detached job is a descendant of this process", || {
        !live_descendants_elsewhere().is_empty()
    })
    .await;
    wait_until("the detached job ran", || env.exists("done")).await;
    wait_until("nothing the command left is still running", || {
        live_descendants_elsewhere().is_empty()
    })
    .await;
    // The watcher between commands may have reaped it already; the next command does if not.
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

// ---------------------------------------------------------------------------
// The full tier: writes fail
// ---------------------------------------------------------------------------

#[tokio::test]
async fn full_tier_refuses_to_plant_a_hook() {
    let _serial = SERIAL.lock().await;
    let Some(env) = Env::new() else { return };
    let Some(sandbox) = env.full() else { return };
    let (output, report) = run(&sandbox, &env, "echo 'echo pwned' > .git/hooks/pre-commit").await;
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("Read-only file system"),
        "{}",
        stderr(&output)
    );
    assert!(!env.exists(".git/hooks/pre-commit"));
    assert_eq!(report, None);
    assert!(sandbox.is_denial(output.status.code(), &stderr(&output)));
}

#[tokio::test]
async fn full_tier_refuses_git_config() {
    let _serial = SERIAL.lock().await;
    let Some(env) = Env::new() else { return };
    let Some(sandbox) = env.full() else { return };
    let before = std::fs::read(env.ws.join(".git/config")).unwrap();
    let (output, _) = run(&sandbox, &env, "git config user.name evil").await;
    assert!(!output.status.success());
    assert_eq!(std::fs::read(env.ws.join(".git/config")).unwrap(), before);
}

#[tokio::test]
async fn full_tier_pins_dot_git() {
    let _serial = SERIAL.lock().await;
    let Some(env) = Env::new() else { return };
    let Some(sandbox) = env.full() else { return };
    let (output, _) = run(&sandbox, &env, "mv .git moved").await;
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("Device or resource busy"),
        "{}",
        stderr(&output)
    );
    assert!(env.ws.join(".git").is_dir());
    assert!(!env.exists("moved"));
    assert!(sandbox.is_denial(output.status.code(), &stderr(&output)));
}

#[tokio::test]
async fn full_tier_refuses_hard_links_across_its_mounts() {
    let _serial = SERIAL.lock().await;
    let Some(env) = Env::new() else { return };
    let Some(sandbox) = env.full() else { return };
    let (output, _) = run(&sandbox, &env, "ln .git/config alias").await;
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("Invalid cross-device link"),
        "{}",
        stderr(&output)
    );
    assert!(!env.exists("alias"));
}

#[tokio::test]
async fn full_tier_protects_nested_repositories_harness_and_head() {
    let _serial = SERIAL.lock().await;
    let Some(env) = Env::new() else { return };
    let Some(sandbox) = env.full() else { return };
    assert!(env.git(&["init", "-q", "sub"]).status.success());
    std::fs::create_dir(env.ws.join(".harness")).unwrap();
    std::fs::write(env.ws.join(".harness/config.toml"), "mode = \"ask\"\n").unwrap();
    std::fs::write(env.ws.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    for target in ["sub/.git/config", ".harness/config.toml", "HEAD"] {
        let before = std::fs::read(env.ws.join(target)).unwrap();
        let (output, _) = run(&sandbox, &env, &format!("echo evil >> {target}")).await;
        assert!(!output.status.success(), "{target}");
        assert!(
            stderr(&output).contains("Read-only file system"),
            "{target}: {}",
            stderr(&output)
        );
        assert_eq!(
            std::fs::read(env.ws.join(target)).unwrap(),
            before,
            "{target}"
        );
    }
}

#[tokio::test]
async fn full_tier_pins_the_way_to_a_gitdir_a_gitfile_names() {
    let _serial = SERIAL.lock().await;
    let Some(env) = Env::new() else { return };
    let Some(sandbox) = env.full() else { return };
    // A repository whose gitdir is kept apart, in `.seps/one`, named by the gitfile `one/.git`.
    // Git makes the gitdir, but not the directory it goes in.
    std::fs::create_dir(env.ws.join(".seps")).unwrap();
    let separate = env.ws.join(".seps/one");
    env.git_ok(&[
        "init",
        "-q",
        "--separate-git-dir",
        separate.to_str().unwrap(),
        "one",
    ]);
    // A submodule's gitdir in `.git/modules`, named by the gitfile `sub/.git`.
    let module = env.ws.join(".git/modules/sub");
    std::fs::create_dir_all(module.join("refs")).unwrap();
    std::fs::create_dir_all(module.join("objects")).unwrap();
    std::fs::write(module.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::write(module.join("config"), "[core]\n").unwrap();
    std::fs::create_dir(env.ws.join("sub")).unwrap();
    std::fs::write(env.ws.join("sub/.git"), "gitdir: ../.git/modules/sub\n").unwrap();
    // Moving a directory on the way would let a new gitdir take the path a gitfile names.
    for (script, error) in [
        ("mv .seps elsewhere", "Device or resource busy"),
        ("mv .git/modules .git/elsewhere", "Device or resource busy"),
        ("mv .seps/one .seps/two", "Device or resource busy"),
        ("echo evil >> one/.git", "Read-only file system"),
        ("echo evil >> sub/.git", "Read-only file system"),
        ("echo evil >> .seps/one/config", "Read-only file system"),
        (
            "echo evil >> .git/modules/sub/config",
            "Read-only file system",
        ),
    ] {
        let (output, _) = run(&sandbox, &env, script).await;
        assert!(!output.status.success(), "{script}");
        assert!(
            stderr(&output).contains(error),
            "{script}: {}",
            stderr(&output)
        );
    }
    assert!(env.ws.join(".seps/one").is_dir());
    assert!(env.ws.join(".git/modules/sub").is_dir());
    assert!(!env.read("one/.git").contains("evil"));
    assert!(!env.read("sub/.git").contains("evil"));
}

#[tokio::test]
async fn full_tier_covers_a_missing_hooks_directory_with_an_empty_one() {
    let _serial = SERIAL.lock().await;
    let Some(env) = Env::new() else { return };
    let Some(sandbox) = env.full() else { return };
    std::fs::remove_dir_all(env.ws.join(".git/hooks")).unwrap();
    let (output, report) = run(&sandbox, &env, "echo 'echo pwned' > .git/hooks/pre-commit").await;
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("Read-only file system"),
        "{}",
        stderr(&output)
    );
    assert_eq!(
        std::fs::read_dir(env.ws.join(".git/hooks"))
            .unwrap()
            .count(),
        0
    );
    assert_eq!(report, None);
}

#[tokio::test]
async fn full_tier_allows_commit_checkout_and_stash() {
    let _serial = SERIAL.lock().await;
    let Some(env) = Env::new() else { return };
    let Some(sandbox) = env.full() else { return };
    let script = "set -e; git commit -q --allow-empty -m one; git checkout -q -b topic; \
                  echo x > f; git add f; git stash -q; git stash pop -q";
    let (output, report) = run(&sandbox, &env, script).await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(report, None);
    let log = env.git(&["log", "--oneline"]);
    assert!(String::from_utf8_lossy(&log.stdout).contains("one"));
}

#[tokio::test]
async fn full_tier_quarantines_a_new_commondir_while_the_command_runs() {
    // `commondir` cannot have a placeholder (git refuses an empty one), so the guard and its
    // watcher handle it.
    let _serial = SERIAL.lock().await;
    let Some(env) = Env::new() else { return };
    let Some(sandbox) = env.full() else { return };
    let prepared = sandbox
        .prepare(
            FsAccess::WorkspaceWrite,
            &env.ws,
            "/bin/sh",
            &[
                "-c",
                "printf /tmp/elsewhere > .git/commondir; exec sleep 30",
            ],
        )
        .expect("prepare the sandboxed command");
    let mut guard = prepared.guard.expect("a workspace-write guard");
    let mut cmd = prepared.command;
    cmd.current_dir(&env.ws)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = cmd.spawn().expect("spawn the sandboxed command");
    guard.started(child.id().expect("a pid"));
    env.wait_for_quarantine(".git/commondir").await;
    assert!(
        child.try_wait().unwrap().is_none(),
        "the command should still be running"
    );
    assert!(!env.exists(".git/commondir"));
    child.kill().await.unwrap();
    let report = guard.finish().expect("a report");
    assert!(report.blocked, "{}", report.message);
    assert!(
        report.message.contains("- .git/commondir: "),
        "{}",
        report.message
    );
}

#[tokio::test]
async fn a_working_directory_inside_dot_git_still_sees_the_mounts() {
    let _serial = SERIAL.lock().await;
    let Some(env) = Env::new() else { return };
    let Some(sandbox) = env.full() else { return };
    let (output, _) = run_in(
        &sandbox,
        &env.ws,
        &env.ws.join(".git"),
        "echo x > hooks/pre-commit",
    )
    .await;
    let output = output.unwrap();
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("Read-only file system"),
        "{}",
        stderr(&output)
    );
    assert!(!env.exists(".git/hooks/pre-commit"));
}

#[tokio::test]
async fn full_tier_mounts_what_an_incomplete_scan_found_and_says_it_was_incomplete() {
    let _serial = SERIAL.lock().await;
    let Some(env) = Env::new() else { return };
    // An ignore file that is a symlink cannot be used as git would use it: no scan is complete.
    std::os::unix::fs::symlink("elsewhere", env.ws.join(".gitignore")).unwrap();
    let Some(sandbox) = env.full() else { return };
    let (output, report) = run(&sandbox, &env, "echo 'echo pwned' > .git/hooks/pre-commit").await;
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("Read-only file system"),
        "{}",
        stderr(&output)
    );
    assert!(!env.exists(".git/hooks/pre-commit"));
    let report = report.expect("a report");
    assert!(!report.blocked, "{}", report.message);
    assert!(
        report
            .message
            .contains("harness could not scan the whole workspace"),
        "{}",
        report.message
    );
    assert_eq!(sandbox.git_protection(), GitProtection::Full);
}

#[tokio::test]
async fn full_tier_uses_a_namespace_only_where_there_is_something_to_protect() {
    let _serial = SERIAL.lock().await;
    let Some(env) = Env::new() else { return };
    let Some(sandbox) = env.full() else { return };
    let ours = std::fs::read_link("/proc/self/ns/mnt").unwrap();
    let theirs = |output: &Output| PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    std::fs::remove_dir_all(env.ws.join(".git")).unwrap();
    let (output, _) = run(&sandbox, &env, "readlink /proc/self/ns/mnt").await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(theirs(&output), ours, "nothing to protect");
    assert!(env.git(&["init", "-q"]).status.success());
    let (output, _) = run(&sandbox, &env, "readlink /proc/self/ns/mnt").await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_ne!(theirs(&output), ours, "a repository to protect");
}

#[tokio::test]
async fn full_tier_refuses_umount_and_the_harness_process_root() {
    let _serial = SERIAL.lock().await;
    let Some(env) = Env::new() else { return };
    let Some(sandbox) = env.full() else { return };
    if !have("python3") {
        return;
    }
    // `exec` makes python the direct child, so `getppid()` is this test process: outside the
    // sandbox, where `/proc/<pid>/root` would show `.git/config` without the mounts.
    let script = format!(
        r#"exec python3 -c '
import ctypes, errno, os, sys
libc = ctypes.CDLL(None, use_errno=True)
if libc.umount2(b".git/config", 0) != -1 or ctypes.get_errno() != errno.EPERM:
    sys.exit("umount2: expected EPERM, got %d" % ctypes.get_errno())
try:
    open("/proc/%d/root{}/.git/config" % os.getppid(), "a")
    sys.exit("opened .git/config through /proc/<pid>/root")
except PermissionError as e:
    if e.errno != errno.EACCES:
        sys.exit("/proc/<pid>/root: expected EACCES, got %d" % e.errno)
'"#,
        env.ws.display()
    );
    let (output, _) = run(&sandbox, &env, &script).await;
    assert!(output.status.success(), "{}", stderr(&output));
}

/// The namespace and mounts are set up before seccomp is installed, which then refuses to make
/// more. Task 5's tests (`linux_sandbox.rs`) prove the filter refuses the mount calls; this one
/// proves it still refuses `unshare` and `setns`, and the mount calls, in a child that set up a
/// namespace of its own and held every capability in it until just before.
#[tokio::test]
async fn full_tier_commands_cannot_make_namespaces_or_mounts_of_their_own() {
    let _serial = SERIAL.lock().await;
    let Some(env) = Env::new() else { return };
    let Some(sandbox) = env.full() else { return };
    if !have("python3") {
        return;
    }
    let script = r#"exec python3 -c '
import ctypes, errno, platform, sys
libc = ctypes.CDLL(None, use_errno=True)
x86 = platform.machine() == "x86_64"
AT_FDCWD = -100
calls = [
    ("unshare", 272 if x86 else 97, (0x10000000 | 0x00020000,)),
    ("setns", 308 if x86 else 268, (-1, 0)),
    ("open_tree", 428, (AT_FDCWD, b".git/hooks", 1)),
    ("move_mount", 429, (AT_FDCWD, b".git/hooks", AT_FDCWD, b".git/hooks", 0)),
    ("mount_setattr", 442, (AT_FDCWD, b".git/hooks", 0, None, 0)),
    ("mount", 165 if x86 else 40, (b".git", b".git/hooks", None, 4096, None)),
]
for name, nr, args in calls:
    rc = libc.syscall(nr, *args)
    err = ctypes.get_errno()
    if rc != -1 or err != errno.EPERM:
        sys.exit("%s: expected EPERM, got rc=%d errno=%d" % (name, rc, err))
'"#;
    let (output, _) = run(&sandbox, &env, script).await;
    assert!(output.status.success(), "{}", stderr(&output));
}

/// Prepares `script` in the workspace with `sandbox`, runs `between` (which may change the
/// workspace between the plan and the setup), and spawns the command as the bash tool does.
/// Says whether it ran, and gives its guard's report.
async fn spawn_after(
    sandbox: &LinuxSandbox,
    env: &Env,
    script: &str,
    between: impl FnOnce(),
) -> (bool, Option<GuardReport>) {
    let prepared = sandbox
        .prepare(
            FsAccess::WorkspaceWrite,
            &env.ws,
            "/bin/sh",
            &["-c", script],
        )
        .expect("prepare the sandboxed command");
    between();
    let mut guard = prepared.guard.expect("a workspace-write guard");
    let mut cmd = prepared.command;
    cmd.current_dir(&env.ws)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let ran = match cmd.spawn() {
        Ok(mut child) => {
            guard.started(child.id().expect("a pid"));
            let _ = child.wait().await;
            true
        }
        Err(_) => false,
    };
    (ran, guard.finish())
}

/// Where user namespaces are blocked, a full-tier sandbox's setup is refused at `unshare` or the
/// id maps: the command does not run, and the session goes on in the basic tier.
#[tokio::test]
async fn a_refused_mount_setup_drops_the_session_to_the_basic_tier() {
    let _serial = SERIAL.lock().await;
    let Some(env) = Env::new() else { return };
    if let GitProtection::Full = linux_git_protection() {
        assert!(
            expected_tier().as_deref() != Some("basic"),
            "HARNESS_EXPECT_LINUX_TIER=basic, but the probe picked the full tier"
        );
        eprintln!("skipping: user namespaces work here, so nothing refuses the setup");
        return;
    }
    let sandbox = LinuxSandbox::with_git_protection(env.settings(), GitProtection::Full);
    sandbox.start_session(&env.ws);
    let (ran, report) = spawn_after(&sandbox, &env, "touch ran", || {}).await;
    assert!(!ran, "the command must not run without its mounts");
    assert!(!env.exists("ran"));
    let report = report.expect("a report");
    assert!(!report.blocked, "{}", report.message);
    assert!(
        report
            .message
            .contains("could not set up its read-only mounts"),
        "{}",
        report.message
    );
    assert!(
        report.message.contains("now uses the basic tier"),
        "{}",
        report.message
    );
    match sandbox.git_protection() {
        GitProtection::Basic { reason } => {
            assert!(reason.contains("failed during the session"), "{reason}")
        }
        GitProtection::Full => panic!("the session should have dropped to the basic tier"),
    }
    assert!(
        subreaper_active(),
        "harness tracks the processes commands leave running"
    );
    // The next command runs, in the basic tier.
    let (output, report) = run(&sandbox, &env, "touch ran").await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(report, None);
    assert!(env.exists("ran"));
    // And its guard puts a changed config back, as the basic tier's does.
    let config = env.ws.join(".git/config");
    let before = std::fs::read(&config).unwrap();
    let (output, report) = run(&sandbox, &env, "git config user.name evil").await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(report.expect("a report").blocked);
    assert_eq!(std::fs::read(&config).unwrap(), before);
    let changed = std::fs::read_to_string(env.quarantined(".git/config")).unwrap();
    assert!(changed.contains("evil"), "{changed}");
}

/// Where user namespaces work, an entry replaced between the plan and the setup fails the
/// child's identity check: the command does not run, and the session keeps the full tier, so
/// nothing outside a command can downgrade it.
#[tokio::test]
async fn an_entry_replaced_before_the_setup_stops_the_command_and_keeps_the_full_tier() {
    let _serial = SERIAL.lock().await;
    let Some(env) = Env::new() else { return };
    let Some(sandbox) = env.full() else { return };
    let config = env.ws.join(".git/config");
    let (ran, report) = spawn_after(&sandbox, &env, "touch ran", || {
        std::fs::copy(&config, env.ws.join(".git/config.new")).unwrap();
        std::fs::rename(env.ws.join(".git/config.new"), &config).unwrap();
    })
    .await;
    assert!(!ran, "the command must not run without its mounts");
    assert!(!env.exists("ran"));
    let report = report.expect("a report");
    assert!(!report.blocked, "{}", report.message);
    assert!(
        report.message.contains("checking that nothing replaced ")
            && report.message.contains("Run the command again"),
        "{}",
        report.message
    );
    assert!(!report.message.contains("basic tier"), "{}", report.message);
    assert_eq!(sandbox.git_protection(), GitProtection::Full);
    // The next command runs, under its mounts.
    let (output, _) = run(&sandbox, &env, "echo 'echo pwned' > .git/hooks/pre-commit").await;
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("Read-only file system"),
        "{}",
        stderr(&output)
    );
    let (output, _) = run(&sandbox, &env, "touch ran").await;
    assert!(output.status.success(), "{}", stderr(&output));
}

/// A process a full-tier command leaves running keeps that command's namespace, where a gitdir
/// added later has no mounts. While it runs, the full tier saves every protected file, as the
/// basic tier does, so what it changes there is undone before the next command.
#[tokio::test]
async fn a_process_a_full_tier_command_leaves_cannot_keep_a_change_to_a_gitdir_added_later() {
    let _serial = SERIAL.lock().await;
    let Some(env) = Env::new() else { return };
    let Some(sandbox) = env.full() else { return };
    assert!(
        subreaper_active(),
        "harness tracks the processes commands leave running in the full tier too"
    );
    let (output, report) = run(
        &sandbox,
        &env,
        "(while [ ! -e go ]; do sleep 0.05; done; \
          printf /tmp/elsewhere > .git/worktrees/w/commondir; touch done) > /dev/null 2>&1 &",
    )
    .await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(report, None);
    // The user adds a linked worktree's gitdir, outside harness.
    let worktree = env.ws.join(".git/worktrees/w");
    std::fs::create_dir_all(&worktree).unwrap();
    std::fs::write(worktree.join("HEAD"), "ref: refs/heads/w\n").unwrap();
    std::fs::write(worktree.join("commondir"), "../..\n").unwrap();
    std::fs::write(worktree.join("gitdir"), "/elsewhere/w/.git\n").unwrap();
    // A command runs while the process lives.
    let (output, report) = run(&sandbox, &env, "true").await;
    assert!(output.status.success(), "{}", stderr(&output));
    if let Some(report) = &report {
        assert!(!report.blocked, "{}", report.message);
    }
    // Then the process rewrites the new gitdir's `commondir`, which its namespace does not cover.
    std::fs::write(env.ws.join("go"), "").unwrap();
    wait_until("the process left running wrote commondir", || {
        env.exists("done")
    })
    .await;
    let (output, report) = run(&sandbox, &env, "true").await;
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
            .contains("- .git/worktrees/w/commondir: changed; restored the earlier version"),
        "{}",
        report.message
    );
    assert_eq!(env.read(".git/worktrees/w/commondir"), "../..\n");
    let changed = std::fs::read_to_string(env.quarantined(".git/worktrees/w/commondir")).unwrap();
    assert_eq!(changed, "/tmp/elsewhere");
}

/// After its mounts, the child locks the securebits and drops every capability it held in its
/// user namespace, so nothing the command runs can regain one, even where harness runs as root.
#[tokio::test]
async fn full_tier_commands_hold_no_capability_and_cannot_regain_one() {
    let _serial = SERIAL.lock().await;
    let Some(env) = Env::new() else { return };
    let Some(sandbox) = env.full() else { return };
    if !have("python3") {
        return;
    }
    let script = r#"exec python3 -c '
import ctypes, sys
libc = ctypes.CDLL(None, use_errno=True)
bits = libc.prctl(27, 0, 0, 0, 0)
if bits != 0xef:
    sys.exit("securebits: expected 0xef, got %#x" % bits)
for line in open("/proc/self/status"):
    name, _, value = line.partition(":")
    if name in ("CapInh", "CapPrm", "CapEff", "CapAmb") and int(value, 16):
        sys.exit("%s is %s" % (name, value.strip()))
'"#;
    let (output, _) = run(&sandbox, &env, script).await;
    assert!(output.status.success(), "{}", stderr(&output));
}
