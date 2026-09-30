//! Compaction: when the conversation nears the context window, the older messages are replaced by
//! a summary the model writes, and the most recent ones are kept as they are.

use std::collections::HashMap;

use crate::{
    message::{ChatRequest, Message, ToolSpec},
    tokens,
};

/// Compact when estimated usage reaches this share of the context window.
pub const DEFAULT_THRESHOLD: f64 = 0.8;
/// Keep this share of the context window of recent messages as they are.
pub const DEFAULT_KEEP_RECENT: f64 = 0.2;
/// Starts the message that stands for the summarized part of the conversation.
pub const SUMMARY_PREFIX: &str = "[Summary of the earlier conversation]";
/// The system prompt of a summary request.
pub const SUMMARY_SYSTEM: &str = "You summarize a conversation between a user and a coding agent, so that the agent can continue the work from the summary alone.";

/// Characters of one message kept in the transcript sent to the summarizer.
const MAX_MESSAGE_CHARS: usize = 4_000;
/// Characters of one tool result kept in that transcript.
const MAX_TOOL_CHARS: usize = 2_000;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompactionConfig {
    /// Share of the context window at which the conversation is compacted.
    pub threshold: f64,
    /// Share of the context window kept as recent messages.
    pub keep_recent: f64,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        CompactionConfig {
            threshold: DEFAULT_THRESHOLD,
            keep_recent: DEFAULT_KEEP_RECENT,
        }
    }
}

/// Estimated tokens of one message, including its tool calls.
pub fn message_tokens(message: &Message) -> u64 {
    let body = match message {
        Message::User { content } | Message::Tool { content, .. } => tokens::estimate(content),
        Message::Assistant {
            content,
            tool_calls,
            ..
        } => {
            tokens::estimate(content)
                + tool_calls
                    .iter()
                    .map(|c| tokens::estimate(&c.name) + tokens::estimate(&c.arguments))
                    .sum::<u64>()
        }
    };
    // Roles and message framing.
    body + 4
}

/// Estimated tokens of a request: its system prompt, tool definitions and messages.
pub fn request_tokens(system: &str, tools: &[ToolSpec], messages: &[Message]) -> u64 {
    let tools = serde_json::to_string(tools).unwrap_or_default();
    tokens::estimate(system)
        + tokens::estimate(&tools)
        + messages.iter().map(message_tokens).sum::<u64>()
}

/// Where the kept part of `messages` starts: the earliest message, other than a tool result (it
/// must follow its call), from which the rest fits in `budget` tokens. `None` when even the last
/// message does not fit, or when everything fits and there is nothing before it to summarize.
pub fn cut(messages: &[Message], budget: u64) -> Option<usize> {
    let mut total = 0;
    let mut first_kept = None;
    for (i, message) in messages.iter().enumerate().rev() {
        total += message_tokens(message);
        if total > budget {
            break;
        }
        if !matches!(message, Message::Tool { .. }) {
            first_kept = Some(i);
        }
    }
    first_kept.filter(|&i| i > 0)
}

/// Whether summarizing `messages` would summarize anything: they are more than, at most, an
/// earlier summary.
pub fn summarizes(messages: &[Message]) -> bool {
    match messages {
        [] => false,
        [Message::User { content }] => !content.starts_with(SUMMARY_PREFIX),
        _ => true,
    }
}

/// The message that stands for the summarized part of the conversation.
pub fn summary_message(summary: &str) -> Message {
    Message::User {
        content: format!("{SUMMARY_PREFIX}\n{summary}"),
    }
}

/// The request asking `model` to summarize `messages`, with the user's `focus` if any. The
/// transcript is kept within `max_tokens` by leaving out the oldest messages.
pub fn summary_request(
    model: &str,
    messages: &[Message],
    focus: Option<&str>,
    max_tokens: u64,
) -> ChatRequest {
    let mut instructions = String::from(
        "Summarize the conversation below. Keep what the user asked for and why; decisions and constraints; the files read, created or changed, with their paths; commands run and what they showed; errors and how they were resolved; and what remains to be done, including any request in progress. Be concise and specific, and reply with the summary only.",
    );
    if let Some(focus) = focus.map(str::trim).filter(|f| !f.is_empty()) {
        instructions.push_str(&format!("\n\nFocus especially on: {focus}"));
    }
    let content = format!(
        "{instructions}\n\n<conversation>\n{}</conversation>",
        transcript(messages, max_tokens)
    );
    ChatRequest {
        model: model.to_string(),
        system: SUMMARY_SYSTEM.to_string(),
        messages: vec![Message::User { content }],
        tools: Vec::new(),
        ..ChatRequest::default()
    }
}

