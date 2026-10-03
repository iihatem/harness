//! The numbers the session shows: the status line, the stats after each turn, `/context` and
//! `/usage`.

use std::collections::BTreeMap;

use harness_core::{
    agent::ContextUsage,
    message::Usage,
    meter::{AccountKind, Avoided, RequestCost, WindowSnapshot},
    permission::Mode,
    time::civil_date,
};
use ratatui::text::{Line, Span};

use crate::{style::Theme, text::sanitize, usage::UsageContext};

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
    status_line_with(model, mode, context, totals, &Extras::default(), theme)
}

/// What the status line shows after the tokens: the session's billed cost once it made a billed
/// request, and, for a subscription provider, the most-used usage window.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Extras {
    pub cost: Option<String>,
    pub window: Option<String>,
}

/// [`status_line`], with the cost and window after the tokens.
pub fn status_line_with(
    model: &str,
    mode: Mode,
    context: &ContextUsage,
    totals: &Totals,
    extras: &Extras,
    theme: &Theme,
) -> Line<'static> {
    let (input, output) = totals.sum();
    // Nearest, not always up: rounding up made 3.07% read as "4%", which the status line then
    // disagreed with itself about once anyone did the arithmetic.
    let used = if context.window == 0 {
        0
    } else {
        (context.total as f64 * 100.0 / context.window as f64).round() as u64
    };
    let mut text = format!(
        "{} · {mode} · {used}% of context · {} in, {} out",
        sanitize(model),
        tokens(input),
        tokens(output)
    );
    for extra in [&extras.cost, &extras.window].into_iter().flatten() {
        text.push_str(" · ");
        text.push_str(extra);
    }
    let mut spans = vec![Span::styled(text, theme.dim())];
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
    // The status line's percentage is `context.total`: the provider-reported input, when there
    // is one, plus estimates since. "free" must agree with it, rather than with the sum of the
    // rows above, which are always estimates and can disagree with what the provider reported.
    let estimated = context.system + context.tools + context.messages;
    rows.push(("free".into(), context.window.saturating_sub(context.total)));
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
    // The rows above are estimates; once the provider has reported the last request's input
    // tokens, the actual next request can differ from their sum, and this says by how much,
    // matching the status line rather than leaving two disagreeing numbers on screen.
    if context.total != estimated {
        lines.push(Line::from(Span::styled(
            format!(
                "Next request \u{2248} {} tokens ({}), as the provider reported; the rows above are estimates.",
                thousands(context.total),
                percent(context.total, context.window),
            ),
            theme.dim(),
        )));
    }
    lines
}

/// An amount in USD: `$1.25`, and `$0.0013` when a cent would hide it.
pub fn usd(amount: f64) -> String {
    if amount == 0.0 || amount >= 0.01 {
        format!("${amount:.2}")
    } else {
        format!("${amount:.4}")
    }
}

/// A sum of money and how many requests had no price (left out of it, never counted as 0).
fn money(usd_sum: f64, unknown: u64) -> String {
    match (unknown, usd_sum == 0.0) {
        (0, _) => usd(usd_sum),
        (_, true) => "price unknown".to_string(),
        (n, false) => format!("{} + {n} price unknown", usd(usd_sum)),
    }
}

/// What the session's requests cost on one model.
#[derive(Debug, Clone, Copy, Default)]
struct ModelCost {
    /// Requests paid for with an API key.
    billed_requests: u64,
    billed: f64,
    billed_unknown: u64,
    list: f64,
    list_unknown: u64,
    avoided: f64,
    avoided_unknown: u64,
    /// Requests the avoided figure applies to.
    avoided_requests: u64,
}

/// The session's cost figures, per model, from the runtime's metered events. The three figures
/// are never added together.
#[derive(Debug, Clone, Default)]
pub struct Costs {
    per_model: BTreeMap<String, ModelCost>,
}

impl Costs {
    pub fn add(&mut self, model: &str, cost: &RequestCost) {
        let entry = self.per_model.entry(model.to_string()).or_default();
        if cost.account == AccountKind::ApiKey {
            entry.billed_requests += 1;
            match cost.billed_usd {
                Some(usd) => entry.billed += usd,
                None => entry.billed_unknown += 1,
            }
        }
        match cost.list_usd {
            Some(usd) => entry.list += usd,
            None => entry.list_unknown += 1,
        }
        match cost.avoided {
            Avoided::NotApplicable => {}
            Avoided::Unknown => {
                entry.avoided_requests += 1;
                entry.avoided_unknown += 1;
            }
            Avoided::Usd(usd) => {
                entry.avoided_requests += 1;
                entry.avoided += usd;
            }
        }
    }

    fn sum(&self) -> ModelCost {
        self.per_model
            .values()
            .fold(ModelCost::default(), |a, m| ModelCost {
                billed_requests: a.billed_requests + m.billed_requests,
                billed: a.billed + m.billed,
                billed_unknown: a.billed_unknown + m.billed_unknown,
                list: a.list + m.list,
                list_unknown: a.list_unknown + m.list_unknown,
                avoided: a.avoided + m.avoided,
                avoided_unknown: a.avoided_unknown + m.avoided_unknown,
                avoided_requests: a.avoided_requests + m.avoided_requests,
            })
    }

    /// Whether any request was metered.
    pub fn is_empty(&self) -> bool {
        self.per_model.is_empty()
    }

