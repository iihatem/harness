//! What a child agent hands back: a capped, structured report. The facts in it (files changed,
//! commands run, how many tools it called) come from the child's tool log, never from what the
//! child says it did.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::agents::AgentStatus;

/// The longest summary a report holds, in characters.
pub const SUMMARY_CAP: usize = 4_000;
/// The most files, and the most commands, a report lists.
pub const LIST_CAP: usize = 50;
/// The longest a listed command is, in characters.
pub const COMMAND_CAP: usize = 200;

/// What the tools a child called did, as the agent saw them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolLog {
    /// Tool calls made, those that did not run included.
    pub calls: u32,
    /// The files tools that succeeded say they changed, in order, without repeats.
    pub files_changed: Vec<PathBuf>,
    /// The shell commands that ran, in order.
    pub commands: Vec<String>,
}

/// Something about a report the reader should weigh.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportFlag {
    /// The child ended "done" without calling a tool: whatever it says it did, nothing ran.
    NoToolCalls,
    /// The summary was cut at [`SUMMARY_CAP`].
    SummaryTruncated,
    /// A list was cut at [`LIST_CAP`] entries, or a command at [`COMMAND_CAP`] characters.
    ListTruncated,
}

/// What a child's model requests used.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReportUsage {
    /// Model requests made.
    pub requests: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    /// What was billed; `None` when a request had no price, so it is never a sum that left one
    /// out.
    pub billed_usd: Option<f64>,
    /// What the same tokens cost at the table's price (an estimate); `None` likewise.
    pub list_usd: Option<f64>,
}

/// Adds up what a child's requests used, as its [`ScopedMeter`](super::ScopedMeter) sees them.
#[derive(Debug, Clone, Default)]
pub struct UsageTally(std::sync::Arc<std::sync::Mutex<Tally>>);

#[derive(Debug, Default)]
struct Tally {
    usage: ReportUsage,
    /// A request with no price was seen.
    billed_unknown: bool,
    list_unknown: bool,
}

impl UsageTally {
    pub(crate) fn add(&self, usage: &crate::message::Usage, cost: &crate::meter::RequestCost) {
        let mut tally = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        tally.usage.requests += 1;
        tally.usage.input_tokens += usage.input_tokens;
        tally.usage.output_tokens += usage.output_tokens;
        tally.usage.cached_tokens += usage.cached_tokens;
        match cost.billed_usd {
            Some(usd) => *tally.usage.billed_usd.get_or_insert(0.0) += usd,
            None => tally.billed_unknown = true,
        }
        match cost.list_usd {
            Some(usd) => *tally.usage.list_usd.get_or_insert(0.0) += usd,
            None => tally.list_unknown = true,
        }
    }

    /// What was used so far; `None` when no request was made.
    pub fn snapshot(&self) -> Option<ReportUsage> {
        let tally = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if tally.usage.requests == 0 {
            return None;
        }
        let mut usage = tally.usage.clone();
        if tally.billed_unknown {
            usage.billed_usd = None;
        }
        if tally.list_unknown {
            usage.list_usd = None;
        }
        Some(usage)
    }
}

/// A child's result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentReport {
    pub agent: String,
    pub label: String,
    /// `done`, `failed` or `cancelled`.
    pub status: AgentStatus,
    /// The child's final text, capped.
    pub summary: String,
    pub files_changed: Vec<String>,
    pub commands_run: Vec<String>,
    pub tool_calls: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flags: Vec<ReportFlag>,
    /// Why it failed or stopped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The child's own session, where its whole transcript is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// What its model requests used, when a meter saw them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<ReportUsage>,
}

/// `text` cut to `cap` characters; whether it was cut.
pub fn cap_chars(text: &str, cap: usize) -> (String, bool) {
    match text.char_indices().nth(cap) {
        Some((end, _)) => (format!("{}…", &text[..end]), true),
        None => (text.to_string(), false),
    }
}

impl AgentReport {
    /// The report for a child that ended as `status`, from its `summary` (its last text), its
    /// tool `log` and the `workspace` its files are shown relative to.
    pub fn build(
        agent: &str,
        label: &str,
        status: AgentStatus,
        summary: &str,
        log: &ToolLog,
        workspace: &Path,
    ) -> AgentReport {
        let mut flags = Vec::new();
        let (summary, cut) = cap_chars(summary.trim(), SUMMARY_CAP);
        if cut {
            flags.push(ReportFlag::SummaryTruncated);
        }
        // Tools resolve paths through symlinks, so the workspace is shown that way too.
        let canonical = workspace
            .canonicalize()
            .unwrap_or_else(|_| workspace.to_path_buf());
        let relative = |path: &PathBuf| {
            path.strip_prefix(workspace)
                .or_else(|_| path.strip_prefix(&canonical))
                .unwrap_or(path)
                .display()
                .to_string()
        };
        let mut list_cut = log.files_changed.len() > LIST_CAP || log.commands.len() > LIST_CAP;
        let files_changed = log
            .files_changed
            .iter()
            .take(LIST_CAP)
            .map(relative)
            .collect();
        let commands_run = log
            .commands
            .iter()
            .take(LIST_CAP)
            .map(|command| {
                let (command, cut) = cap_chars(command, COMMAND_CAP);
                list_cut |= cut;
                command
            })
            .collect();
        if list_cut {
            flags.push(ReportFlag::ListTruncated);
        }
        if status == AgentStatus::Done && log.calls == 0 {
            flags.push(ReportFlag::NoToolCalls);
        }
        AgentReport {
            agent: agent.to_string(),
            label: label.to_string(),
            status,
            summary,
            files_changed,
            commands_run,
            tool_calls: log.calls,
            flags,
            error: None,
            session: None,
            usage: None,
        }
    }
}
