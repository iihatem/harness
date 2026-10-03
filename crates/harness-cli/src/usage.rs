//! `harness usage`: what the ledger holds, by model, provider, day or project. Offline: it reads
//! only files on this machine.

use harness_usage::{
    date::is_date,
    paths::Dirs,
    store::{Group, Query, Store, avoided_line, render},
};

use crate::{setup, term::terminal_safe};

/// A date from the command line, or the message that says why it is not one.
fn date(flag: &str, value: Option<String>) -> Result<Option<String>, String> {
    match value {
        Some(text) if !is_date(&text) => Err(format!(
            "{flag} takes a date as YYYY-MM-DD (UTC), not `{text}`"
        )),
        other => Ok(other),
    }
}

/// Prints the report. Exit code 2 for an option it does not understand, 1 when the ledger or its
/// cache cannot be read.
pub fn report(by: &str, since: Option<String>, until: Option<String>) -> u8 {
    let query = (|| {
        let by = Group::parse(by)
            .ok_or_else(|| format!("--by takes model, provider, day or project, not `{by}`"))?;
        Ok::<_, String>(Query {
            by,
            since: date("--since", since)?,
            until: date("--until", until)?,
        })
    })();
    let query = match query {
        Ok(query) => query,
        Err(message) => {
            eprintln!("error: {message}");
            return 2;
        }
    };
    let setup = match setup::load() {
        Ok(setup) => setup,
        Err(message) => {
            eprintln!("error: {}", terminal_safe(&message));
            return 2;
        }
    };
    let dirs = Dirs::under(&setup.paths.data_dir);
    let report = Store::open(&dirs).and_then(|mut store| {
        store.sync()?;
        store.report(&query)
    });
    match report {
        Ok(report) => {
            for line in render(&report) {
                println!("{}", terminal_safe(&line));
            }
            // Only with a baseline named: never a number without one.
            if let Some(baseline) = &setup.config.usage.baseline {
                let avoided = report.avoided(&crate::pricing::load(&setup), Some(baseline));
                if let Some(line) = avoided_line(baseline, &avoided) {
                    println!("{}", terminal_safe(&line));
                }
            }
            0
        }
        Err(e) => {
            eprintln!("error: {}", terminal_safe(&e.to_string()));
            1
        }
    }
}
