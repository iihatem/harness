/// Keywords that, together with a non-zero, non-harness-bug exit code,
/// indicate the sandbox (rather than the command itself) most likely
/// caused the failure. Matched case-insensitively against combined
/// stdout+stderr.
const SANDBOX_DENIED_KEYWORDS: [&str; 5] = [
    "operation not permitted",
    "permission denied",
    "read-only file system",
    "sandbox",
    "failed to write file",
];

/// Additional keywords checked only when the caller tells us the sandbox
/// disabled networking. `"connection refused"` is deliberately excluded:
/// unlike a resolver or `connect(2)` failure, a refused connection is a
/// perfectly ordinary outcome for a real (unsandboxed) client hitting a
/// port nothing is listening on, so treating it as a sandbox signal would
/// produce false positives.
const NETWORK_DENIED_KEYWORDS: [&str; 5] = [
    "could not resolve host",
    "nodename nor servname",
    "network is unreachable",
    "temporary failure in name resolution",
    "failed to connect",
];

/// We have no fully reliable way to tell whether a command failed because
/// the sandbox blocked it, versus failing on its own merits — a script
/// might exit 1 because of a real bug. This is the same conservative,
/// keyword-based heuristic `codex` uses: require both a non-zero exit
/// (excluding a couple of "harness itself is broken" codes) and a denial
/// keyword in the output.
///
/// - Returns `false` on success (`exit_code == Some(0)`).
/// - Returns `false` for exit codes 65/71: `sandbox-exec`'s own EX_DATAERR /
///   EX_OSERR when it fails to parse or apply the profile — a bug in the
///   profile/harness, not a denial of the target command.
/// - Otherwise returns `true` only if `output` contains a denial keyword
///   (case-insensitive); `network_disabled` additionally enables the
///   network-failure keyword set.
pub fn looks_like_sandbox_denial(
    exit_code: Option<i32>,
    output: &str,
    network_disabled: bool,
) -> bool {
    match exit_code {
        Some(0) => return false,
        Some(65) | Some(71) => return false,
        _ => {}
    }

    let lower = output.to_lowercase();
    let matched_core = SANDBOX_DENIED_KEYWORDS.iter().any(|k| lower.contains(k));
    let matched_network =
        network_disabled && NETWORK_DENIED_KEYWORDS.iter().any(|k| lower.contains(k));

    matched_core || matched_network
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_write_denial_is_detected() {
        let output = "touch: /workspace/a: Operation not permitted\n";
        assert!(looks_like_sandbox_denial(Some(1), output, false));
    }

    #[test]
    fn plain_failure_without_keyword_is_not_a_denial() {
        let output = "error: something went wrong\n";
        assert!(!looks_like_sandbox_denial(Some(1), output, false));
    }

    #[test]
    fn success_is_never_a_denial() {
        let output = "Operation not permitted\n"; // even if output is noisy
        assert!(!looks_like_sandbox_denial(Some(0), output, false));
    }

    #[test]
    fn command_not_found_is_not_a_denial() {
        let output = "sh: foo: command not found\n";
        assert!(!looks_like_sandbox_denial(Some(127), output, false));
    }

    #[test]
    fn sandbox_exec_own_failure_is_not_a_denial() {
        let output = "sandbox-exec: Operation not permitted\n";
        assert!(!looks_like_sandbox_denial(Some(71), output, false));
        assert!(!looks_like_sandbox_denial(Some(65), output, false));
    }

    #[test]
    fn network_keyword_requires_network_disabled_flag() {
        let output = "curl: (6) Could not resolve host: example.com\n";
        assert!(looks_like_sandbox_denial(Some(6), output, true));
        assert!(!looks_like_sandbox_denial(Some(6), output, false));
    }

    #[test]
    fn connection_refused_is_excluded_from_network_keywords() {
        let output = "curl: (7) Failed to connect to 127.0.0.1 port 9: Connection refused\n";
        // "failed to connect" matches, so this is expected to be flagged —
        // but a bare "connection refused" alone (without "failed to
        // connect") must not be, since that's a normal client-side outcome.
        let bare_refused = "curl: (7) connect() to 127.0.0.1:9 failed: Connection refused\n";
        assert!(!looks_like_sandbox_denial(Some(7), bare_refused, true));
        assert!(looks_like_sandbox_denial(Some(7), output, true));
    }

    #[test]
    fn no_exit_code_still_checks_keywords() {
        let output = "permission denied\n";
        assert!(looks_like_sandbox_denial(None, output, false));
        assert!(!looks_like_sandbox_denial(None, "killed\n", false));
    }
}
