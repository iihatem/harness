//! Tool calls a model wrote as text instead of as native calls, which local models served
//! without a tool-call parser do. Only a whole message counts: one or more
//! `<tool_call>…</tool_call>` blocks with nothing but whitespace around them, or one JSON object
//! with `name` and `arguments` (or `parameters`, as Llama 3.1 writes it). A call must name a tool
//! the agent has; anything else, a quoted example included, stays text.

use serde_json::Value;

use crate::message::ToolCall;

const OPEN: &str = "<tool_call>";
const CLOSE: &str = "</tool_call>";

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
            calls.push(call(serde_json::from_str(inner[..end].trim()).ok()?)?);
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