/// `messages` as plain text for the summarizer, each clipped, the oldest left out when the whole
/// would exceed `max_tokens`. An earlier summary at the start is always kept, and clipped only to
/// the whole budget: it ends with what remains to be done.
fn transcript(messages: &[Message], max_tokens: u64) -> String {
    let budget = max_tokens.saturating_mul(4) as usize;
    let keep_first = matches!(messages.first(), Some(Message::User { content }) if content.starts_with(SUMMARY_PREFIX));
    let mut tools: HashMap<&str, &str> = HashMap::new();
    let mut entries: Vec<String> = Vec::new();
    for message in messages {
        let entry = match message {
            Message::User { content } if keep_first && entries.is_empty() => {
                format!("User: {}", clip(content, budget))
            }
            Message::User { content } => format!("User: {}", clip(content, MAX_MESSAGE_CHARS)),
            Message::Assistant {
                content,
                tool_calls,
                ..
            } => {
                let mut text = String::new();
                if !content.is_empty() {
                    text.push_str(&format!("Assistant: {}", clip(content, MAX_MESSAGE_CHARS)));
                }
                for call in tool_calls {
                    tools.insert(&call.id, &call.name);
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(&format!(
                        "Assistant called {} with {}",
                        call.name,
                        clip(&call.arguments, MAX_TOOL_CHARS)
                    ));
                }
                text
            }
            Message::Tool {
                call_id,
                content,
                is_error,
            } => format!(
                "Result of {}{}: {}",
                tools.get(call_id.as_str()).copied().unwrap_or("a tool"),
                if *is_error { " (error)" } else { "" },
                clip(content, MAX_TOOL_CHARS)
            ),
        };
        entries.push(entry);
    }
    let mut dropped = 0;
    while entries.iter().map(|e| e.len() + 2).sum::<usize>() > budget
        && entries.len() > usize::from(keep_first) + 1
    {
        entries.remove(usize::from(keep_first));
        dropped += 1;
    }
    if dropped > 0 {
        entries.insert(
            usize::from(keep_first),
            format!("[{dropped} earlier messages left out]"),
        );
    }
    entries.iter().map(|e| format!("{e}\n\n")).collect()
}

/// `text` cut to about `max` characters, saying how much was left out.
fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let kept: String = text.chars().take(max).collect();
    format!("{kept} [… {} more characters]", text.chars().count() - max)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::ToolCall;

    fn user(text: &str) -> Message {
        Message::User {
            content: text.into(),
        }
    }

    #[test]
    fn the_kept_part_fits_the_budget_and_never_starts_with_a_tool_result() {
        let messages = vec![
            user(&"a".repeat(400)),
            Message::Assistant {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "read".into(),
                    arguments: "{}".into(),
                }],
                model: "m".into(),
            },
            Message::Tool {
                call_id: "c1".into(),
                content: "b".repeat(40),
                is_error: false,
            },
            user("next"),
        ];
        // The tool result and the last message fit, but the kept part cannot start with the
        // result, so it starts at the last message.
        let budget = message_tokens(&messages[2]) + message_tokens(&messages[3]);
        assert_eq!(cut(&messages, budget), Some(3));
        // With room for the call too, the call and its result stay together.
        assert_eq!(cut(&messages, budget + 20), Some(1));
        // Everything fits: nothing to summarize.
        assert_eq!(cut(&messages, 10_000), None);
        // Not even the last message fits.
        assert_eq!(cut(&messages, 1), None);
    }

    #[test]
    fn the_transcript_leaves_out_the_oldest_messages_first() {
        let messages: Vec<Message> = (0..20)
            .map(|i| user(&format!("message {i} {}", "x".repeat(100))))
            .collect();
        let text = transcript(&messages, 200);
        assert!(text.len() <= 900, "{}", text.len());
        assert!(text.contains("message 19") && !text.contains("message 0 "));
        assert!(text.contains("earlier messages left out"));
    }

    // Review F I2: a summary ends with what remains to be done, so clipping an earlier one
    // lost the most important part, more with each compaction.
    #[test]
    fn an_earlier_summary_is_kept_whole() {
        let summary = format!("{}TAIL-REMAINING-WORK", "s".repeat(6_000));
        let messages = vec![
            summary_message(&summary),
            user(&"x".repeat(5_000)),
            user("next"),
        ];
        let text = transcript(&messages, 16_000);
        assert!(text.contains("TAIL-REMAINING-WORK"));
        // Only the transcript's whole budget limits it.
        let text = transcript(&messages, 1_000);
        assert!(text.len() <= 4_000 + 200, "{}", text.len());
        assert!(
            text.starts_with(&format!("User: {SUMMARY_PREFIX}")),
            "{text:.100}"
        );
        assert!(text.contains("next"));
    }

    #[test]
    fn long_messages_are_clipped() {
        assert_eq!(clip("abcdef", 4), "abcd [… 2 more characters]");
        assert_eq!(clip("abc", 4), "abc");
    }
}
