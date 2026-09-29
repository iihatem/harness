//! The conversation as the user sees it: what the agent reports turns into lines for the
//! scrollback once each part is finished, and what is still streaming or running is shown in
//! the live region.

use std::collections::HashMap;

use harness_core::event::{AgentEvent, TurnEndReason};
use ratatui::text::{Line, Span};
use serde_json::Value;

use crate::{
    diff, markdown,
    style::Theme,
    text::{lines, sanitize, wrap},
};

/// Lines of a tool's output shown in the transcript; the rest is summarized.
const OUTPUT_LINES: usize = 6;
/// Lines of an edit's diff shown in the transcript.
const DIFF_LINES: usize = 20;

/// A tool call that has not finished.
#[derive(Debug, Clone)]
struct Call {
    name: String,
    arguments: Value,
}

/// The conversation's finished lines, waiting to go into scrollback, and what is in progress.
pub struct Transcript {
    theme: Theme,
    /// Finished lines not yet written to the terminal.
    pending: Vec<Line<'static>>,
    /// The assistant's reply as it streams.
    streaming: String,
    /// The model is reasoning (its reasoning is not shown).
    thinking: bool,
    calls: HashMap<String, Call>,
    /// The call running now, for the live region.
    running: Option<String>,
    /// Whether a turn is running.
    busy: bool,
    /// Whether the last finished line is blank, or there is none yet.
    blank: bool,
}

impl Transcript {
    pub fn new(theme: Theme) -> Transcript {
        Transcript {
            theme,
            pending: Vec::new(),
            streaming: String::new(),
            thinking: false,
            calls: HashMap::new(),
            running: None,
            busy: false,
            blank: true,
        }
    }

    pub fn theme(&self) -> &Theme {
        &self.theme
    }

    /// Whether a turn is running.
    pub fn busy(&self) -> bool {
        self.busy
    }

