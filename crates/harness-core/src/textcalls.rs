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
