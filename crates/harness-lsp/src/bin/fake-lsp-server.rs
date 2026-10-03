//! A language server for tests: it publishes diagnostics for what the text of a file says.
//!
//! In a file's text, a line holding `ERROR` is an error at that line, `WARN` a warning, `HINT` a
//! hint; `SLOW:<ms>` delays the publish; `NOPUBLISH` publishes nothing; `CRASH` exits at once;
//! `TWICE` publishes a syntax pass (no diagnostics) and then the real one 5 ms later, well inside the client's 150 ms settle even on a loaded machine.
//! Like rust-analyzer, `RALIKE` opens a progress token, publishes an empty set at once, and the
//! real one 1.2 s later, and closes the token (the token is opened only for a client that said it
//! takes work-done progress). `FLYCHECK` is rust-analyzer's order at its start: idle, and only
//! 300 ms later the progress of `cargo check`, which publishes the real set. `STATUS` does the same through `experimental/serverStatus`, which
//! it sends only to a client that asked for it; `STUCK` opens a progress token that never closes
//! and publishes at once. `STALE` publishes under the previous version of the file, `NOVERSION`
//! publishes with no version, `NOREPEAT` publishes only when the set differs from the last one
//! published for the file, and `LOWERHEX` writes the percent-encoding of the URI in lower case.
//! `FAKE_LSP_CHILD` names a file the pid of a `sleep 300` child, started at `initialize`, is
//! written to; `FAKE_LSP_HANG_INIT` makes the server never answer `initialize`.
//! Each received message is appended to a log, one line each: the method (`reply` for a reply to
//! a request of the server's). Before them come `program: <name>` and `args: <arguments>`. The log
//! is `FAKE_LSP_LOG`, or else `server.log` beside the directory the server was run from.

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

/// Where to log: `FAKE_LSP_LOG`, else `server.log` beside the directory the server was started
/// from (`<dir>/bin/<name>`), so each test's servers log to a file of its own.
fn log_path() -> Option<std::path::PathBuf> {
    if let Ok(path) = std::env::var("FAKE_LSP_LOG") {
        return Some(path.into());
    }
    let program = std::path::PathBuf::from(std::env::args().next()?);
    Some(program.parent()?.parent()?.join("server.log"))
}

