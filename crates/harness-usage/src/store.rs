//! The report cache: a SQLite database `usage/index.sqlite`, built from the ledger and safe to
//! delete at any time. It holds a copy of the ledger's records and how far into each ledger
//! file it has read, so it is brought up to date by reading only the lines it lacks.

use std::{
    collections::BTreeSet,
    io::{Read, Seek, SeekFrom},
    path::PathBuf,
};

use harness_core::time::civil_date;
use rusqlite::{Connection, params};

use crate::{
    error::{Error, Result},
    ledger::{Ledger, LedgerRecord},
    paths::Dirs,
};

/// The cache's layout version: a cache of another version is replaced.
const SCHEMA: i64 = 1;

/// What a report is grouped by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    Model,
    Provider,
    Day,
    Project,
}

impl Group {
    /// The column header and the word in the title.
    pub fn name(self) -> &'static str {
        match self {
            Group::Model => "model",
            Group::Provider => "provider",
            Group::Day => "day",
            Group::Project => "project",
        }
    }

    /// Reads `model`, `provider`, `day` or `project`.
    pub fn parse(text: &str) -> Option<Group> {
        match text {
            "model" => Some(Group::Model),
            "provider" => Some(Group::Provider),
            "day" => Some(Group::Day),
            "project" => Some(Group::Project),
            _ => None,
        }
    }

    fn column(self) -> &'static str {
        match self {
            Group::Model => "model",
            Group::Provider => "provider",
            Group::Day => "day",
            Group::Project => "project",
        }
    }
}

/// What to report: how to group, and the period, as UTC dates (`YYYY-MM-DD`, both ends
/// included).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    pub by: Group,
    pub since: Option<String>,
    pub until: Option<String>,
}

impl Query {
    /// Every record, grouped by `by`.
    pub fn all(by: Group) -> Query {
        Query {
            by,
            since: None,
            until: None,
        }
    }
}

/// Token counts, by the ledger's disjoint buckets (`cache_write` holds both tiers).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tokens {
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
    pub reasoning: u64,
}

/// A sum of money, and how many requests were left out of it for want of a price.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Money {
    pub usd: f64,
    /// Requests with no price: not counted in `usd`, and never counted as 0.
    pub unknown: u64,
}

/// One line of a report.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub key: String,
    pub requests: u64,
    /// Requests that ended in an error.
    pub failed: u64,
    pub tokens: Tokens,
    /// What the API-key requests cost; subscription and local requests count 0.
    pub billed: Money,
    /// What every hosted request would cost at the table's price (an estimate).
    pub list: Money,
}

/// A report: its rows, their total, and whether the ledger has no records at all.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub by: Group,
    pub rows: Vec<Row>,
    pub total: Row,
    pub ledger_empty: bool,
}

/// The cache, over the ledger it is built from.
pub struct Store {
    db: Connection,
    ledger: Ledger,
    path: PathBuf,
}

impl Store {
    /// Opens the cache under `dirs.usage`, making it when it is missing; one that is not a
    /// database of this layout is replaced, since everything in it can be built again.
    pub fn open(dirs: &Dirs) -> Result<Store> {
        crate::paths::create_private_dir(&dirs.usage)?;
        let path = dirs.usage.join("index.sqlite");
        let db = match open_db(&path) {
            Ok(db) => db,
            Err(_) => {
                remove_cache(&path);
                open_db(&path)?
            }
        };
        Ok(Store {
            db,
            ledger: Ledger::new(&dirs.usage),
            path,
        })
    }

    /// Brings the cache up to date with the ledger: reads each file from where the cache stopped,
    /// forgets files that are gone or that shrank, and returns how many records it added. A last
    /// line without its newline is left for the next call.
    pub fn sync(&mut self) -> Result<usize> {
        match self.sync_inner() {
            Ok(added) => Ok(added),
            Err(Error(_)) => {
                // Whatever is wrong with the cache, the ledger is the truth: start over once.
                self.rebuild()?;
                self.sync_inner()
            }
        }
    }

    fn rebuild(&mut self) -> Result<()> {
        let tx = self.db.transaction()?;
        tx.execute_batch("DELETE FROM requests; DELETE FROM files;")?;
        tx.commit()?;
        Ok(())
    }

