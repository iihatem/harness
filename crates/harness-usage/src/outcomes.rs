//! The outcome log: one JSON line for each finished turn, how it went, as counts and ids only,
//! in `outcomes/YYYY-MM.jsonl` (0600). Local, on by default, and separate from the ledger.

use std::{
    collections::HashMap,
    io::Write,
    path::{Path, PathBuf},
};

use harness_core::meter::{GateCounts, TurnRecord};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ledger::{is_month, month_of};

/// The log's format version, written to every record.
pub const VERSION: u32 = 1;

/// Where a turn's requests are in the ledger: its session, and the time span.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LedgerRef {
    pub session: String,
    pub from: u64,
    pub to: u64,
}

/// One finished turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutcomeRecord {
    pub v: u32,
    /// When the turn ended.
    pub t: u64,
    pub session: String,
    /// A hash of the workspace root, never the path.
    pub project: String,
    /// The turn's user message's entry in the session: an id, not its text.
    pub turn: String,
    pub role: String,
    pub model: String,
    /// `config`, `user`, `fallback` or `escalation`.
    pub selected_by: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// The cost is the ledger's: this says where to look.
    pub ledger: LedgerRef,
    pub first_token_ms: Option<u64>,
    pub duration_ms: u64,
    pub tool_calls: u32,
    pub invalid_calls: u32,
    pub retries: u32,
    pub finish_reason: String,
    pub gates: GateCounts,
    /// The kind and result of a Build turn's hand-off: filled by the model roles.
    pub handoff: Option<Value>,
    /// `rewound`, `interrupted` or `continued`, from what the user did next.
    pub user_signal: String,
}

impl OutcomeRecord {
    /// The record for `turn`, in the project `project`. A turn the user stopped is
    /// `interrupted`; any other is `continued` until the user rewinds it.
    pub fn of(turn: &TurnRecord, project: &str) -> OutcomeRecord {
        OutcomeRecord {
            v: VERSION,
            t: turn.ended_at,
            session: turn.session.clone(),
            project: project.to_string(),
            turn: turn.turn.clone(),
            role: turn.role.clone(),
            model: turn.model.clone(),
            selected_by: turn.selected_by.clone(),
            input_tokens: turn.input_tokens,
            output_tokens: turn.output_tokens,
            ledger: LedgerRef {
                session: turn.session.clone(),
                from: turn.started_at,
                to: turn.ended_at,
            },
            first_token_ms: turn.first_token_ms,
            duration_ms: turn.duration_ms,
            tool_calls: turn.tool_calls,
            invalid_calls: turn.invalid_calls,
            retries: turn.retries,
            finish_reason: turn.finish_reason.clone(),
            gates: turn.gates,
            handoff: None,
            user_signal: if turn.finish_reason == "interrupted" {
                "interrupted"
            } else {
                "continued"
            }
            .to_string(),
        }
    }
}

/// The outcome log's directory.
#[derive(Debug, Clone)]
pub struct OutcomeLog {
    dir: PathBuf,
}

impl OutcomeLog {
    pub fn new(dir: &Path) -> OutcomeLog {
        OutcomeLog {
            dir: dir.to_path_buf(),
        }
    }

    /// Appends `record` to its month's file, creating it (0600) and the directory (0700).
    pub fn append(&self, record: &OutcomeRecord) -> std::io::Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        let _lock = crate::lock::shared(&self.dir)?;
        let mut line = serde_json::to_string(record).map_err(std::io::Error::other)?;
        line.push('\n');
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(self.dir.join(format!("{}.jsonl", month_of(record.t))))?;
        file.write_all(line.as_bytes())
    }

    /// The files of the log, oldest month first.
    pub fn files(&self) -> Vec<PathBuf> {
        let mut files: Vec<PathBuf> = std::fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .and_then(|n| n.strip_suffix(".jsonl"))
                    .is_some_and(is_month)
            })
            .collect();
        files.sort();
        files
    }

    /// Says the user rewound `turns` of `session`, at `now`: one amendment line each,
    /// `{"kind":"signal","session":..,"turn":..,"signal":"rewound","t":..}`, appended to the
    /// current month's file (0600). The records are not rewritten; readers apply the latest
    /// signal for a turn (see `read`).
    pub fn mark_rewound(&self, session: &str, turns: &[String], now: u64) -> std::io::Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        if turns.is_empty() {
            return Ok(());
        }
        let _lock = crate::lock::shared(&self.dir)?;
        let mut lines = String::new();
        for turn in turns {
            lines.push_str(
                &serde_json::json!({
                    "kind": "signal",
                    "session": session,
                    "turn": turn,
                    "signal": "rewound",
                    "t": now,
                })
                .to_string(),
            );
            lines.push('\n');
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(self.dir.join(format!("{}.jsonl", month_of(now))))?;
        file.write_all(lines.as_bytes())
    }

    /// Every turn's record, oldest month first, each with the latest signal said for it: an
    /// amendment line replaces the `user_signal` of the record of its session and turn, wherever
    /// that record is. A line that is neither a record nor an amendment is skipped.
    pub fn read(&self) -> Vec<OutcomeRecord> {
        let mut records = Vec::new();
        let mut signals: HashMap<(String, String), String> = HashMap::new();
        for path in self.files() {
            let text = std::fs::read_to_string(path).unwrap_or_default();
            for line in text.lines() {
                let Ok(value) = serde_json::from_str::<Value>(line) else {
                    continue;
                };
                if value["kind"] == "signal" {
                    if let (Some(session), Some(turn), Some(signal)) = (
                        value["session"].as_str(),
                        value["turn"].as_str(),
                        value["signal"].as_str(),
                    ) {
                        signals.insert((session.to_string(), turn.to_string()), signal.to_string());
                    }
                } else if let Ok(record) = serde_json::from_value::<OutcomeRecord>(value) {
                    records.push(record);
                }
            }
        }
        for record in &mut records {
            if let Some(signal) = signals.get(&(record.session.clone(), record.turn.clone())) {
                record.user_signal = signal.clone();
            }
        }
        records
    }
}
