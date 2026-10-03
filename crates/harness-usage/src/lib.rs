//! Usage for harness: the ledger of model requests, its report cache, prices, budgets and the
//! outcome log. Everything here is local: nothing is sent anywhere.

pub mod ledger;
pub mod meter;
pub mod paths;

/// The version of the SQLite library built into harness, which the report cache runs on.
pub fn sqlite_version() -> String {
    rusqlite::version().to_string()
}

/// Opens an in-memory database and counts two rows, to show the bundled library works.
pub fn sqlite_smoke() -> rusqlite::Result<i64> {
    let db = rusqlite::Connection::open_in_memory()?;
    db.execute_batch("CREATE TABLE t (n INTEGER); INSERT INTO t VALUES (1), (2);")?;
    db.query_row("SELECT COUNT(*) FROM t", [], |row| row.get(0))
}