    fn sync_inner(&mut self) -> Result<usize> {
        let files = self.ledger.files();
        let present: BTreeSet<String> = files
            .iter()
            .filter_map(|p| p.file_name().and_then(|n| n.to_str()).map(String::from))
            .collect();
        let tx = self.db.transaction()?;
        // Files that are gone.
        let known: Vec<String> = {
            let mut stmt = tx.prepare("SELECT name FROM files")?;
            let rows = stmt.query_map([], |row| row.get(0))?;
            rows.collect::<std::result::Result<_, _>>()?
        };
        for name in known.iter().filter(|n| !present.contains(*n)) {
            tx.execute("DELETE FROM requests WHERE file = ?1", [name])?;
            tx.execute("DELETE FROM files WHERE name = ?1", [name])?;
        }
        let mut added = 0;
        for path in &files {
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let offset: i64 = tx
                .query_row("SELECT offset FROM files WHERE name = ?1", [name], |r| {
                    r.get(0)
                })
                .unwrap_or(0);
            let len = std::fs::metadata(path)?.len() as i64;
            let mut offset = offset;
            if len < offset {
                // The file was rewritten shorter than what was read: read it again from its start.
                tx.execute("DELETE FROM requests WHERE file = ?1", [name])?;
                offset = 0;
            }
            if len > offset {
                let mut file = std::fs::File::open(path)?;
                file.seek(SeekFrom::Start(offset as u64))?;
                let mut bytes = Vec::new();
                file.take((len - offset) as u64).read_to_end(&mut bytes)?;
                // Only whole lines: the last one may still be being written.
                let whole = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
                for line in bytes[..whole].split(|b| *b == b'\n') {
                    let Ok(record) = serde_json::from_slice::<LedgerRecord>(line) else {
                        continue;
                    };
                    insert(&tx, name, &record)?;
                    added += 1;
                }
                offset += whole as i64;
            }
            tx.execute(
                "INSERT INTO files (name, offset) VALUES (?1, ?2)
                 ON CONFLICT(name) DO UPDATE SET offset = excluded.offset",
                params![name, offset],
            )?;
        }
        tx.commit()?;
        Ok(added)
    }

    /// The report for `query`, from what the cache holds (call [`sync`](Self::sync) first).
    pub fn report(&self, query: &Query) -> Result<Report> {
        let column = query.by.column();
        let sql = format!(
            "SELECT {column},
                COUNT(*),
                SUM(outcome != 'ok'),
                SUM(input), SUM(cache_read), SUM(cache_write + cache_write_1h), SUM(output), SUM(reasoning),
                SUM(CASE WHEN account = 'api_key' THEN COALESCE(billed, 0) ELSE 0 END),
                SUM(CASE WHEN account = 'api_key' AND billed IS NULL THEN 1 ELSE 0 END),
                SUM(COALESCE(list, 0)),
                SUM(CASE WHEN list IS NULL THEN 1 ELSE 0 END)
             FROM requests
             WHERE (?1 IS NULL OR day >= ?1) AND (?2 IS NULL OR day <= ?2)
             GROUP BY {column} ORDER BY {column}"
        );
        let mut stmt = self.db.prepare(&sql)?;
        let rows = stmt.query_map(params![query.since, query.until], |r| {
            let n = |i: usize| -> rusqlite::Result<u64> { Ok(r.get::<_, i64>(i)?.max(0) as u64) };
            Ok(Row {
                key: r.get(0)?,
                requests: n(1)?,
                failed: n(2)?,
                tokens: Tokens {
                    input: n(3)?,
                    cache_read: n(4)?,
                    cache_write: n(5)?,
                    output: n(6)?,
                    reasoning: n(7)?,
                },
                billed: Money {
                    usd: r.get(8)?,
                    unknown: n(9)?,
                },
                list: Money {
                    usd: r.get(10)?,
                    unknown: n(11)?,
                },
            })
        })?;
        let rows: Vec<Row> = rows.collect::<std::result::Result<_, _>>()?;
        let mut total = Row {
            key: "total".into(),
            requests: 0,
            failed: 0,
            tokens: Tokens::default(),
            billed: Money::default(),
            list: Money::default(),
        };
        for row in &rows {
            total.requests += row.requests;
            total.failed += row.failed;
            total.tokens.input += row.tokens.input;
            total.tokens.cache_read += row.tokens.cache_read;
            total.tokens.cache_write += row.tokens.cache_write;
            total.tokens.output += row.tokens.output;
            total.tokens.reasoning += row.tokens.reasoning;
            total.billed.usd += row.billed.usd;
            total.billed.unknown += row.billed.unknown;
            total.list.usd += row.list.usd;
            total.list.unknown += row.list.unknown;
        }
        let ledger_empty: i64 = self
            .db
            .query_row("SELECT COUNT(*) FROM requests", [], |r| r.get(0))?;
        Ok(Report {
            by: query.by,
            rows,
            total,
            ledger_empty: ledger_empty == 0,
        })
    }

