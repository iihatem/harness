//! What the terminal says about subagents: the line a child leaves in the transcript when it
//! ends, and the `/agents` listing.

use harness_core::agents::{AgentInfo, AgentStatus, Definitions};
use ratatui::text::{Line, Span};

use crate::{style::Theme, text::sanitize};

/// The longest part of a child's report shown on its result line, in characters.
const SUMMARY_SHOWN: usize = 100;

/// `count` calls; a child that finished "done" without any is marked as unchecked, since nothing
/// it says was looked up.
fn calls(count: u32, status: AgentStatus) -> String {
    match count {
        0 if status == AgentStatus::Done => "no tool calls (unchecked)".into(),
        0 => "no tool calls".into(),
        1 => "1 tool call".into(),
        n => format!("{n} tool calls"),
    }
}

/// `text` cut to `cap` characters.
fn cut(text: &str, cap: usize) -> String {
    match text.char_indices().nth(cap) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_string(),
    }
}

/// The line for a child that has ended: who, how it ended, what it did, and the start of what it
/// said or why it failed. A child that was "done" without calling a tool is said to have called
/// none.
pub fn result_line(child: &AgentInfo) -> String {
    let mut line = format!(
        "◦ {} {} · {}",
        sanitize(child.tag.id.as_str()),
        sanitize(&cut(&child.tag.label, 40)),
        child.status.as_str()
    );
    if let Some(model) = &child.model {
        line.push_str(&format!(" · {}", sanitize(&cut(model, 40))));
    }
    if child.status != AgentStatus::Cancelled || child.tool_calls > 0 {
        line.push_str(&format!(" · {}", calls(child.tool_calls, child.status)));
    }
    let detail = match child.status {
        AgentStatus::Failed => child.error.as_deref(),
        _ => child.summary.as_deref(),
    };
    if let Some(first) = detail.and_then(|d| d.lines().map(str::trim).find(|l| !l.is_empty())) {
        line.push_str(&format!(" — {}", sanitize(&cut(first, SUMMARY_SHOWN))));
    }
    line
}

/// `/agents`: the agents the model can start, each with where it came from and what it gets, and
/// the children this session has run.
pub fn report(
    definitions: Option<&Definitions>,
    children: &[AgentInfo],
    results: &[harness_core::worktree::Record],
    theme: &Theme,
    redact: &dyn Fn(&str) -> String,
) -> Vec<Line<'static>> {
    // Everything that came from a file or a model is redacted, then made safe to print.
    let sanitize = |text: &str| sanitize(&redact(text));
    let mut lines = vec![Line::from(Span::styled(
        "Agents the model can start",
        theme.bold(),
    ))];
    match definitions {
        None => lines.push(Line::from(Span::styled(
            "  (none: this session cannot start subagents)",
            theme.dim(),
        ))),
        Some(definitions) => {
            for def in definitions.iter() {
                lines.push(Line::from(vec![
                    Span::styled(format!("  {}", sanitize(&def.name)), theme.accent()),
                    Span::raw(format!("  {}", sanitize(&def.description))),
                ]));
                lines.push(Line::from(Span::styled(
                    format!("    from {}", sanitize(&def.source.label())),
                    theme.dim(),
                )));
                let mut what = Vec::new();
                if let Some(tools) = &def.tools {
                    what.push(format!("tools {}", tools.join(", ")));
                }
                if let Some(mode) = def.mode {
                    what.push(format!("mode {mode}"));
                }
                if let Some(model) = &def.model {
                    what.push(format!("model {}", sanitize(model)));
                } else if def.model_inherit {
                    what.push("model: the parent's".into());
                }
                if let Some(steps) = def.max_steps {
                    what.push(format!("{steps} steps at most"));
                }
                if !what.is_empty() {
                    lines.push(Line::from(Span::styled(
                        format!("    {}", what.join(" · ")),
                        theme.dim(),
                    )));
                }
                if def.background_ignored {
                    lines.push(Line::from(Span::styled(
                        "    background ignored: workspace not trusted",
                        theme.warning(),
                    )));
                }
                if let Some(model) = &def.model_ignored {
                    lines.push(Line::from(Span::styled(
                        format!(
                            "    model {} ignored: workspace not trusted",
                            sanitize(model)
                        ),
                        theme.warning(),
                    )));
                }
            }
        }
    }
    lines.push(Line::from(Span::styled(
        "Subagents in this session",
        theme.bold(),
    )));
    if children.is_empty() {
        lines.push(Line::from(Span::styled(
            "  No subagents have run in this session.",
            theme.dim(),
        )));
    }
    for child in children {
        let mut text = format!(
            "  {} {} · {}",
            sanitize(child.tag.id.as_str()),
            sanitize(&cut(&child.tag.label, 40)),
            child.status.as_str()
        );
        if let Some(model) = &child.model {
            text.push_str(&format!(" · {}", sanitize(&cut(model, 40))));
        }
        if child.background {
            text.push_str(" · background");
        }
        text.push_str(&format!(" · {}", calls(child.tool_calls, child.status)));
        if child.status == AgentStatus::Failed
            && let Some(error) = &child.error
        {
            text.push_str(&format!(" — {}", sanitize(&cut(error, SUMMARY_SHOWN))));
        }
        lines.push(Line::from(text));
    }
    // What the worktrees hold: results waiting for review, from this session and earlier ones.
    let ready: Vec<&harness_core::worktree::Record> = results
        .iter()
        .filter(|r| crate::review::is_ready(r))
        .collect();
    if !ready.is_empty() {
        lines.push(Line::from(Span::styled(
            "Results waiting for review (/results)",
            theme.bold(),
        )));
        for record in ready {
            lines.push(Line::from(format!(
                "  {}",
                sanitize(&redact(&crate::review::describe(record)))
            )));
        }
    }
    lines
}
