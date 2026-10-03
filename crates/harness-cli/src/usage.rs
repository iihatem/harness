//! `harness usage`: what the ledger holds, by model, provider, day or project. Offline: it reads
//! only files on this machine.

use harness_usage::{
    date::is_date,
    paths::Dirs,
    store::{Group, Query, Store, avoided_line, render},
};

use crate::{
    setup::{self, Setup},
    term::terminal_safe,
};

/// A date from the command line, or the message that says why it is not one.
fn date(flag: &str, value: Option<String>) -> Result<Option<String>, String> {
    match value {
        Some(text) if !is_date(&text) => Err(format!(
            "{flag} takes a date as YYYY-MM-DD (UTC), not `{text}`"
        )),
        other => Ok(other),
    }
}

/// What to report, from the command line's words.
fn query(by: &str, since: Option<String>, until: Option<String>) -> Result<Query, String> {
    let by = Group::parse(by)
        .ok_or_else(|| format!("--by takes model, provider, day or project, not `{by}`"))?;
    Ok(Query {
        by,
        since: date("--since", since)?,
        until: date("--until", until)?,
    })
}

/// The report's lines for `query`, from the ledger: the table, and with a baseline named the
/// avoided figure. Offline.
pub fn lines(setup: &Setup, query: &Query) -> Result<Vec<String>, String> {
    let dirs = Dirs::under(&setup.paths.data_dir);
    let report = Store::open(&dirs)
        .and_then(|mut store| {
            store.sync()?;
            store.report(query)
        })
        .map_err(|e| e.to_string())?;
    let mut lines = render(&report);
    // Only with a baseline named: never a number without one.
    if let Some(baseline) = &setup.config.usage.baseline {
        let avoided = report.avoided(&crate::pricing::load(setup), Some(baseline));
        lines.extend(avoided_line(baseline, &avoided));
    }
    Ok(lines)
}

/// The report for `/usage <args>`: a group (`model`, `provider`, `day`, `project`; `model` by
/// default) and `--since DATE`, `--until DATE`. A mistake in the arguments is one line saying so.
pub fn session_lines(setup: &Setup, args: &str) -> Vec<String> {
    let mut by = "model".to_string();
    let (mut since, mut until) = (None, None);
    let mut words = args.split_whitespace();
    while let Some(word) = words.next() {
        match word {
            "--since" => since = words.next().map(String::from),
            "--until" => until = words.next().map(String::from),
            group if !group.starts_with('-') => by = group.to_string(),
            other => {
                return vec![format!(
                    "/usage takes a group and --since, --until, not `{other}`"
                )];
            }
        }
    }
    match query(&by, since, until).and_then(|query| lines(setup, &query)) {
        Ok(lines) => lines,
        Err(message) => vec![format!("/usage: {message}")],
    }
}

/// Prints the report. Exit code 2 for an option it does not understand, 1 when the ledger or its
/// cache cannot be read.
pub fn report(by: &str, since: Option<String>, until: Option<String>) -> u8 {
    let query = match query(by, since, until) {
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
    match lines(&setup, &query) {
        Ok(lines) => {
            for line in lines {
                println!("{}", terminal_safe(&line));
            }
            0
        }
        Err(message) => {
            eprintln!("error: {}", terminal_safe(&message));
            1
        }
    }
}

/// `/budget`'s lines: each budget with what is spent against it, and what a reached one stops.
pub fn budget_lines(report: &[harness_usage::budget::BudgetLine]) -> Vec<String> {
    let note =
        "a reached budget pauses requests on an API key; ChatGPT plans and local models go on";
    report
        .iter()
        .map(|line| {
            let name = line.budget.name();
            match line.limit_usd {
                Some(limit) => format!(
                    "{name:<8}  ${:.2} of ${limit:.2} ({:.0}%)",
                    line.spent_usd,
                    line.spent_usd / limit * 100.0
                ),
                None => format!("{name:<8}  ${:.2} spent, no limit", line.spent_usd),
            }
        })
        .chain(std::iter::once(note.to_string()))
        .collect()
}

/// `harness usage export`: the ledger's records on standard output.
pub fn export(since: Option<&str>, format: &str) -> u8 {
    let Some(format) = harness_usage::export::Format::parse(format) else {
        eprintln!("error: --format takes jsonl or csv, not `{format}`");
        return 2;
    };
    let setup = match setup::load() {
        Ok(setup) => setup,
        Err(message) => {
            eprintln!("error: {}", terminal_safe(&message));
            return 2;
        }
    };
    let dirs = Dirs::under(&setup.paths.data_dir);
    let mut out = Vec::new();
    if let Err(e) = harness_usage::export::export(&dirs, since, format, &mut out) {
        eprintln!("error: {}", terminal_safe(&e.to_string()));
        return 2;
    }
    // A closed pipe is the reader's choice, not an error of ours.
    let _ = std::io::Write::write_all(&mut std::io::stdout().lock(), &out);
    0
}

/// `harness usage forget`: deletes the ledger and the outcome log, in a range, and rebuilds the
/// cache. With neither `--before` nor `--all` it deletes nothing and says so.
pub fn forget(before: Option<&str>, all: bool) -> u8 {
    if before.is_none() && !all {
        eprintln!(
            "error: usage forget needs a range: --before DATE (UTC, YYYY-MM-DD) or --all; nothing was deleted"
        );
        return 2;
    }
    let setup = match setup::load() {
        Ok(setup) => setup,
        Err(message) => {
            eprintln!("error: {}", terminal_safe(&message));
            return 2;
        }
    };
    let dirs = Dirs::under(&setup.paths.data_dir);
    match harness_usage::export::forget(&dirs, before) {
        Ok(files) => {
            println!("Forgot usage data: {files} file(s) deleted or shortened.");
            0
        }
        Err(e) => {
            eprintln!("error: {}", terminal_safe(&e.to_string()));
            1
        }
    }
}