fn log(line: &str) {
    if let Some(path) = log_path() {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        // One write, so lines of servers sharing the log never interleave.
        file.write_all(format!("{line}\n").as_bytes()).unwrap();
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

/// Like typescript-language-server, the fake publishes nothing unless the client said, when it
/// initialized, that it takes `textDocument/publishDiagnostics`.
static CLIENT_TAKES_DIAGNOSTICS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
static CLIENT_TAKES_PROGRESS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
static CLIENT_TAKES_STATUS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn notification(method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": method, "params": params})
}

fn progress(kind: &str) -> Value {
    notification(
        "$/progress",
        json!({"token": "ra", "value": {"kind": kind, "title": "Indexing"}}),
    )
}

fn status(quiescent: bool) -> Value {
    notification(
        "experimental/serverStatus",
        json!({"health": "ok", "quiescent": quiescent}),
    )
}

fn diagnostics_message(uri: &Value, version: Option<i64>, text: &str) -> Value {
    let mut uri = uri.clone();
    if text.contains("LOWERHEX") {
        uri = Value::String(lower_hex(uri.as_str().unwrap_or_default()));
    }
    let mut params = json!({"uri": uri, "diagnostics": diagnostics(text)});
    if let Some(version) = version {
        params["version"] = json!(version);
    }
    notification("textDocument/publishDiagnostics", params)
}

fn lower_hex(uri: &str) -> String {
    let bytes = uri.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            out.push('%');
            out.push_str(&uri[i + 1..i + 3].to_ascii_lowercase());
            i += 3;
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

/// The sets last published, by URI, for `NOREPEAT`.
static LAST: std::sync::Mutex<Vec<(String, String)>> = std::sync::Mutex::new(Vec::new());

fn publish(out: &mut impl Write, uri: &Value, text: &str, version: i64) {
    if text.contains("NOPUBLISH")
        || !CLIENT_TAKES_DIAGNOSTICS.load(std::sync::atomic::Ordering::SeqCst)
    {
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
    let version = (!text.contains("NOVERSION")).then_some(version);
    if text.contains("NOREPEAT") {
        let set = serde_json::to_string(&diagnostics(text)).unwrap();
        let mut last = LAST.lock().unwrap();
        let key = uri.as_str().unwrap_or_default().to_string();
        if last.iter().any(|(k, v)| *k == key && *v == set) {
            return;
        }
        last.retain(|(k, _)| *k != key);
        last.push((key, set));
    }
    if text.contains("STALE") {
        send(out, &diagnostics_message(uri, version.map(|v| v - 1), text));
        return;
    }
    let taking_progress = CLIENT_TAKES_PROGRESS.load(std::sync::atomic::Ordering::SeqCst);
    let taking_status = CLIENT_TAKES_STATUS.load(std::sync::atomic::Ordering::SeqCst);
    if text.contains("RALIKE") && taking_progress {
        send(out, &progress("begin"));
        send(out, &diagnostics_message(uri, version, ""));
        std::thread::sleep(Duration::from_millis(1200));
        send(out, &diagnostics_message(uri, version, text));
        send(out, &progress("end"));
        return;
    }
    if text.contains("STATUS") && taking_status {
        send(out, &status(false));
        send(out, &diagnostics_message(uri, version, ""));
        std::thread::sleep(Duration::from_millis(1200));
        send(out, &diagnostics_message(uri, version, text));
        send(out, &status(true));
        return;
    }
    if text.contains("FLYCHECK") && taking_status && taking_progress {
        // rust-analyzer's real order: idle first, and only then `cargo check` begins.
        send(out, &status(false));
        send(out, &diagnostics_message(uri, version, ""));
        std::thread::sleep(Duration::from_millis(600));
        send(out, &status(true));
        std::thread::sleep(Duration::from_millis(300));
        send(out, &progress("begin"));
        std::thread::sleep(Duration::from_millis(100));
        send(out, &diagnostics_message(uri, version, text));
        send(out, &progress("end"));
        return;
    }
    if text.contains("STUCK") && taking_progress {
        send(out, &progress("begin"));
        send(out, &diagnostics_message(uri, version, text));
        return;
    }
    if text.contains("TWICE") {
        send(out, &diagnostics_message(uri, version, ""));
        std::thread::sleep(Duration::from_millis(5));
    }
    send(out, &diagnostics_message(uri, version, text));
}

fn main() {
    let mut args = std::env::args();
    let program = args.next().unwrap_or_default();
    log(&format!(
        "program: {}",
        program.rsplit('/').next().unwrap_or_default()
    ));
    log(&format!("args: {}", args.collect::<Vec<_>>().join(" ")));
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut out = std::io::stdout().lock();
    while let Some(message) = read_message(&mut input) {
        let method = message["method"].as_str().unwrap_or_default().to_string();
        log(if method.is_empty() { "reply" } else { &method });
        match method.as_str() {
            "initialize" => {
                let capabilities = &message["params"]["capabilities"];
                CLIENT_TAKES_DIAGNOSTICS.store(
                    capabilities["textDocument"]["publishDiagnostics"].is_object(),
                    std::sync::atomic::Ordering::SeqCst,
                );
                CLIENT_TAKES_PROGRESS.store(
                    capabilities["window"]["workDoneProgress"] == json!(true),
                    std::sync::atomic::Ordering::SeqCst,
                );
                CLIENT_TAKES_STATUS.store(
                    capabilities["experimental"]["serverStatusNotification"] == json!(true),
                    std::sync::atomic::Ordering::SeqCst,
                );
                if let Ok(path) = std::env::var("FAKE_LSP_CHILD") {
                    // Left running on purpose: a test checks that the client ends it.
                    #[allow(clippy::zombie_processes)]
                    let child = std::process::Command::new("sleep")
                        .arg("300")
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .spawn()
                        .unwrap();
                    std::fs::write(path, child.id().to_string()).unwrap();
                }
                if std::env::var_os("FAKE_LSP_HANG_INIT").is_some() {
                    continue;
                }
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
                    item["version"].as_i64().unwrap_or(0),
                );
            }
            "textDocument/didChange" => {
                let params = &message["params"];
                let text = params["contentChanges"][0]["text"]
                    .as_str()
                    .unwrap_or_default();
                publish(
                    &mut out,
                    &params["textDocument"]["uri"],
                    text,
                    params["textDocument"]["version"].as_i64().unwrap_or(0),
                );
            }
            _ => {}
        }
    }
}
