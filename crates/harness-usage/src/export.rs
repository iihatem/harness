//! `harness usage export` and `harness usage forget`.

use std::{
    io::Write,
    path::{Path, PathBuf},
};

use harness_core::time::timestamp;
use serde_json::Value;

use crate::{
    date::day_start,
    error::{Error, Result},
    ledger::{Ledger, LedgerRecord, is_ledger_file, is_month},
    outcomes::OutcomeLog,
    paths::Dirs,
    store::Store,
};

/// How `export` writes the records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Jsonl,
    Csv,
}

impl Format {
    pub fn parse(text: &str) -> Option<Format> {
        match text {
            "jsonl" => Some(Format::Jsonl),
            "csv" => Some(Format::Csv),
            _ => None,
        }
    }
}

const CSV_HEADER: &str = "t,time,session,project,role,model,account,input,cache_read,cache_write,cache_write_1h,output,reasoning,billed_usd,list_usd,price,ms,outcome,window";

/// A CSV cell: quoted when it holds a comma, a quote or a line break.
fn cell(text: &str) -> String {
    if text.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", text.replace('"', "\"\""))
    } else {
        text.to_string()
    }
}

fn number(value: Option<f64>) -> String {
    value.map(|v| v.to_string()).unwrap_or_default()
}

fn csv_row(r: &LedgerRecord) -> String {
    let cells = [
        r.t.to_string(),
        timestamp(r.t),
        cell(&r.session),
        cell(&r.project),
        cell(&r.role),
        cell(&r.model),
        r.account.as_str().to_string(),
        r.input.to_string(),
        r.cache_read.to_string(),
        r.cache_write.to_string(),
        r.cache_write_1h.to_string(),
        r.output.to_string(),
        r.reasoning.to_string(),
        number(r.billed_usd),
        number(r.list_usd),
        cell(r.price.as_deref().unwrap_or_default()),
        r.ms.to_string(),
        cell(&r.outcome),
        cell(r.window.as_deref().unwrap_or_default()),
    ];
    cells.join(",")
}

/// Writes the ledger's records since `since` (a UTC date; all when `None`) to `out`.
pub fn export(
    dirs: &Dirs,
    since: Option<&str>,
    format: Format,
    out: &mut impl Write,
) -> Result<()> {
    let from = match since {
        Some(date) => day_start(date).ok_or_else(|| {
            Error(format!(
                "--since takes a date as YYYY-MM-DD (UTC), not `{date}`"
            ))
        })?,
        None => 0,
    };
    let records: Vec<LedgerRecord> = Ledger::new(&dirs.usage)
        .read()
        .into_iter()
        .filter(|r| r.t >= from)
        .collect();
    if records.is_empty() {
        return Ok(());
    }
    if format == Format::Csv {
        writeln!(out, "{CSV_HEADER}")?;
    }
    for record in &records {
        match format {
            Format::Jsonl => writeln!(
                out,
                "{}",
                serde_json::to_string(record).map_err(|e| Error(e.to_string()))?
            )?,
            Format::Csv => writeln!(out, "{}", csv_row(record))?,
        }
    }
    Ok(())
}

/// Replaces `path` with `bytes` through a temporary file, private to the user.
pub(crate) fn replace_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension(format!("jsonl.tmp-{}", std::process::id()));
    let written = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// The month a file name is for: `ledger-2026-10.jsonl`, `windows-2026-10.jsonl` and
/// `2026-10.jsonl` are October 2026.
fn month_of_file(name: &str) -> Option<String> {
    let stem = name.strip_suffix(".jsonl")?;
    let month = stem
        .strip_prefix("ledger-")
        .or_else(|| stem.strip_prefix("windows-"))
        .unwrap_or(stem);
    is_month(month).then(|| month.to_string())
}

/// The Unix time of the first day of `month` (`YYYY-MM`) and of the month after it.
fn month_span(month: &str) -> Option<(u64, u64)> {
    let start = day_start(&format!("{month}-01"))?;
    let (year, number): (u32, u32) = (month[..4].parse().ok()?, month[5..7].parse().ok()?);
    let next = if number == 12 {
        format!("{:04}-01-01", year + 1)
    } else {
        format!("{year:04}-{:02}-01", number + 1)
    };
    Some((start, day_start(&next)?))
}

/// The files that hold usage: the ledger's, the window snapshots' and the outcome log's.
fn usage_files(dirs: &Dirs) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dirs.usage)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                is_ledger_file(n) || (n.starts_with("windows-") && month_of_file(n).is_some())
            })
        })
        .collect();
    files.extend(OutcomeLog::new(&dirs.outcomes).files());
    files.sort();
    files
}

/// The exclusive lock on `dir`, when there is a directory (nothing to lock, and nothing to
/// forget, when there is not).
fn lock_if_exists(dir: &Path) -> Result<Option<crate::lock::DirLock>> {
    if !dir.is_dir() {
        return Ok(None);
    }
    Ok(Some(crate::lock::exclusive(dir)?))
}

/// Deletes the ledger, window and outcome files before `before` (a UTC date), or all of them when
/// `None`, then brings the report cache in line. A month that holds `before` loses only the
/// records before it. Returns how many files were deleted or rewritten.
pub fn forget(dirs: &Dirs, before: Option<&str>) -> Result<usize> {
    let cutoff = match before {
        Some(date) => Some(day_start(date).ok_or_else(|| {
            Error(format!(
                "--before takes a date as YYYY-MM-DD (UTC), not `{date}`"
            ))
        })?),
        None => None,
    };
    // The only rewriter of usage files: appends wait for these locks (usage first, then
    // outcomes, always in that order) while it runs.
    let _usage_lock = lock_if_exists(&dirs.usage)?;
    let _outcomes_lock = lock_if_exists(&dirs.outcomes)?;
    let mut touched = 0;
    for path in usage_files(dirs) {
        let Some(cutoff) = cutoff else {
            std::fs::remove_file(&path)?;
            touched += 1;
            continue;
        };
        let Some((start, end)) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(month_of_file)
            .and_then(|m| month_span(&m))
        else {
            continue;
        };
        if end <= cutoff {
            std::fs::remove_file(&path)?;
            touched += 1;
        } else if start < cutoff {
            let text = std::fs::read_to_string(&path)?;
            let kept: String = text
                .lines()
                .filter(|line| {
                    serde_json::from_str::<Value>(line)
                        .ok()
                        .and_then(|v| v["t"].as_u64())
                        .is_some_and(|t| t >= cutoff)
                })
                .map(|line| format!("{line}\n"))
                .collect();
            if kept.is_empty() {
                std::fs::remove_file(&path)?;
            } else {
                replace_private(&path, kept.as_bytes())?;
            }
            touched += 1;
        }
    }
    drop((_outcomes_lock, _usage_lock));
    // The cache follows the ledger: files that are gone or shorter are read again or forgotten.
    if dirs.usage.exists() {
        let mut store = Store::open(dirs)?;
        store.sync()?;
        // Rows that were deleted must not linger in the cache file.
        store.vacuum();
    }
    Ok(touched)
}
