use std::path::Path;

use super::SANDBOX_EXEC_PATH;

/// Returns `true` if `/usr/bin/sandbox-exec` exists and a trivial profile
/// (`(version 1)(allow default)`) runs `/usr/bin/true` successfully with no
/// stderr output. This is a cheap smoke test that Seatbelt is present and
/// usable (e.g. not itself running nested inside another sandbox that
/// blocks `sandbox-exec`).
pub fn seatbelt_available() -> bool {
    if !Path::new(SANDBOX_EXEC_PATH).is_file() {
        return false;
    }

    let output = match std::process::Command::new(SANDBOX_EXEC_PATH)
        .arg("-p")
        .arg("(version 1)(allow default)")
        .arg("/usr/bin/true")
        .output()
    {
        Ok(output) => output,
        Err(_) => return false,
    };

    output.status.success() && output.stderr.is_empty()
}