    /// The billed cost for the status line: shown once the session has made a billed request,
    /// and `price unknown` when a billed request had no price.
    pub fn status(&self) -> Option<String> {
        let total = self.sum();
        (total.billed_requests > 0).then(|| money(total.billed, total.billed_unknown))
    }
}

/// `5h 62%` for the status line: the most-used window by its length, `(stale)` when it was
/// observed more than 15 minutes before `now`, and `window unknown` when nothing is known: never 0%.
pub fn window_segment(snapshot: Option<&WindowSnapshot>, now: u64) -> String {
    match snapshot.and_then(|s| s.most_used().map(|w| (s, w))) {
        Some((snapshot, window)) => {
            let used = window.used_percent.unwrap_or(0.0);
            let stale = if snapshot.is_stale(now) {
                " (stale)"
            } else {
                ""
            };
            format!("{} {used:.0}%{stale}", window.label())
        }
        None => "window unknown".to_string(),
    }
}

/// When a window resets: `14:30 UTC` within a day of `now`, else with its date.
pub fn reset_text(resets_at: u64, now: u64) -> String {
    let secs = resets_at % 86_400;
    let time = format!("{:02}:{:02} UTC", secs / 3_600, secs / 60 % 60);
    if resets_at.abs_diff(now) < 86_400 && civil_date(resets_at) == civil_date(now) {
        time
    } else {
        format!("{} {time}", civil_date(resets_at))
    }
}

/// The lines for the subscription windows, one for each window: its length, how much is used and
/// when it resets; or that they are unknown.
pub fn window_lines(
    snapshot: Option<&WindowSnapshot>,
    now: u64,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled(
        "Subscription windows",
        theme.bold(),
    ))];
    let windows = snapshot.filter(|s| !s.windows.is_empty());
    let Some(snapshot) = windows else {
        lines.push(Line::from("  window unknown"));
        return lines;
    };
    for window in &snapshot.windows {
        let used = window
            .used_percent
            .map_or("usage unknown".to_string(), |u| format!("{u:.0}% used"));
        let resets = window.resets_at.map_or(String::new(), |r| {
            format!(", resets {}", reset_text(r, now))
        });
        lines.push(Line::from(format!("  {:<6}{used}{resets}", window.label())));
    }
    if snapshot.is_stale(now) {
        let minutes = now.saturating_sub(snapshot.observed_at) / 60;
        lines.push(Line::from(Span::styled(
            format!("  stale: observed {minutes} minutes ago"),
            theme.dim(),
        )));
    }
    lines
}

/// `/usage`'s cost lines: the session's billed cost, the list-price estimate and, with a baseline,
/// what was avoided, then each model's.
pub fn cost_lines(costs: &Costs, ctx: &UsageContext, theme: &Theme) -> Vec<Line<'static>> {
    if costs.is_empty() {
        return Vec::new();
    }
    let total = costs.sum();
    let mut lines = vec![Line::from(Span::styled("Cost this session", theme.bold()))];
    lines.push(Line::from(format!(
        "  billed        {}   (requests paid for with an API key)",
        money(total.billed, total.billed_unknown)
    )));
    lines.push(Line::from(format!(
        "  list price    {}   (estimate: every hosted request at the table price)",
        money(total.list, total.list_unknown)
    )));
    if let Some(baseline) = &ctx.baseline {
        let figure = if total.avoided_requests == 0 {
            usd(0.0)
        } else {
            money(total.avoided, total.avoided_unknown)
        };
        lines.push(Line::from(format!(
            "  avoided       {figure}   vs {} (tokens on local or subscription models; plan and hardware fees not counted)",
            sanitize(baseline)
        )));
    }
    let width = costs
        .per_model
        .keys()
        .map(|m| m.chars().count())
        .max()
        .unwrap_or(0)
        .max(5);
    lines.push(Line::from(Span::styled(
        format!(
            "{:width$}  {:>22}  {}",
            "model", "billed", "list price (estimate)"
        ),
        theme.bold(),
    )));
    for (model, cost) in &costs.per_model {
        let billed = if cost.billed_requests == 0 {
            "not billed".to_string()
        } else {
            money(cost.billed, cost.billed_unknown)
        };
        lines.push(Line::from(format!(
            "{:width$}  {:>22}  {}",
            sanitize(model),
            billed,
            money(cost.list, cost.list_unknown)
        )));
    }
    lines
}

/// `/roles`: each role's model, and where it came from.
pub fn roles_report(roles: &[harness_core::role::RoleLine], theme: &Theme) -> Vec<Line<'static>> {
    use harness_core::role::RoleSource;
    let model_width = roles
        .iter()
        .map(|r| sanitize(&r.model).chars().count())
        .max()
        .unwrap_or(0);
    let mut lines = vec![Line::from(Span::styled("Roles", theme.bold()))];
    for line in roles {
        let source = match line.source {
            RoleSource::Inherited => "inherited from main".to_string(),
            other => other.as_str().to_string(),
        };
        lines.push(Line::from(vec![
            Span::styled(format!("  {:<10}", line.role.as_str()), theme.accent()),
            Span::raw(format!("  {:<model_width$}", sanitize(&line.model))),
            Span::styled(format!("  {source}"), theme.dim()),
        ]));
    }
    lines
}
