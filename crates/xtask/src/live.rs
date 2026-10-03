//! Live mode: a model runs each task through `harness ask`, the task's test decides pass or
//! fail, and the run's events give the other metrics. Nothing here calls a model itself: the
//! `harness` binary does, with the configuration it is given.

use std::{
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use harness_core::edit_format::EditFormat;
use serde::Serialize;
use serde_json::Value;

use crate::task::{Task, write_tree};

pub struct Options {
    /// `<provider>/<model>`.
    pub model: String,
    pub format: EditFormat,
    /// How many times each task runs.
    pub runs: u32,
    /// The `harness` binary.
    pub harness: PathBuf,
    /// The configuration file the run's own is made from; none starts from an empty one.
    pub user_config: Option<PathBuf>,
    /// How long one run may take.
    pub timeout: Duration,
    /// Environment variables for the `harness` process, besides the ones the run sets.
    pub env: Vec<(String, String)>,
}

/// What a run's events say.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Events {
    /// Calls of an edit tool (`edit`, `write`, `apply_patch`, `hashline_edit`).
    pub edit_calls: u32,
    /// Those the tool refused.
    pub edit_errors: u32,
    /// Whether the first of them was applied; `None` when there was none.
    pub first_edit_applied: Option<bool>,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunResult {
    pub task: String,
    pub run: u32,
    pub passed: bool,
    pub events: Events,
    /// Why the run did not finish, when it did not.
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Summary {
    pub runs: usize,
    pub pass_rate: f64,
    /// Among the runs that edited: how many applied their first edit.
    pub first_try_apply_rate: f64,
    /// Refused edit calls over all edit calls.
    pub format_error_rate: f64,
    /// Edit calls that followed a refused one, per run.
    pub retries_per_run: f64,
    pub output_tokens_per_run: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub model: String,
    pub format: EditFormat,
    pub date: String,
    pub runs: Vec<RunResult>,
    pub summary: Summary,
}

const EDIT_TOOLS: [&str; 4] = ["edit", "write", "apply_patch", "hashline_edit"];

/// The metrics in the JSON event lines `harness ask --json` printed. Lines that are not events
/// are skipped.
pub fn parse_events(stdout: &str) -> Events {
    let mut events = Events::default();
    let mut editing = std::collections::HashSet::new();
    for line in stdout.lines() {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match event["type"].as_str() {
            Some("tool_call_requested") => {
                if event["name"]
                    .as_str()
                    .is_some_and(|n| EDIT_TOOLS.contains(&n))
                    && let Some(id) = event["id"].as_str()
                {
                    editing.insert(id.to_string());
                }
            }
            Some("tool_call_finished") => {
                if event["id"].as_str().is_some_and(|id| editing.contains(id)) {
                    let failed = event["is_error"].as_bool().unwrap_or(false);
                    events.edit_calls += 1;
                    events.edit_errors += u32::from(failed);
                    events.first_edit_applied.get_or_insert(!failed);
                }
            }
            Some("turn_stats") => {
                events.output_tokens += event["output_tokens"].as_u64().unwrap_or(0)
            }
            _ => {}
        }
    }
    events
}

fn rate(part: f64, whole: f64) -> f64 {
    if whole == 0.0 { 0.0 } else { part / whole }
}

pub fn summarize(runs: &[RunResult]) -> Summary {
    let n = runs.len() as f64;
    let count = |f: &dyn Fn(&RunResult) -> bool| runs.iter().filter(|r| f(r)).count() as f64;
    let edited = count(&|r| r.events.first_edit_applied.is_some());
    let first_ok = count(&|r| r.events.first_edit_applied == Some(true));
    let calls: u32 = runs.iter().map(|r| r.events.edit_calls).sum();
    let errors: u32 = runs.iter().map(|r| r.events.edit_errors).sum();
    let tokens: u64 = runs.iter().map(|r| r.events.output_tokens).sum();
    Summary {
        runs: runs.len(),
        pass_rate: rate(count(&|r| r.passed), n),
        first_try_apply_rate: rate(first_ok, edited),
        format_error_rate: rate(f64::from(errors), f64::from(calls)),
        retries_per_run: rate(f64::from(errors), n),
        output_tokens_per_run: rate(tokens as f64, n),
    }
}

/// The configuration for one run: `user` (a config file's text, if any) with the model chosen
/// and its profile's edit format set to `format`.
pub fn home_config(user: Option<&str>, model: &str, format: EditFormat) -> Result<String, String> {
    let mut table: toml::Table = match user {
        Some(text) => text
            .parse()
            .map_err(|e| format!("the configuration is not valid TOML: {e}"))?,
        None => toml::Table::new(),
    };
    table.insert("model".into(), toml::Value::String(model.into()));
    let profiles = table
        .entry("profiles")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    let toml::Value::Table(profiles) = profiles else {
        return Err("`profiles` in the configuration is not a table".into());
    };
    let profile = profiles
        .entry(model)
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    let toml::Value::Table(profile) = profile else {
        return Err(format!(
            "`profiles.{model}` in the configuration is not a table"
        ));
    };
    profile.insert(
        "edit_format".into(),
        toml::Value::String(format.to_string()),
    );
    toml::to_string(&table).map_err(|e| e.to_string())
}

/// The date `seconds` after the Unix epoch falls on, as `YYYY-MM-DD` (UTC).
pub fn civil_date(seconds: u64) -> String {
    // Days since 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's algorithm).
    let z = (seconds / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// The name a report is saved under.
pub fn result_file_name(date: &str, model: &str, format: EditFormat) -> String {
    let model: String = model
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    format!("{date}-{model}-{format}.json")
}

/// Runs every task `options.runs` times.
pub fn run(tasks: &[Task], options: &Options) -> Result<Report, String> {
    let user = match &options.user_config {
        Some(path) => Some(
            std::fs::read_to_string(path)
                .map_err(|e| format!("cannot read {}: {e}", path.display()))?,
        ),
        None => None,
    };
    let config = home_config(user.as_deref(), &options.model, options.format)?;
    // One build directory for the Rust tasks, so each run does not compile from nothing. It is
    // made fresh, with a name nobody can guess (a fixed one in the world-writable temp directory
    // could be planted), and removed at the end.
    let build = tempfile::Builder::new()
        .prefix("harness-eval-target-")
        .tempdir()
        .map_err(|e| format!("cannot make the build directory: {e}"))?;
    let target = build.path().to_path_buf();
    let mut runs = Vec::new();
    for task in tasks {
        for number in 1..=options.runs {
            runs.push(run_one(task, number, options, &config, &target));
        }
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    Ok(Report {
        model: options.model.clone(),
        format: options.format,
        date: civil_date(now),
        summary: summarize(&runs),
        runs,
    })
}

fn run_one(task: &Task, number: u32, options: &Options, config: &str, target: &Path) -> RunResult {
    let mut result = RunResult {
        task: task.id.clone(),
        run: number,
        passed: false,
        events: Events::default(),
        error: None,
    };
    if let Err(e) = attempt(task, options, config, target, &mut result) {
        result.error = Some(e);
    }
    result
}

fn attempt(
    task: &Task,
    options: &Options,
    config: &str,
    target: &Path,
    result: &mut RunResult,
) -> Result<(), String> {
    let workspace = tempfile::tempdir().map_err(|e| e.to_string())?;
    let home = tempfile::tempdir().map_err(|e| e.to_string())?;
    write_tree(workspace.path(), &task.before)?;
    std::fs::create_dir_all(home.path().join("config")).map_err(|e| e.to_string())?;
    std::fs::write(home.path().join("config/config.toml"), config).map_err(|e| e.to_string())?;
    let mut command = Command::new(&options.harness);
    // In a group of its own, so that a run that has to be stopped takes what it started with it
    // (shell commands, language servers).
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    command
        .args(["--mode", "auto", "ask", "--json", &task.prompt])
        .current_dir(workspace.path())
        .env("HARNESS_HOME", home.path())
        // Credentials stored by `harness login` or `harness auth add` are not carried over.
        .env("HARNESS_CREDENTIAL_STORE", "file")
        .env("CARGO_TARGET_DIR", target)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for (key, value) in &options.env {
        command.env(key, value);
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("cannot start {}: {e}", options.harness.display()))?;
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        text
    });
    let deadline = Instant::now() + options.timeout;
    let timed_out = loop {
        match child.try_wait() {
            Ok(Some(_)) => break false,
            Ok(None) if Instant::now() >= deadline => {
                let _ = nix::sys::signal::killpg(
                    nix::unistd::Pid::from_raw(child.id() as i32),
                    nix::sys::signal::Signal::SIGKILL,
                );
                let _ = child.kill();
                let _ = child.wait();
                break true;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(e.to_string()),
        }
    };
    let text = reader.join().unwrap_or_default();
    result.events = parse_events(&text);
    if timed_out {
        result.error = Some(format!("timed out after {} s", options.timeout.as_secs()));
    }
    // The task's own test decides, whatever the model said and however the run ended.
    result.passed = crate::oracle::passes_in(workspace.path(), &task.test, target)?;
    Ok(())
}

/// Writes `report` as JSON in `dir`, named by date, model and format; the path.
pub fn save(report: &Report, dir: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let path = dir.join(result_file_name(&report.date, &report.model, report.format));
    let mut text = serde_json::to_string_pretty(report).map_err(|e| e.to_string())?;
    text.push('\n');
    std::fs::write(&path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(path)
}

impl Report {
    /// The summary as a table, for the terminal.
    pub fn table(&self) -> String {
        let s = &self.summary;
        format!(
            "{} with {} ({} runs)\n  pass rate            {:.0}%\n  first-try apply      {:.0}%\n  format-error rate    {:.0}%\n  retries per run      {:.2}\n  output tokens per run {:.0}\n",
            self.model,
            self.format,
            s.runs,
            s.pass_rate * 100.0,
            s.first_try_apply_rate * 100.0,
            s.format_error_rate * 100.0,
            s.retries_per_run,
            s.output_tokens_per_run
        )
    }
}