    /// Where the cache is.
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

/// Opens (making it when missing, private) and prepares the database at `path`.
fn open_db(path: &std::path::Path) -> Result<Connection> {
    use std::os::unix::fs::OpenOptionsExt;
    // Made private before SQLite opens it.
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?;
    let db = Connection::open(path)?;
    db.busy_timeout(std::time::Duration::from_secs(5))?;
    let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version != SCHEMA {
        db.execute_batch(
            "DROP TABLE IF EXISTS requests; DROP TABLE IF EXISTS files;
             CREATE TABLE requests (
                 file TEXT NOT NULL, t INTEGER NOT NULL, day TEXT NOT NULL,
                 session TEXT NOT NULL, project TEXT NOT NULL, role TEXT NOT NULL,
                 model TEXT NOT NULL, provider TEXT NOT NULL, account TEXT NOT NULL,
                 input INTEGER NOT NULL, cache_read INTEGER NOT NULL,
                 cache_write INTEGER NOT NULL, cache_write_1h INTEGER NOT NULL,
                 output INTEGER NOT NULL, reasoning INTEGER NOT NULL,
                 billed REAL, list REAL, price TEXT, ms INTEGER NOT NULL,
                 outcome TEXT NOT NULL, window TEXT
             );
             CREATE INDEX requests_day ON requests (day);
             CREATE INDEX requests_session ON requests (session);
             CREATE TABLE files (name TEXT PRIMARY KEY, offset INTEGER NOT NULL);",
        )?;
        db.pragma_update(None, "user_version", SCHEMA)?;
    }
    // Reading the tables proves the file is a database of this layout.
    db.query_row("SELECT COUNT(*) FROM files", [], |r| r.get::<_, i64>(0))?;
    Ok(db)
}

fn remove_cache(path: &std::path::Path) {
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let mut name = path.as_os_str().to_os_string();
        name.push(suffix);
        let _ = std::fs::remove_file(PathBuf::from(name));
    }
}

fn insert(tx: &rusqlite::Transaction<'_>, file: &str, r: &LedgerRecord) -> Result<()> {
    tx.execute(
        "INSERT INTO requests (file, t, day, session, project, role, model, provider, account,
            input, cache_read, cache_write, cache_write_1h, output, reasoning,
            billed, list, price, ms, outcome, window)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21)",
        params![
            file,
            r.t as i64,
            civil_date(r.t),
            r.session,
            r.project,
            r.role,
            r.model,
            r.model.split('/').next().unwrap_or_default(),
            r.account.as_str(),
            r.input as i64,
            r.cache_read as i64,
            r.cache_write as i64,
            r.cache_write_1h as i64,
            r.output as i64,
            r.reasoning as i64,
            r.billed_usd,
            r.list_usd,
            r.price,
            r.ms as i64,
            r.outcome,
            r.window,
        ],
    )?;
    Ok(())
}

/// A sum of money as a cell: `$1.20`, `$0.0013`, `price unknown`, or both
/// (`$0.50 + 1 price unknown`).
pub fn money_cell(money: &Money) -> String {
    let amount = if money.usd >= 0.01 || money.usd == 0.0 {
        format!("${:.2}", money.usd)
    } else {
        format!("${:.4}", money.usd)
    };
    match (money.unknown, money.usd == 0.0) {
        (0, _) => amount,
        (_, true) => "price unknown".to_string(),
        (n, false) => format!("{amount} + {n} price unknown"),
    }
}

fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// The report as lines of text: a title, a header, a row for each group and the total. An empty
/// ledger says so, and that usage from M1 sessions is not included.
pub fn render(report: &Report) -> Vec<String> {
    let mut lines = vec![format!("Usage by {}", report.by.name())];
    if report.ledger_empty {
        lines.push(
            "No usage recorded yet. Usage from M1 sessions is not included: the ledger starts with this version."
                .into(),
        );
        return lines;
    }
    let rows: Vec<&Row> = report.rows.iter().chain([&report.total]).collect();
    let width = rows
        .iter()
        .map(|r| r.key.chars().count())
        .max()
        .unwrap_or(0)
        .max(report.by.name().len());
    lines.push(format!(
        "{:width$}  {:>8}  {:>12}  {:>12}  {:>10}  {:>10}  {:>14}  {}",
        report.by.name(),
        "requests",
        "input",
        "cache read",
        "cache write",
        "output",
        "billed",
        "list price (estimate)",
    ));
    for row in rows {
        lines.push(format!(
            "{:width$}  {:>8}  {:>12}  {:>12}  {:>10}  {:>10}  {:>14}  {}",
            row.key,
            thousands(row.requests),
            thousands(row.tokens.input),
            thousands(row.tokens.cache_read),
            thousands(row.tokens.cache_write),
            thousands(row.tokens.output),
            money_cell(&row.billed),
            money_cell(&row.list),
        ));
    }
    lines
}
