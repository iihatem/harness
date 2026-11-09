//! A SIGTERM or SIGHUP sent to `harness ask` stops the program a delegated model runs in (and
//! what the program started) as Ctrl+C does, and `ask` exits with the conventional code. The
//! transport crate's scripted fake child stands in for `claude`; no live program is run.
mod common;
use common::Isolate;

use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Once,
    time::{Duration, Instant},
};

use nix::{sys::signal::Signal, unistd::Pid};

const BIN: &str = env!("CARGO_BIN_EXE_harness");

fn fake_child() -> PathBuf {
    static BUILD: Once = Once::new();
    let path = Path::new(BIN).parent().unwrap().join("fake-delegate-child");
    BUILD.call_once(|| {
        if !path.exists() {
            let status = std::process::Command::new(env!("CARGO"))
                .args([
                    "build",
                    "-p",
                    "harness-delegate",
                    "--bin",
                    "fake-delegate-child",
                ])
                .status()
                .unwrap();
            assert!(status.success());
        }
    });
    path
}

const INIT: &str = r#"{"type":"system","subtype":"init","apiKeySource":"ANTHROPIC_API_KEY","claude_code_version":"2.1.312","model":"claude-x","session_id":"sess-9"}"#;

fn alive(pid: i32) -> bool {
    nix::sys::signal::kill(Pid::from_raw(pid), None).is_ok()
}

/// Runs `harness ask` on a fake `claude` that starts a grandchild and then hangs, sends `signal`
/// to harness once the grandchild is there, and checks harness's exit code and that nothing the
/// program started is left.
fn stopped_by(signal: Signal, code: i32) {
    let home = tempfile::Builder::new()
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap();
    let ws = tempfile::tempdir().unwrap();
    let tools = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("config")).unwrap();
    std::fs::create_dir(ws.path().join(".git")).unwrap();
    let pidfile = tools.path().join("grandchild.pid");
    let script = tools.path().join("claude.script");
    std::fs::write(
        &script,
        format!(
            "expect user\nemit {INIT}\nspawn-grandchild {}\nhang\n",
            pidfile.display()
        ),
    )
    .unwrap();
    let help = tools.path().join("claude.help");
    std::fs::write(&help, "Options:\n  --model <model>  alias\n").unwrap();
    let binary = tools.path().join("claude");
    std::fs::write(
        &binary,
        format!(
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo '2.1.312 (Claude Code)'; exit 0; fi\n\
             if [ \"$1\" = \"--help\" ]; then cat '{}'; exit 0; fi\n\
             if [ \"$1\" = \"mcp\" ]; then echo '[]'; exit 0; fi\n\
             FAKE_CHILD_SCRIPT='{}' exec '{}' \"$@\"\n",
            help.display(),
            script.display(),
            fake_child().display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();

    let mut harness = std::process::Command::new(BIN)
        .args(["--model", "claude-code/sonnet", "ask", "hi"])
        .current_dir(ws.path())
        .env("HARNESS_HOME", home.path())
        .env("HARNESS_CLAUDE_BIN", &binary)
        .isolate()
        .env("ANTHROPIC_API_KEY", "sk-ant-api03-test")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(tools.path().join("stderr")).unwrap())
        .spawn()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(20);
    let grandchild: i32 = loop {
        if let Some(pid) = std::fs::read_to_string(&pidfile)
            .ok()
            .and_then(|t| t.trim().parse().ok())
        {
            break pid;
        }
        if Instant::now() >= deadline {
            let _ = harness.kill();
            let said = std::fs::read_to_string(tools.path().join("stderr")).unwrap_or_default();
            panic!("the program never started its grandchild: {said}");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(alive(grandchild));
    nix::sys::signal::kill(Pid::from_raw(harness.id() as i32), signal).unwrap();

    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = harness.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = harness.kill();
            let _ = nix::sys::signal::kill(Pid::from_raw(grandchild), Signal::SIGKILL);
            panic!("harness did not end after {signal}");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(status.code(), Some(code), "{status:?}");
    // The grandchild is reaped by init after its group was killed: give that a moment.
    let deadline = Instant::now() + Duration::from_secs(10);
    while alive(grandchild) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    let left = alive(grandchild);
    if left {
        let _ = nix::sys::signal::kill(Pid::from_raw(grandchild), Signal::SIGKILL);
    }
    assert!(!left, "the program's grandchild outlived harness");
}

#[test]
fn sigterm_to_ask_stops_the_delegated_program_and_exits_143() {
    stopped_by(Signal::SIGTERM, 143);
}

#[test]
fn sighup_to_ask_stops_the_delegated_program_and_exits_129() {
    stopped_by(Signal::SIGHUP, 129);
}
