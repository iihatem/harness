//! Tool calls a model wrote as text instead of as native calls, which local models served
//! without a tool-call parser do. Only a whole message counts: one or more
//! `<tool_call>…</tool_call>` blocks with nothing but whitespace around them, or one JSON object
//! with `name` and `arguments` (or `parameters`, as Llama 3.1 writes it). A block holds a JSON
//! object of that shape (Hermes' form), or one function in Qwen3-Coder's form:
//!
//! ```text
//! <tool_call>
//! <function=read>
//! <parameter=path>
//! src/lib.rs
//! </parameter>
//! </function>
//! </tool_call>
//! ```
//!
//! A call must name a tool the agent has; anything else, a quoted example included, stays text.

use serde_json::{Map, Value};

use crate::message::ToolCall;

const OPEN: &str = "<tool_call>";
const CLOSE: &str = "</tool_call>";
const FUNCTION: &str = "<function=";
const FUNCTION_END: &str = "</function>";
const PARAMETER: &str = "<parameter=";
const PARAMETER_END: &str = "</parameter>";

/// The calls `text` consists of, when it is nothing but calls to tools `known` names. Their ids
/// are empty; the agent gives them fresh ones.
pub fn recover(text: &str, known: impl Fn(&str) -> bool) -> Option<Vec<ToolCall>> {
    let text = text.trim();
    let calls = if text.starts_with(OPEN) {
        let mut calls = Vec::new();
        let mut rest = text;
        while !rest.is_empty() {
            let inner = rest.strip_prefix(OPEN)?;
            let end = inner.find(CLOSE)?;
            let block = inner[..end].trim();
            calls.push(if block.starts_with(FUNCTION) {
                function_call(block)?
            } else {
                call(serde_json::from_str(block).ok()?)?
            });
            rest = inner[end + CLOSE.len()..].trim_start();
        }
        calls
    } else if text.starts_with('{') {
        vec![call(serde_json::from_str(text).ok()?)?]
    } else {
        return None;
    };
    calls.iter().all(|c| known(&c.name)).then_some(calls)
}

/// A call from `{"name": …, "arguments": …}`. Arguments that are an object become JSON text;
/// arguments already given as JSON text are kept as they are.
fn call(value: Value) -> Option<ToolCall> {
    let name = value["name"].as_str().filter(|n| !n.is_empty())?;
    let arguments = match value.get("arguments").or_else(|| value.get("parameters"))? {
        Value::String(text) => text.clone(),
        object @ Value::Object(_) => object.to_string(),
        _ => return None,
    };
    Some(ToolCall {
        id: String::new(),
        name: name.to_string(),
        arguments,
    })
}

/// A call in Qwen3-Coder's form: `<function=NAME>`, then `<parameter=ARG>value</parameter>` for
/// each argument, then `</function>`, with nothing but whitespace between them.
fn function_call(block: &str) -> Option<ToolCall> {
    let rest = block.strip_prefix(FUNCTION)?;
    let close = rest.find('>')?;
    let name = rest[..close].trim();
    let body = rest[close + 1..].strip_suffix(FUNCTION_END)?;
    if name.is_empty() {
        return None;
    }
    let mut arguments = Map::new();
    let mut rest = body.trim_start();
    while !rest.is_empty() {
        let parameter = rest.strip_prefix(PARAMETER)?;
        let close = parameter.find('>')?;
        let key = parameter[..close].trim();
        let after = &parameter[close + 1..];
        let end = after.find(PARAMETER_END)?;
        if key.is_empty() {
            return None;
        }
        arguments.insert(key.to_string(), parameter_value(&after[..end]));
        rest = after[end + PARAMETER_END.len()..].trim_start();
    }
    Some(ToolCall {
        id: String::new(),
        name: name.to_string(),
        arguments: Value::Object(arguments).to_string(),
    })
}

/// A parameter's value: JSON when it parses as JSON, a string otherwise. The chat template writes
/// a newline on each side of it, which is not part of it.
fn parameter_value(raw: &str) -> Value {
    let raw = raw.strip_prefix('\n').unwrap_or(raw);
    let raw = raw.strip_suffix('\n').unwrap_or(raw);
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

/// The keys a call object can start with.
const CALL_KEYS: [&str; 3] = ["name", "arguments", "parameters"];

/// Watches a reply as it streams, to tell whether it can still become a message that [`recover`]
/// accepts, so that such text is held back rather than shown, while any other text is shown as it
/// comes. It errs on the side of showing: a call object whose first key is not one of a call's
/// is shown, and still recovered at the end.
#[derive(Debug, Default)]
pub struct CallWatch {
    /// Once the text cannot become calls, it never can.
    ruled_out: bool,
    /// How far the text has been scanned for the end of a JSON object.
    scanned: usize,
    depth: u32,
    in_string: bool,
    escaped: bool,
    /// Where the object ends, once it has.
    end: Option<usize>,
}

impl CallWatch {
    /// Whether `text`, the reply so far, can still become calls. Each call passes the text of the
    /// one before with more appended.
    pub fn may_be_calls(&mut self, text: &str) -> bool {
        if !self.ruled_out && !self.check(text) {
            self.ruled_out = true;
        }
        !self.ruled_out
    }

    fn check(&mut self, text: &str) -> bool {
        let trimmed = text.trim_start();
        if trimmed.is_empty() {
            return true;
        }
        if trimmed.starts_with('{') {
            return self.object(text, text.len() - trimmed.len());
        }
        // One `<tool_call>` block after another, with nothing but whitespace between.
        let mut rest = trimmed;
        loop {
            if rest.is_empty() || OPEN.starts_with(rest) {
                return true;
            }
            let Some(inner) = rest.strip_prefix(OPEN) else {
                return false;
            };
            let Some(end) = inner.find(CLOSE) else {
                return true;
            };
            rest = inner[end + CLOSE.len()..].trim_start();
        }
    }

    /// For a JSON object starting at `start`: its first key can be a call's, and nothing but
    /// whitespace follows its end.
    fn object(&mut self, text: &str, start: usize) -> bool {
        let body = text[start + 1..].trim_start();
        if !body.is_empty() {
            let Some(key) = body.strip_prefix('"') else {
                return false;
            };
            let key_fits = match key.find('"') {
                Some(end) => CALL_KEYS.contains(&&key[..end]),
                None => CALL_KEYS.iter().any(|k| k.starts_with(key)),
            };
            if !key_fits {
                return false;
            }
        }
        if self.end.is_none() {
            self.scan(text, start);
        }
        self.end.is_none_or(|end| text[end..].trim().is_empty())
    }

    /// Scans what was added since the last call for the end of the object at `start`.
    fn scan(&mut self, text: &str, start: usize) {
        let from = self.scanned.max(start);
        for (i, c) in text[from..].char_indices() {
            if self.in_string {
                match c {
                    _ if self.escaped => self.escaped = false,
                    '\\' => self.escaped = true,
                    '"' => self.in_string = false,
                    _ => {}
                }
                continue;
            }
            match c {
                '"' => self.in_string = true,
                '{' | '[' => self.depth += 1,
                '}' | ']' => {
                    self.depth = self.depth.saturating_sub(1);
                    if self.depth == 0 {
                        self.end = Some(from + i + 1);
                        return;
                    }
                }
                _ => {}
            }
        }
        self.scanned = text.len();
    }
}
