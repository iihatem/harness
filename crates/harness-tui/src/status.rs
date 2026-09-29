//! The numbers the session shows: the status line, the stats after each turn, `/context` and
//! `/usage`.

use std::collections::BTreeMap;

use harness_core::{agent::ContextUsage, message::Usage, permission::Mode};
use ratatui::text::{Line, Span};

use crate::{style::Theme, text::sanitize};

/// `n` tokens, short: `950`, `1.2k`, `34k`, `1.5M`.
pub fn tokens(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..10_000 => format!("{:.1}k", n as f64 / 1_000.0),
        10_000..1_000_000 => format!("{}k", n / 1_000),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

/// `n` with thousands separators: `24,353`.
pub fn thousands(n: u64) -> String {
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

/// `part` as a percentage of `whole`, one decimal.
fn percent(part: u64, whole: u64) -> String {
    if whole == 0 {
        return "0%".into();
    }
    format!("{:.1}%", part as f64 * 100.0 / whole as f64)
}

/// The session's token counts from the provider, per model.
#[derive(Debug, Clone, Default)]
pub struct Totals {
    per_model: BTreeMap<String, Usage>,
}

impl Totals {
    pub fn add(&mut self, model: &str, usage: &Usage) {
        let total = self.per_model.entry(model.to_string()).or_default();
        total.input_tokens += usage.input_tokens;
        total.output_tokens += usage.output_tokens;
        total.cached_tokens += usage.cached_tokens;
    }

    /// Input and output tokens over every model.
    pub fn sum(&self) -> (u64, u64) {
        self.per_model.values().fold((0, 0), |(i, o), u| {
            (i + u.input_tokens, o + u.output_tokens)
        })
    }

    /// `/usage`: input, output and cached tokens per model.
    pub fn report(&self, theme: &Theme) -> Vec<Line<'static>> {
        if self.per_model.is_empty() {
            return vec![Line::from(Span::styled(
                "No tokens used yet in this session.",
                theme.dim(),
            ))];
        }
        let width = self
            .per_model
            .keys()
            .map(|m| m.chars().count())
            .max()
            .unwrap_or(0)
            .max(5);
        let mut lines = vec![Line::from(Span::styled(
            format!(
                "{:width$}  {:>10}  {:>10}  {:>10}",
                "model", "input", "output", "cached"
            ),
            theme.bold(),
        ))];
        for (model, usage) in &self.per_model {
            lines.push(Line::from(format!(
                "{:width$}  {:>10}  {:>10}  {:>10}",
                sanitize(model),
                thousands(usage.input_tokens),
                thousands(usage.output_tokens),
                thousands(usage.cached_tokens),
            )));
        }
        lines.push(Line::from(Span::styled(
            "Counted since harness started, as each provider reported them.",
            theme.dim(),
        )));
        lines
    }
}

/// The status line: the model, the approval mode, how full the context window is, and the
/// session's tokens.
pub fn status_line(
    model: &str,
    mode: Mode,
    context: &ContextUsage,
    totals: &Totals,
    theme: &Theme,
) -> Line<'static> {
    let (input, output) = totals.sum();
    let used = if context.window == 0 {
        0
    } else {
        (context.total * 100).div_ceil(context.window)
    };
    let mut spans = vec![Span::styled(
        format!(
            "{} · {mode} · {used}% of context · {} in, {} out",
            sanitize(model),
            tokens(input),
            tokens(output)
        ),
        theme.dim(),
    )];
    if mode == Mode::FullAccess {
        spans.push(Span::styled(
            " · full-access: no sandbox, no approvals",
            theme.error(),
        ));
    }
    Line::from(spans)
}

/// The dim line after a turn: the model that answered, time to first token, output tokens per
/// second, and the prompt-cache hit rate when the provider reported cached tokens.
pub fn stats_line(
    model: &str,
    time_to_first_token_ms: Option<u64>,
    generation_ms: u64,
    input_tokens: u64,
    output_tokens: u64,
    cached_tokens: u64,
    theme: &Theme,
) -> Line<'static> {
    let mut parts = vec![sanitize(model)];
    if let Some(ms) = time_to_first_token_ms {
        parts.push(format!("first token {:.1}s", ms as f64 / 1_000.0));
    }
    if output_tokens > 0 && generation_ms > 0 {
        let rate = output_tokens as f64 * 1_000.0 / generation_ms as f64;
        parts.push(format!("{rate:.0} tok/s"));
    }
    if cached_tokens > 0 && input_tokens > 0 {
        parts.push(format!("cache {}", percent(cached_tokens, input_tokens)));
    }
    Line::from(Span::styled(parts.join(" · "), theme.dim()))
}

/// `/context`: the context window, and how the next request fills it: the system prompt, the
/// tool definitions, each instruction file (its tokens are part of the system prompt), the
/// conversation, and what is free.
pub fn context_report(
    context: &ContextUsage,
    instruction_files: &[(String, u64)],
    window_note: Option<&str>,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let files: u64 = instruction_files.iter().map(|(_, t)| *t).sum();
    let mut rows: Vec<(String, u64)> = vec![
        ("system prompt".into(), context.system.saturating_sub(files)),
        ("tool definitions".into(), context.tools),
    ];
    rows.extend(
        instruction_files
            .iter()
            .map(|(name, tokens)| (sanitize(name), *tokens)),
    );
    rows.push(("conversation".into(), context.messages));
    let used = context.system + context.tools + context.messages;
    rows.push(("free".into(), context.window.saturating_sub(used)));
    let mut title = format!("Context window: {} tokens", thousands(context.window));
    if let Some(note) = window_note {
        title.push_str(&format!(" ({})", sanitize(note)));
    }
    let name_width = rows
        .iter()
        .map(|(n, _)| n.chars().count())
        .max()
        .unwrap_or(0);
    let mut lines = vec![Line::from(Span::styled(title, theme.bold()))];
    for (name, tokens) in rows {
        lines.push(Line::from(vec![
            Span::raw(format!("  {name:name_width$}  ")),
            Span::raw(format!("{:>9}", thousands(tokens))),
            Span::styled(
                format!("  {:>6}", percent(tokens, context.window)),
                theme.dim(),
            ),
        ]));
    }
    lines
}
