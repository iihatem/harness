//! The commands the documentation gives run as written.

use std::process::Command;

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");

fn xtask(args: &[&str]) -> std::process::Output {
    Command::new(XTASK).args(args).output().unwrap()
}

#[test]
fn the_documented_replay_command_runs_and_reports_every_task_applied() {
    let out = xtask(&["eval", "replay"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("30 tasks x 4 formats: 120 applied, 0 failed"),
        "{text}"
    );
}

#[test]
fn the_documented_live_command_takes_the_documented_flags() {
    let out = xtask(&["eval", "run", "--help"]);
    let text = String::from_utf8_lossy(&out.stdout);
    for flag in ["--model", "--format", "-n", "--task", "--save", "--timeout"] {
        assert!(text.contains(flag), "{flag}: {text}");
    }
}

#[test]
fn an_unknown_edit_format_is_refused_naming_the_four() {
    let out = xtask(&["eval", "run", "--model", "a/b", "--format", "diff"]);
    assert!(!out.status.success());
    let text = String::from_utf8_lossy(&out.stderr);
    assert!(
        text.contains("apply_patch") && text.contains("hashline"),
        "{text}"
    );
}
