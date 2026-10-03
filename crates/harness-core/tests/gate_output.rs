//! Reading what a gate command left, and cutting it to a tail.

use harness_core::{
    event::GateStatus,
    gate::{outcome, tail},
    tool::ToolOutput,
};

fn shown(output: &str, lines: usize) -> (String, usize) {
    let t = tail(output, lines);
    (t.text, t.omitted)
}

#[test]
fn a_tail_keeps_the_last_lines_and_counts_the_rest() {
    assert_eq!(shown("a\nb\nc\nd\n", 2), ("c\nd\n".to_string(), 2));
    assert_eq!(shown("a\nb", 5), ("a\nb\n".to_string(), 0));
    assert_eq!(shown("", 3), (String::new(), 0));
    assert_eq!(shown("only", 1), ("only\n".to_string(), 0));
    assert!(!tail("a\nb\nc\n", 2).shortened);
}

// A single line of 3 MB (minified output, a JSON dump) is cut, and so is a long output of long
// lines: what the model gets is bounded in bytes as well as in lines.
#[test]
fn one_very_long_line_is_cut_with_a_marker() {
    let output = format!("{}\n", "x".repeat(3_000_000));
    let t = tail(&output, 60);
    assert!(t.text.len() < 2_000, "{}", t.text.len());
    assert!(t.text.starts_with(&"x".repeat(1000)));
    assert!(t.text.contains("more characters"), "{}", t.text);
    assert!(t.shortened);
    assert_eq!(t.omitted, 0);
}

#[test]
fn a_tail_is_at_most_16_kib_and_the_lines_dropped_for_it_are_counted() {
    let output: String = (0..60)
        .map(|n| format!("{n:04} {}\n", "y".repeat(990)))
        .collect();
    let t = tail(&output, 60);
    assert!(t.text.len() <= 16 * 1024, "{}", t.text.len());
    assert!(t.omitted > 40, "{}", t.omitted);
    // The last lines are the ones kept.
    assert!(t.text.contains("0059 "), "{}", &t.text[..40]);
    assert!(!t.text.contains("0000 "));
    // Whole characters only.
    let wide = "é".repeat(5000);
    let t = tail(&format!("{wide}\n"), 5);
    assert!(t.text.len() <= 16 * 1024);
    assert!(t.text.starts_with(&"é".repeat(1000)));
}

#[test]
fn lines_of_normal_length_are_not_touched() {
    let t = tail("short\nlines\n", 60);
    assert_eq!(t.text, "short\nlines\n");
    assert!(!t.shortened);
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
