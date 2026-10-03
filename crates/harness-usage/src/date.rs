//! Calendar dates in UTC, as `YYYY-MM-DD`, for the periods reports and exports take.

use harness_core::time::civil_date;

/// The Unix time of the start of `date` (`YYYY-MM-DD`, UTC), or `None` when it is not a date
/// that exists.
pub fn day_start(date: &str) -> Option<u64> {
    let bytes = date.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let digits = |range: std::ops::Range<usize>| -> Option<i64> {
        let part = &date[range];
        part.bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| part.parse().ok())?
    };
    let (year, month, day) = (digits(0..4)?, digits(5..7)?, digits(8..10)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // Howard Hinnant's days-from-civil.
    let y = year - i64::from(month <= 2);
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = u64::try_from(days).ok()?.checked_mul(86_400)?;
    // A day that does not exist in its month (February 30th) comes back as another date.
    (civil_date(secs) == date).then_some(secs)
}

/// Whether `date` is a date that exists, as `YYYY-MM-DD`.
pub fn is_date(date: &str) -> bool {
    day_start(date).is_some()
}
