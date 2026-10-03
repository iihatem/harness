//! Reading what a gate command left, and cutting it to a tail.

use harness_core::{
    event::GateStatus,
    gate::{outcome, tail},
    tool::ToolOutput,
};

#[test]
fn a_tail_keeps_the_last_lines_and_counts_the_rest() {
    assert_eq!(tail("a\nb\nc\nd\n", 2), ("c\nd\n".to_string(), 2));
    assert_eq!(tail("a\nb", 5), ("a\nb\n".to_string(), 0));
    assert_eq!(tail("", 3), (String::new(), 0));
    assert_eq!(tail("only", 1), ("only\n".to_string(), 0));
}

#[test]
fn exit_codes_and_output_are_read_from_the_bash_result() {
    let failed = outcome(&ToolOutput::error("exit code 101\nerror[E0308]\n"));
    assert_eq!(
        (failed.status, failed.exit_code, failed.output.as_str()),
        (GateStatus::Failed, Some(101), "error[E0308]\n")
    );
    let passed = outcome(&ToolOutput::ok("exit code 0\nok\n"));
    assert_eq!(
        (passed.status, passed.exit_code),
        (GateStatus::Passed, Some(0))
    );
    // Killed by a signal: no code.
    let killed = outcome(&ToolOutput::error("exit code signal\n"));
    assert_eq!(
        (killed.status, killed.exit_code),
        (GateStatus::Failed, None)
    );
}

#[test]
fn a_timeout_keeps_what_the_command_had_printed() {
    let timed = outcome(&ToolOutput::error(
        "command timed out after 5s and was terminated\nhalf\n",
    ));
    assert_eq!(timed.status, GateStatus::TimedOut);
    assert_eq!(timed.output, "half\n");
}

#[test]
fn a_refused_call_is_blocked_with_its_reason() {
    let blocked = outcome(&ToolOutput::refused("denied: matches deny rule `bash:x*`"));
    assert_eq!(blocked.status, GateStatus::Blocked);
    assert_eq!(
        blocked.refusal.as_deref(),
        Some("denied: matches deny rule `bash:x*`")
    );
}

#[test]
fn a_stopped_turn_is_not_a_failure_of_the_gate() {
    for text in [
        "command interrupted by the user\nx",
        "interrupted by the user before this tool ran",
    ] {
        assert!(outcome(&ToolOutput::error(text)).interrupted, "{text}");
    }
}