    /// The finished lines not yet written, which are then forgotten.
    pub fn take_finished(&mut self) -> Vec<Line<'static>> {
        std::mem::take(&mut self.pending)
    }

    /// Adds `lines` to the finished lines.
    fn emit(&mut self, lines: impl IntoIterator<Item = Line<'static>>) {
        for line in lines {
            self.blank = line.spans.iter().all(|s| s.content.trim().is_empty());
            self.pending.push(line);
        }
    }

    /// A blank line between one part of the conversation and the next.
    fn gap(&mut self) {
        if !self.blank {
            self.emit([Line::default()]);
        }
    }

    /// Adds `lines`, wrapped to `width`, to the finished lines.
    pub fn push_lines(&mut self, lines: Vec<Line<'static>>, width: usize) {
        for line in lines {
            self.emit(wrap(&line, width, &[], &[]));
        }
    }

    /// What the user sent.
    pub fn push_user(&mut self, text: &str, width: usize) {
        self.gap();
        let style = self.theme.user();
        let marker = Span::styled("› ", self.theme.accent());
        for (i, line) in lines(text, style).into_iter().enumerate() {
            let first = if i == 0 {
                vec![marker.clone()]
            } else {
                vec![Span::raw("  ")]
            };
            self.emit(wrap(&line, width, &first, &[Span::raw("  ")]));
        }
    }

    /// A note from harness, dim.
    pub fn push_note(&mut self, text: &str, width: usize) {
        let style = self.theme.dim();
        self.push_lines(lines(text, style), width);
    }

    pub fn push_warning(&mut self, text: &str, width: usize) {
        let style = self.theme.warning();
        self.push_lines(lines(&format!("warning: {text}"), style), width);
    }

    pub fn push_error(&mut self, text: &str, width: usize) {
        let style = self.theme.error();
        self.push_lines(lines(&format!("error: {text}"), style), width);
    }

    /// Takes in one event from the agent. `width` is the screen's width.
    pub fn on_event(&mut self, event: &AgentEvent, width: usize) {
        match event {
            AgentEvent::TurnStarted => {
                self.busy = true;
                self.streaming.clear();
            }
            AgentEvent::TextDelta { text } => {
                self.thinking = false;
                self.streaming.push_str(text);
            }
            AgentEvent::ReasoningDelta { .. } => self.thinking = true,
            AgentEvent::AssistantMessage { content, .. } => {
                self.thinking = false;
                self.streaming.clear();
                if !content.trim().is_empty() {
                    self.gap();
                    let rendered = markdown::render(content, width, &self.theme);
                    self.emit(rendered);
                }
            }
            AgentEvent::ToolCallRequested {
                id,
                name,
                arguments,
            } => {
                let arguments = serde_json::from_str(arguments).unwrap_or(Value::Null);
                self.calls.insert(
                    id.clone(),
                    Call {
                        name: name.clone(),
                        arguments,
                    },
                );
                self.running = Some(id.clone());
            }
            AgentEvent::ToolCallFinished {
                id,
                output,
                is_error,
            } => {
                if self.running.as_ref() == Some(id) {
                    self.running = None;
                }
                let call = self.calls.remove(id).unwrap_or(Call {
                    name: "tool".into(),
                    arguments: Value::Null,
                });
                self.finish_call(&call, output, *is_error, width);
            }
            AgentEvent::ActionBlocked { reason, .. } => {
                let style = self.theme.warning();
                self.push_lines(lines(&format!("blocked: {reason}"), style), width);
            }
            AgentEvent::Retrying {
                attempt,
                reason,
                delay_ms,
            } => {
                let text = format!(
                    "retrying (attempt {attempt}) in {:.1}s: {reason}",
                    *delay_ms as f64 / 1000.0
                );
                self.push_note(&text, width);
            }
            AgentEvent::Warning { message } => self.push_warning(message, width),
            AgentEvent::Error { message, .. } => self.push_error(message, width),
            AgentEvent::Compacted {
                summary,
                tokens_before,
                tokens_after,
            } => {
                self.gap();
                self.push_note(
                    &format!(
                        "compacted the conversation from about {tokens_before} to {tokens_after} tokens; summary:"
                    ),
                    width,
                );
                let rendered = markdown::render(summary, width, &self.theme);
                self.emit(rendered);
            }
            AgentEvent::TurnFinished { reason } => {
                self.busy = false;
                self.thinking = false;
                self.running = None;
                if !self.streaming.trim().is_empty() {
                    // Output that never became a message: keep what arrived.
                    let text = std::mem::take(&mut self.streaming);
                    self.gap();
                    let rendered = markdown::render(&text, width, &self.theme);
                    self.emit(rendered);
                }
                self.streaming.clear();
                match reason {
                    TurnEndReason::Interrupted => self.push_note("interrupted", width),
                    TurnEndReason::StepLimit => {
                        self.push_error("stopped after reaching the step limit", width)
                    }
                    TurnEndReason::Completed | TurnEndReason::Error => {}
                }
            }
            AgentEvent::TurnStats {
                model,
                time_to_first_token_ms,
                generation_ms,
                input_tokens,
                output_tokens,
                cached_tokens,
            } => {
                let line = crate::status::stats_line(
                    model,
                    *time_to_first_token_ms,
                    *generation_ms,
                    *input_tokens,
                    *output_tokens,
                    *cached_tokens,
                    &self.theme,
                );
                self.gap();
                self.push_lines(vec![line], width);
            }
            AgentEvent::ApprovalNeeded { .. }
            | AgentEvent::Usage { .. }
            | AgentEvent::CheckpointCreated { .. } => {}
        }
    }

    /// The lines for a finished tool call: what it did, then a short look at its result.
    fn finish_call(&mut self, call: &Call, output: &str, is_error: bool, width: usize) {
        self.gap();
        let header = call_summary(&call.name, &call.arguments);
        let marker = Span::styled("● ", self.theme.accent());
        let style = if is_error {
            self.theme.error()
        } else {
            self.theme.bold()
        };
        let line = Line::from(Span::styled(sanitize(&header).replace('\n', " "), style));
        self.emit(wrap(&line, width, &[marker], &[Span::raw("  ")]));
        let indent = [Span::raw("  ")];
        let body: Vec<Line<'static>> = match call.name.as_str() {
            "edit" if !is_error => {
                let old = call.arguments["old_string"].as_str().unwrap_or_default();
                let new = call.arguments["new_string"].as_str().unwrap_or_default();
                let mut diff = diff::unified(old, new, 3, &self.theme);
                // The hunk header's line numbers are the snippet's, not the file's.
                diff.retain(|l| !l.spans.iter().any(|s| s.content.starts_with("@@")));
                clip(diff, DIFF_LINES, &self.theme)
            }
            "read" | "write" if !is_error => Vec::new(),
            _ => clip(
                lines(output.trim_end(), self.theme.dim()),
                OUTPUT_LINES,
                &self.theme,
            ),
        };
        for line in body {
            self.emit(wrap(&line, width, &indent, &indent));
        }
    }

    /// What the live region shows of the turn in progress, at most `rows` lines: the tail of the
    /// reply streaming in, and what is running.
    pub fn live(&self, width: usize, rows: usize) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        if !self.streaming.is_empty() {
            out = markdown::render(&self.streaming, width, &self.theme);
        }
        if let Some(call) = self.running.as_ref().and_then(|id| self.calls.get(id)) {
            let summary = call_summary(&call.name, &call.arguments);
            let line = Line::from(vec![
                Span::styled("● ", self.theme.accent()),
                Span::styled(sanitize(&summary).replace('\n', " "), self.theme.dim()),
            ]);
            out.extend(wrap(&line, width, &[], &[Span::raw("  ")]));
        } else if self.thinking {
            out.push(Line::from(Span::styled("thinking…", self.theme.dim())));
        }
        let skip = out.len().saturating_sub(rows);
        out.split_off(skip)
    }
}

/// At most `max` of `lines`, then a line saying how many more there are.
fn clip(mut lines: Vec<Line<'static>>, max: usize, theme: &Theme) -> Vec<Line<'static>> {
    if lines.len() > max {
        let more = lines.len() - max;
        lines.truncate(max);
        lines.push(Line::from(Span::styled(
            format!("… {more} more line{}", if more == 1 { "" } else { "s" }),
            theme.dim(),
        )));
    }
    lines
}

/// One line saying what a tool call does, from its arguments.
pub fn call_summary(name: &str, args: &Value) -> String {
    let text = |key: &str| args[key].as_str().unwrap_or_default().to_string();
    match name {
        "bash" => format!("$ {}", text("command").lines().next().unwrap_or_default()),
        "read" => format!("read {}", text("path")),
        "write" => {
            let lines = args["content"].as_str().map_or(0, |c| c.lines().count());
            format!("write {} ({lines} lines)", text("path"))
        }
        "edit" => format!("edit {}", text("path")),
        "grep" => match args["path"].as_str() {
            Some(path) => format!("grep {} in {path}", text("pattern")),
            None => format!("grep {}", text("pattern")),
        },
        "glob" => format!("glob {}", text("pattern")),
        other => {
            let shown: String = args.to_string().chars().take(80).collect();
            format!("{other} {shown}")
        }
    }
}
