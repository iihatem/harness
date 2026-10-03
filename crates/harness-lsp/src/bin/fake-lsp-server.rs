//! A language server for tests: it publishes diagnostics for what the text of a file says.
//!
//! In a file's text, a line holding `ERROR` is an error at that line, `WARN` a warning, `HINT` a
//! hint; `SLOW:<ms>` delays the publish; `NOPUBLISH` publishes nothing; `CRASH` exits at once;
//! `TWICE` publishes a syntax pass (no diagnostics) and then the real one 50 ms later.
//! `FAKE_LSP_LOG` names a file each received message is appended to, one line each: the method.
//! `FAKE_LSP_ARGS` is not read: arguments of the command are logged first, as `args: ...`.

use std::{
    io::{BufRead, Write},
    time::Duration,
};

use serde_json::{Value, json};

fn read_message(input: &mut impl BufRead) -> Option<Value> {
    let mut length = 0usize;
    loop {
        let mut line = String::new();
        if input.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(value) = line.strip_prefix("Content-Length:") {
            length = value.trim().parse().ok()?;
        }
    }
    let mut body = vec![0; length];
    input.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

fn send(out: &mut impl Write, message: &Value) {
    let body = message.to_string();
    write!(out, "Content-Length: {}\r\n\r\n{body}", body.len()).unwrap();
    out.flush().unwrap();
}

fn log(line: &str) {
    if let Ok(path) = std::env::var("FAKE_LSP_LOG") {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        writeln!(file, "{line}").unwrap();
    }
}

fn diagnostics(text: &str) -> Vec<Value> {
    text.lines()
        .enumerate()
        .filter_map(|(line, text)| {
            let severity = if text.contains("ERROR") {
                1
            } else if text.contains("WARN") {
                2
            } else if text.contains("HINT") {
                4
            } else {
                return None;
            };
            Some(json!({
                "range": {"start": {"line": line, "character": 0}, "end": {"line": line, "character": 1}},
                "severity": severity,
                "message": format!("problem on line {}: {}", line + 1, text.trim()),
            }))
        })
        .collect()
}

fn publish(out: &mut impl Write, uri: &Value, text: &str) {
    if text.contains("NOPUBLISH") {
        return;
    }
    if let Some(ms) = text
        .split("SLOW:")
        .nth(1)
        .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|digits| digits.parse::<u64>().ok())
    {
        std::thread::sleep(Duration::from_millis(ms));
    }
    if text.contains("CRASH") {
        std::process::exit(1);
    }
    if text.contains("TWICE") {
        send(
            out,
            &json!({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": {"uri": uri, "diagnostics": []}}),
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    send(
        out,
        &json!({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": {"uri": uri, "diagnostics": diagnostics(text)}}),
    );
}

fn main() {
    log(&format!(
        "args: {}",
        std::env::args().skip(1).collect::<Vec<_>>().join(" ")
    ));
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut out = std::io::stdout().lock();
    while let Some(message) = read_message(&mut input) {
        let method = message["method"].as_str().unwrap_or_default().to_string();
        log(if method.is_empty() { "reply" } else { &method });
        match method.as_str() {
            "initialize" => {
                // Requests of the server's own, which the client must answer without being asked.
                send(
                    &mut out,
                    &json!({"jsonrpc": "2.0", "id": 900, "method": "window/workDoneProgress/create", "params": {"token": "t"}}),
                );
                send(
                    &mut out,
                    &json!({"jsonrpc": "2.0", "id": message["id"], "result": {"capabilities": {"textDocumentSync": 1}}}),
                );
            }
            "shutdown" => send(
                &mut out,
                &json!({"jsonrpc": "2.0", "id": message["id"], "result": null}),
            ),
            "exit" => return,
            "textDocument/didOpen" => {
                let item = &message["params"]["textDocument"];
                publish(
                    &mut out,
                    &item["uri"],
                    item["text"].as_str().unwrap_or_default(),
                );
            }
            "textDocument/didChange" => {
                let params = &message["params"];
                let text = params["contentChanges"][0]["text"]
                    .as_str()
                    .unwrap_or_default();
                publish(&mut out, &params["textDocument"]["uri"], text);
            }
            _ => {}
        }
    }
}
