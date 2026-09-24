use std::{
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

/// The base system prompt plus environment facts captured once per run. Kept short on purpose:
/// local models have small context windows. P3 replaces this with full context assembly.
pub fn system_prompt(workspace: &Path, date: &str) -> String {
    format!(
        "You are harness, a coding agent working in the user's project.\n\
         Use the tools to inspect files and make changes; never guess file contents.\n\
         Read a file before editing it. Make focused changes, and verify them (for example by running the tests) when you can.\n\
         When you are done, reply with a short summary of what you changed.\n\
         \n\
         Working directory: {}\n\
         Operating system: {}\n\
         Date: {date}\n",
        workspace.display(),
        std::env::consts::OS
    )
}

/// Today's date in UTC as `YYYY-MM-DD`.
pub fn today_utc() -> String {
    civil_date(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    )
}

/// Converts Unix seconds to a UTC calendar date (Howard Hinnant's days-to-civil algorithm).
pub fn civil_date(unix_secs: u64) -> String {
    let z = (unix_secs / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates_are_correct() {
        assert_eq!(civil_date(0), "1970-01-01");
        assert_eq!(civil_date(1_709_164_800), "2024-02-29");
        assert_eq!(civil_date(1_790_208_000), "2026-09-24");
    }

    #[test]
    fn base_prompt_is_under_1000_tokens() {
        let prompt = system_prompt(Path::new("/some/project"), "2026-09-24");
        assert!(prompt.len() / 4 < 1000, "~{} tokens", prompt.len() / 4);
        assert!(prompt.contains("Working directory: /some/project"));
    }
}
