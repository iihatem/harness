//! The ledger: one JSON line for every model request, appended to a file for each month, with
//! counts and ids only: never prompt text, model output, file paths or command lines.

use std::{
    io::Write,
    path::{Path, PathBuf},
};

use harness_core::{meter::AccountKind, time::civil_date};
use serde::{Deserialize, Serialize};

/// The ledger's format version, written to every record.
pub const VERSION: u32 = 1;

/// One model request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LedgerRecord {
    #[serde(default)]
    pub v: u32,
    /// When the request ended, in seconds since the Unix epoch.
    pub t: u64,
    pub session: String,
    /// A hash of the workspace root, never the path.
    pub project: String,
    pub role: String,
    /// `<provider>/<model>`.
    pub model: String,
    pub account: AccountKind,
    /// The disjoint token buckets (see `harness_core::message::Buckets`).
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cache_write_1h: u64,
    pub output: u64,
    pub reasoning: u64,
    /// What the request cost on its account: the table price on an API-key account, 0 on a
    /// subscription or a local one; `None` when the model has no price.
    pub billed_usd: Option<f64>,
    /// What the same tokens cost at the table's price, hosted or not; `None` when unpriced.
    pub list_usd: Option<f64>,
    /// Which price table priced it (`override`, `downloaded <date>` or `embedded <date>`).
    pub price: Option<String>,
    /// How long the request took, in milliseconds.
    pub ms: u64,
    /// `ok`, or `error:<kind>`.
    pub outcome: String,
    /// The id of the subscription window snapshot seen with the request, when there is one.
    pub window: Option<String>,
}

/// `YYYY-MM` of a time in seconds since the epoch: the month whose file a record goes in.
pub fn month_of(t: u64) -> String {
    civil_date(t)[..7].to_string()
}

/// The ledger's directory.
#[derive(Debug, Clone)]
pub struct Ledger {
    dir: PathBuf,
}

impl Ledger {
    pub fn new(dir: &Path) -> Ledger {
        Ledger {
            dir: dir.to_path_buf(),
        }
    }

    /// The directory the files are in.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Appends `record` to its month's file, creating the file (0600) and directory (0700) when
    /// needed. One `write` of the whole line, so records from two processes do not mix.
    pub fn append(&self, record: &LedgerRecord) -> std::io::Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        crate::paths::create_private_dir(&self.dir)?;
        let path = self.file_for(record.t);
        let mut line = serde_json::to_string(record).map_err(std::io::Error::other)?;
        line.push('\n');
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(line.as_bytes())
    }

    /// Appends `snapshot` to its month's window file, `windows-YYYY-MM.jsonl` (0600): its id, when
    /// it was observed, and each window's length, used percent, reset time and source.
    pub fn append_window(
        &self,
        snapshot: &harness_core::meter::WindowSnapshot,
    ) -> std::io::Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        crate::paths::create_private_dir(&self.dir)?;
        let windows: Vec<serde_json::Value> = snapshot
            .windows
            .iter()
            .map(|w| {
                serde_json::json!({
                    "minutes": w.window_minutes,
                    "used_percent": w.used_percent,
                    "resets_at": w.resets_at,
                    "source": w.source,
                })
            })
            .collect();
        let mut line = serde_json::json!({
            "id": snapshot.id(),
            "t": snapshot.observed_at,
            "windows": windows,
        })
        .to_string();
        line.push('\n');
        let path = self
            .dir
            .join(format!("windows-{}.jsonl", month_of(snapshot.observed_at)));
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(line.as_bytes())
    }

    /// The file for the month of `t`.
    pub fn file_for(&self, t: u64) -> PathBuf {
        self.dir.join(format!("ledger-{}.jsonl", month_of(t)))
    }

    /// The ledger files, oldest month first.
    pub fn files(&self) -> Vec<PathBuf> {
        let mut files: Vec<PathBuf> = std::fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(is_ledger_file)
            })
            .collect();
        files.sort();
        files
    }

    /// Every record, oldest month first. A line that is not a record (the end of a file a crash
    /// cut short, say) is skipped.
    pub fn read(&self) -> Vec<LedgerRecord> {
        self.files()
            .iter()
            .flat_map(|path| read_file(path))
            .collect()
    }
}

/// Whether `name` is a month's ledger file: `ledger-YYYY-MM.jsonl`.
pub fn is_ledger_file(name: &str) -> bool {
    name.strip_prefix("ledger-")
        .and_then(|rest| rest.strip_suffix(".jsonl"))
        .is_some_and(is_month)
}

/// Whether `text` is `YYYY-MM`.
pub fn is_month(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 7
        && bytes[4] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(i, b)| i == 4 || b.is_ascii_digit())
}

/// The records in one ledger file, skipping lines that are not records.
pub fn read_file(path: &Path) -> Vec<LedgerRecord> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}
