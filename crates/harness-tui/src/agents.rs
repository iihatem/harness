//! What the terminal says about subagents: the line a child leaves in the transcript when it
//! ends, and the `/agents` listing.

use harness_core::agents::{AgentInfo, AgentStatus, Definitions};
use ratatui::text::{Line, Span};

use crate::{style::Theme, text::sanitize};

/// The longest part of a child's report shown on its result line, in characters.
const SUMMARY_SHOWN: usize = 100;

fn calls(count: u32) -> String {
    match count {
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
    if child.status != AgentStatus::Cancelled || child.tool_calls > 0 {
        line.push_str(&format!(" · {}", calls(child.tool_calls)));
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
    theme: &Theme,
) -> Vec<Line<'static>> {
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
        text.push_str(&format!(" · {}", calls(child.tool_calls)));
        if child.status == AgentStatus::Failed
            && let Some(error) = &child.error
        {
            text.push_str(&format!(" — {}", sanitize(&cut(error, SUMMARY_SHOWN))));
        }
        lines.push(Line::from(text));
    }
    lines
}
